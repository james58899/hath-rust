use std::{
    collections::HashMap,
    fmt::Display,
    fs::Metadata,
    io::Error,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, SystemTime},
};

use async_stream::stream;
use aws_lc_rs::digest;
use bytes::Bytes;
use filesize::{file_real_size, file_real_size_fast};
use filetime::{FileTime, set_file_mtime};
use futures::{Stream, StreamExt, TryFutureExt, TryStreamExt, stream};
use hex::FromHex;
use log::{debug, error, info, warn};
use mime::Mime;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tempfile::TempPath;
use tokio::{
    fs::{DirEntry, File, copy, create_dir_all, metadata, read_dir, remove_dir_all, remove_file, rename},
    io::{AsyncReadExt, AsyncWriteExt},
    runtime::{Builder, Handle, Runtime},
    spawn,
    sync::mpsc::{UnboundedSender, channel},
    task::{block_in_place, spawn_blocking},
    time::{Instant, sleep_until},
};
use tokio_stream::wrappers::ReadDirStream;

use crate::{
    file_reader::FileReader,
    rpc::{InitSettings, Settings},
};

const SIZE_100MB: u64 = 100 * 1024 * 1024;

pub struct CacheManager {
    cache_dir: PathBuf,
    cache_state: Mutex<HashMap<String, CacheState>>,
    cache_state_path: PathBuf,
    temp_dir: PathBuf,
    size_limit: AtomicU64,
    io_pool: Option<Runtime>,
}

#[derive(Serialize, Deserialize)]
struct CacheState {
    file_count: u64,
    size: u64,
    oldest: i64,
}

impl CacheState {
    fn add_file(&mut self, size: u64) {
        self.file_count += 1;
        self.size += size;
    }

    fn remove_file(&mut self, size: u64) {
        self.file_count -= 1;
        self.size -= size;
    }
}

impl Default for CacheState {
    fn default() -> Self {
        Self {
            file_count: 0,
            size: 0,
            oldest: FileTime::now().unix_seconds(),
        }
    }
}

impl Drop for CacheManager {
    fn drop(&mut self) {
        // IO Pool need shutdown in blocking context
        if let Some(io_pool) = self.io_pool.take() {
            block_in_place(|| io_pool.shutdown_background());
        }
    }
}

impl CacheManager {
    pub async fn new<P: AsRef<Path>>(
        data_dir: P,
        cache_dir: P,
        temp_dir: P,
        settings: Arc<Settings>,
        init_settings: &InitSettings,
        force_background_scan: bool,
        shutdown: UnboundedSender<()>,
    ) -> Result<Arc<Self>, Error> {
        let io_pool = Builder::new_multi_thread().thread_name("IO-Worker").worker_threads(20).build()?;
        let new = Arc::new(Self {
            cache_dir: cache_dir.as_ref().to_path_buf(),
            cache_state: Default::default(),
            cache_state_path: data_dir.as_ref().join("cache_state.dat"),
            temp_dir: temp_dir.as_ref().to_path_buf(),
            size_limit: AtomicU64::new(u64::MAX),
            io_pool: Some(io_pool),
        });
        new.update_settings(settings);

        delete_java_cache_data(data_dir).await; // Force official version rescan cache
        clean_temp_dir(temp_dir.as_ref()).await;

        let manager = new.clone();
        let verify_cache = init_settings.verify_cache();
        let static_range = init_settings.static_range();
        let free = get_available_space(cache_dir.as_ref());
        let low_disk = free.is_some_and(|x| x < SIZE_100MB);

        // Load cache state
        if !verify_cache {
            let old_state = match read_state(&new.cache_state_path).await {
                Ok(state) => state,
                Err(err) => {
                    error!("Load cache state error: err={}", err);
                    HashMap::new()
                }
            };
            for (sr, state) in old_state {
                if static_range.contains(&sr) {
                    new.cache_state.lock().insert(sr, state);
                }
            }
        }

        if low_disk || (verify_cache && !force_background_scan) {
            // Low space or force cache check
            if low_disk {
                warn!("Disk space is low than 100MiB: available={}MiB", free.unwrap() / 1024 / 1024);
            }

            if verify_cache {
                info!("Start force cache check");
            } else {
                info!("Start foreground cache scan due to low disk space");
            }
            new.scan_cache(static_range, 16, verify_cache).await?;
            CacheManager::start_background_task(manager);
        } else {
            // Background cache scan
            if verify_cache {
                info!("Start background force cache check");
            } else {
                info!("Start background cache scan");
            }
            spawn(async move {
                if let Err(err) = manager.scan_cache(static_range, 4, verify_cache).await {
                    error!("Cache scan error: {}", err);
                    let _ = shutdown.send(());
                }
                CacheManager::start_background_task(manager);
            });
        }

        Ok(new)
    }

    pub async fn create_temp_file(&self) -> TempPath {
        let temp_dir = self.temp_dir.clone();
        spawn_blocking(|| {
            tempfile::Builder::new()
                .prefix("proxyfile_")
                .tempfile_in(temp_dir)
                .unwrap()
                .into_temp_path()
        })
        .await
        .unwrap()
    }

    pub async fn get_file(self: &Arc<Self>, info: &CacheFileInfo) -> Option<impl Stream<Item = Result<Bytes, Error>> + use<>> {
        let path = info.to_path(&self.cache_dir);

        // Check exists and open file
        let io_pool = self.get_io_pool();
        let path2 = path.clone();
        let metadata = io_pool.spawn(async move { std::fs::metadata(&path2) }).await.ok()?.ok()?;
        if !metadata.is_file() || metadata.len() != info.size() as u64 {
            warn!("Unexcepted cache file metadata: type={:?}, size={}", metadata.file_type(), metadata.len());
            return None;
        }
        let path2 = path.clone();
        let mut file_reader = match io_pool.spawn(async move { std::fs::File::open(&path2) }).await.ok()? {
            Ok(file) => FileReader::new(io_pool.clone(), file, 64 * 1024), // 64KiB buffer
            Err(err) => {
                error!("Open cache file error: path={:?}, err={}", &path, err);
                return None;
            }
        };

        // Skip hash check if file is recently accessed
        let one_week_ago = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 7);
        if FileTime::from_last_modification_time(&metadata) >= one_week_ago.into() {
            return Some(file_reader.boxed());
        }

        self.mark_recently_accessed(info).await;
        let cache_manager = self.clone();
        let info = info.clone();
        let (tx, mut rx) = channel::<Result<Bytes, Error>>(1);
        tokio::spawn(async move {
            let file_size = metadata.len();
            let mut read_off = 0;
            let mut hasher = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);

            while read_off < file_size {
                let buffer = match file_reader.read().await {
                    Ok(buf) => buf,
                    Err(err) => {
                        error!("Read cache file error: path={:?}, err={}", &path, err);
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                };
                read_off += buffer.len() as u64;
                hasher.update(&buffer);
                let _ = tx.send(Ok(buffer)).await;
            }

            let hash = hasher.finish();
            if hash.as_ref() != info.hash() {
                warn!("Detected corrupt cache file: path={:?}, hash={:x?}, actual={:x?}", &path, info.hash(), hash);
                cache_manager.remove_cache(&info).await;
            }
        });

        Some(
            stream! {
                while let Some(item) = rx.recv().await {
                    yield item;
                }
            }
            .boxed(),
        )
    }

    pub async fn import_cache(&self, info: &CacheFileInfo, file_path: &TempPath) {
        let path = info.to_path(&self.cache_dir);
        let dir = path.parent().unwrap();

        if metadata(dir).await.is_err() {
            if let Err(err) = create_dir_all(dir).await {
                error!("Create cache directory fail: {}", err);
                return;
            }
        }

        // Try remove existing file
        self.remove_cache(info).await;

        // Importing
        if rename(&file_path, &path).await.is_err() {
            // Can't cross fs move file, try copy.
            if let Err(err) = copy(file_path, &path).await {
                error!("Import cache failed: {}", err);
                return;
            }
        }

        // Fix permission
        fix_permission(&path).await;

        if let Some(size) = async_filesize(&path).await {
            self.cache_state.lock().entry(info.static_range()).or_default().add_file(size);
        }
    }

    pub async fn remove_cache(&self, info: &CacheFileInfo) -> Option<()> {
        let path = info.to_path(&self.cache_dir);
        let metadata = metadata(&path).await.ok()?;
        let size = async_filesize_fast(&path, &metadata).await?;

        debug!("Delete cache: {:?}", path);
        if let Err(err) = std::fs::remove_file(&path) {
            error!("Delete cache file error: path={:?}, err={}", &path, err);
            return None;
        }

        if let Some(state) = self.cache_state.lock().get_mut(&info.static_range()) {
            state.remove_file(size);
        }

        Some(())
    }

    pub fn update_settings(&self, settings: Arc<Settings>) {
        let size_limit = settings.size_limit();
        info!("Set size limit to {:} GiB", size_limit / 1024 / 1024 / 1024);
        self.size_limit.store(size_limit, Relaxed);
    }

    pub async fn save_state(&self) {
        let mut file = match File::create(&self.cache_state_path).await {
            Ok(file) => file,
            Err(err) => {
                error!("Create cache state file error: err={}", err);
                return;
            }
        };
        let buf = match rmp_serde::to_vec_named(&*self.cache_state.lock()) {
            Ok(buf) => buf,
            Err(err) => {
                error!("Serialize cache state error: err={}", err);
                return;
            }
        };
        if let Err(err) = file.write_all(&buf).await {
            error!("Write cache state error: err={}", err);
        }
    }

    fn get_io_pool(&self) -> Handle {
        self.io_pool.as_ref().map_or_else(|| Handle::current(), |rt| rt.handle().clone())
    }

    fn start_background_task(new: Arc<Self>) {
        let manager = Arc::downgrade(&new);
        spawn(async move {
            // Check cache size every 10min
            let mut next_run = Instant::now();
            loop {
                sleep_until(next_run).await;
                if let Some(manager) = manager.upgrade() {
                    manager.save_state().await;
                    manager.check_cache_usage().await;
                    next_run = Instant::now() + Duration::from_secs(600);
                } else {
                    break;
                }
            }
        });
    }

    async fn mark_recently_accessed(&self, info: &CacheFileInfo) {
        let path = info.to_path(&self.cache_dir);
        let _ = spawn_blocking(move || {
            if let Err(err) = set_file_mtime(&path, FileTime::now()) {
                error!("Update cache file time error: path={:?}, err={}", &path, err);
            }
        })
        .await;
    }

    async fn scan_cache(&self, static_range: Vec<String>, parallelism: usize, verify_cache: bool) -> Result<(), Error> {
        let mut dirs = Vec::with_capacity(static_range.len());

        let mut root = read_dir(&self.cache_dir).await?;
        while let Some(l1) = root.next_entry().await? {
            let l1_path = l1.path();
            if !l1_path.is_dir() {
                warn!("Found unexpected file in cache dir: {}", l1_path.to_str().unwrap_or_default());
                continue;
            };

            let mut l1_stream = read_dir(&l1_path).await?;
            while let Some(l2) = l1_stream.next_entry().await? {
                let l2_path = l2.path();
                if !l2_path.is_dir() {
                    warn!("Found unexpected file in cache dir: {}", l2_path.to_str().unwrap_or_default());
                    continue;
                };

                let mut hash = l1.file_name().clone();
                hash.push(l2.file_name());
                if let Some(sr) = static_range.iter().find(|sr| hash.eq_ignore_ascii_case(sr)) {
                    dirs.push((sr, l2_path));
                } else {
                    warn!("Delete not in static range dir: {}", l2_path.to_str().unwrap_or_default());
                    if let Err(err) = remove_dir_all(&l2_path).await {
                        error!("Delete cache dir error: path={:?}, err={}", &l2_path, err);
                    }
                }
            }
        }

        debug!("Cache dir number: {}", &dirs.len());

        let mut counter = 0;
        let total = dirs.len();
        for (sr, path) in dirs.into_iter() {
            // Check old state match
            let file_count = match open_dir(&path).await {
                Some(stream) => stream.filter_map(|e| async { e.ok() }).count().await as u64,
                None => continue,
            };
            if file_count == self.cache_state.lock().get(sr).map(|s| s.file_count).unwrap_or(0) {
                counter += 1;
                if counter % 100 == 0 || counter == total {
                    info!("Scanned {}/{} static ranges.", counter, total);
                }
                continue;
            }

            // State mismatch, scan dir
            self.cache_state.lock().remove(sr); // Clear old state

            let files = match open_dir(&path).await {
                Some(mut stream) => list_files(&mut stream).await,
                None => continue,
            };

            stream::iter(files)
                .for_each_concurrent(parallelism, |entry| async move {
                    let path = entry.path();
                    let metadata = entry.metadata().await;
                    if let Err(err) = metadata {
                        error!("Read cache file metadata error: path={:?}, err={}", path, err);
                        return;
                    }
                    let metadata = metadata.unwrap();
                    if metadata.is_dir() {
                        warn!("Found unexpected dir in cache dir: {:?}", path);
                        return;
                    }

                    // Parse info
                    let info = entry.file_name().to_str().and_then(CacheFileInfo::from_file_id);
                    if info.is_none() {
                        warn!("Invalid cache file: {:?}", path);
                        return;
                    }
                    let info = info.unwrap();

                    let size = metadata.len();
                    if size != info.size() as u64 {
                        warn!("Delete corrupt cache file: path={:?}, size={:x?}, actual={:x?}", &path, &info.size(), size);

                        if let Err(err) = remove_file(&path).await {
                            error!("Delete corrupt cache file error: path={:?}, err={}", &path, err);
                        }
                        return;
                    }

                    if verify_cache {
                        let mut file = match File::open(&path).await {
                            Ok(file) => file,
                            Err(err) => {
                                error!("Open cache file {:?} error: {}", path, err);
                                return;
                            }
                        };
                        let mut hasher = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);
                        let mut buf = vec![0; 1024 * 1024]; // 1MiB
                        loop {
                            match file.read(&mut buf).await {
                                Ok(n) => {
                                    if n == 0 {
                                        break;
                                    }
                                    hasher.update(&buf[0..n]);
                                }
                                Err(e) => {
                                    error!("Read cache file {:?} error: {}", path, e);
                                    break;
                                }
                            }
                        }
                        let actual_hash = hasher.finish();
                        if actual_hash.as_ref() != info.hash {
                            warn!("Delete corrupt cache file: path={:?}, hash={:x?}, actual={:x?}", path, &info.hash, &actual_hash);
                            if let Err(err) = remove_file(&path).await {
                                error!("Delete corrupt cache file error: path={:?}, err={}", path, err);
                            }
                            return;
                        }
                    }

                    // Update cache state
                    if let Some(size) = async_filesize_fast(&path, &metadata).await {
                        let mtime = FileTime::from_last_modification_time(&metadata).unix_seconds();
                        let mut cache_state = self.cache_state.lock();
                        let state = cache_state.entry(info.static_range()).or_default();
                        state.add_file(size);
                        if state.oldest > mtime {
                            state.oldest = mtime;
                        }
                    }
                })
                .await;

            // Scan progress
            counter += 1;
            if counter % 100 == 0 || counter == total {
                info!("Scanned {}/{} static ranges.", counter, total);
            }
        }

        if counter == 0 && static_range.len() > 20 {
            error!(
                "This client has static ranges assigned to it, but the cache is empty. Check file permissions and file system integrity."
            );
            return Err(Error::new(std::io::ErrorKind::NotFound, "Cache is empty."));
        }

        let (total_size, file_count) = self
            .cache_state
            .lock()
            .values()
            .fold((0, 0), |(size, count), state| (size + state.size, count + state.file_count));
        info!("Finished cache scan. Cache size: {}, file count: {}", total_size, file_count);

        Ok(())
    }

    async fn check_cache_usage(&self) {
        let total_size = self.cache_state.lock().values().fold(0, |acc, state| acc + state.size);
        let size_limit = self.size_limit.load(Relaxed);
        let disk_free = get_available_space(&self.cache_dir);
        let mut need_free = total_size.saturating_sub(size_limit);

        if let Some(free) = disk_free {
            if free < SIZE_100MB {
                warn!("Disk space is low than 100MiB: available={}MiB", free / 1024 / 1024);
                need_free += SIZE_100MB.saturating_sub(free);
            }
        }

        debug!("Cache usage: totel={total_size}bytes, limit={size_limit}bytes, available={disk_free:?}bytes");

        if need_free == 0 {
            return;
        }

        debug!("Start cache cleaner: need_free={}bytes", need_free);
        while need_free > 0 {
            // Select the oldest directory
            let static_range;
            let target_dir;
            let cut_off;
            {
                let map = self.cache_state.lock();
                let mut state = map.iter().collect::<Vec<_>>();
                if state.is_empty() {
                    return;
                }
                state.sort_unstable_by(|(_, a), (_, b)| a.oldest.cmp(&b.oldest));
                let (sr, state) = state[0];
                static_range = sr.clone();
                target_dir = self.cache_dir.join(&sr[0..2]).join(&sr[2..4]);
                // 1 day
                cut_off = FileTime::from_unix_time(state.oldest + 86400, 0);
            }

            // List files
            let files = match open_dir(&target_dir).await {
                Some(mut stream) => list_files(&mut stream).await,
                None => break,
            };

            // Sort by mtime
            let mut files: Vec<(DirEntry, FileTime, Metadata)> = stream::iter(files)
                .filter_map(|entry| async move {
                    let metadata = match entry.metadata().await {
                        Ok(metadata) => metadata,
                        Err(err) => {
                            error!("Read cache dir {:?} error: {}", entry.path(), err);
                            return None;
                        }
                    };

                    if metadata.is_file() {
                        Some((entry, FileTime::from_last_modification_time(&metadata), metadata))
                    } else {
                        None
                    }
                })
                .collect()
                .await;
            files.sort_unstable_by(|(_, a, _), (_, b, _)| a.cmp(b));

            // Delete cache until need_free is 0 or mtime is new than cut_off
            let mut new_oldest = files.last().map_or_else(FileTime::now, |(_, mtime, _)| *mtime);
            for (entry, mtime, metadata) in files {
                if mtime > cut_off || need_free == 0 {
                    new_oldest = mtime;
                    break;
                }

                let info = entry.file_name().to_str().and_then(CacheFileInfo::from_file_id);
                let path = entry.path();
                if info.is_none() {
                    warn!("Invalid cache file: {:?}", path);
                    continue;
                }

                if let Some(size) = async_filesize_fast(&path, &metadata).await {
                    self.remove_cache(&info.unwrap()).await;
                    need_free = need_free.saturating_sub(size);
                }
            }

            // Update oldest
            if let Some(state) = self.cache_state.lock().get_mut(&static_range) {
                state.oldest = new_oldest.unix_seconds();
            }
        }
    }
}

async fn async_filesize(path: &Path) -> Option<u64> {
    let path2 = path.to_path_buf();
    match spawn_blocking(move || file_real_size(path2)).await {
        Ok(Ok(size)) => Some(size),
        Ok(Err(e)) => {
            error!("Read cache file {:?} size error: {}", path, e);
            None
        }
        Err(e) => {
            error!("Read cache file {:?} size error: {}", path, e);
            None
        }
    }
}

async fn async_filesize_fast(path: &Path, metadata: &Metadata) -> Option<u64> {
    let path2 = path.to_path_buf();
    let metadata = metadata.clone();
    match spawn_blocking(move || file_real_size_fast(path2, &metadata)).await {
        Ok(Ok(size)) => Some(size),
        Ok(Err(e)) => {
            error!("Read cache file {:?} size error: {}", path, e);
            None
        }
        Err(e) => {
            error!("Read cache file {:?} size error: {}", path, e);
            None
        }
    }
}

async fn delete_java_cache_data<P: AsRef<Path>>(data_dir: P) {
    let base = data_dir.as_ref();
    let _ = remove_file(base.join("pcache_info")).await;
    let _ = remove_file(base.join("pcache_ages")).await;
    let _ = remove_file(base.join("pcache_lru")).await;
}

async fn read_state(path: &Path) -> Result<HashMap<String, CacheState>, Box<dyn std::error::Error>> {
    let mut file = File::open(path).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    Ok(rmp_serde::from_slice(&buf)?)
}

async fn open_dir(path: &Path) -> Option<ReadDirStream> {
    read_dir(path)
        .map_ok(ReadDirStream::new)
        .await
        .inspect_err(|err| error!("Read cache dir error: path={:?}, err={}", path, err))
        .ok()
}

async fn list_files(stream: &mut ReadDirStream) -> Vec<DirEntry> {
    let mut files: Vec<DirEntry> = stream
        .inspect_err(|err| error!("List cache file error: {}", err))
        .filter_map(|entry| async { entry.ok() })
        .collect()
        .await;

    sort_by_inode(&mut files);

    files
}

#[cfg(unix)]
fn sort_by_inode(files: &mut [DirEntry]) {
    files.sort_by_key(|f| f.ino());
}

#[cfg(not(unix))]
fn sort_by_inode(_files: &mut [DirEntry]) {
    // Not support
}

#[cfg(unix)]
async fn fix_permission(path: &Path) {
    use std::os::unix::prelude::PermissionsExt;

    use tokio::fs::set_permissions;

    _ = set_permissions(&path, PermissionsExt::from_mode(0o644)).await;
}

#[cfg(not(unix))]
async fn fix_permission(_path: &Path) {
    // Skip
}

#[cfg(unix)]
fn get_available_space(path: &Path) -> Option<u64> {
    use rustix::fs::statvfs;
    if let Ok(stat) = statvfs(path) {
        Some(stat.f_bavail * stat.f_bsize)
    } else {
        None
    }
}

#[cfg(windows)]
fn get_available_space(path: &Path) -> Option<u64> {
    use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::HSTRING};

    let full_path = path.canonicalize().ok()?;
    let mut free: u64 = 0;

    if unsafe { GetDiskFreeSpaceExW(&HSTRING::from(full_path.as_path()), Some(&mut free), None, None) }.is_ok() {
        Some(free)
    } else {
        None
    }
}

#[cfg(not(any(unix, windows)))]
fn get_available_space(_path: &Path) -> Option<u64> {
    None // Not support
}

async fn clean_temp_dir(path: &Path) {
    info!("Deleting old temp files");

    if let Ok(mut dir) = read_dir(path).await {
        while let Ok(Some(file)) = dir.next_entry().await {
            let path = file.path();
            if path.is_file() && file.file_name().to_string_lossy().starts_with("proxyfile_") {
                debug!("Delete old temp file: {:?}", path);
                let _ = remove_file(path).await;
            }
        }
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
enum FileType {
    Jpeg,
    Png,
    Gif,
    Mp4,
    Webm,
    Webp,
    Avif,
    Jpegxl,
    Other(String),
}

impl FileType {
    fn to_mime(&self) -> Mime {
        match self {
            Self::Jpeg => mime::IMAGE_JPEG,
            Self::Png => mime::IMAGE_PNG,
            Self::Gif => mime::IMAGE_GIF,
            Self::Mp4 => "video/mp4".parse().unwrap(),
            Self::Webm => "video/webm".parse().unwrap(),
            Self::Webp => "image/webp".parse().unwrap(),
            Self::Avif => "image/avif".parse().unwrap(),
            Self::Jpegxl => "image/jxl".parse().unwrap(),
            Self::Other(_) => mime::APPLICATION_OCTET_STREAM,
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
            Self::Webm => "wbm",
            Self::Webp => "wbp",
            Self::Avif => "avf",
            Self::Jpegxl => "jxl",
            Self::Other(s) => s,
        }
    }
}

impl Display for FileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl From<&str> for FileType {
    fn from(s: &str) -> Self {
        match s {
            "jpg" => Self::Jpeg,
            "png" => Self::Png,
            "gif" => Self::Gif,
            "mp4" => Self::Mp4,
            "wbm" => Self::Webm,
            "wbp" => Self::Webp,
            "avf" => Self::Avif,
            "jxl" => Self::Jpegxl,
            _ => Self::Other(s.to_string()),
        }
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
pub struct CacheFileInfo {
    hash: [u8; 20],
    size: u32,
    xres: u32,
    yres: u32,
    mime_type: FileType,
}

impl CacheFileInfo {
    pub fn from_file_id<T: AsRef<str>>(id: T) -> Option<CacheFileInfo> {
        let mut part = id.as_ref().split('-');
        let none_res = part.clone().count() == 3;

        Some(CacheFileInfo {
            hash: <[u8; 20]>::from_hex(part.next()?).ok()?,
            size: part.next().and_then(|s| s.parse().ok())?,
            xres: if none_res { 0 } else { part.next().and_then(|s| s.parse().ok())? },
            yres: if none_res { 0 } else { part.next().and_then(|s| s.parse().ok())? },
            mime_type: part.next()?.into(),
        })
    }

    /// Get the cache file's size.
    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn hash(&self) -> [u8; 20] {
        self.hash
    }

    fn to_path(&self, cache_dir: &Path) -> PathBuf {
        let hash = hex::encode(self.hash);
        let filename = if self.xres > 0 {
            format!("{}-{}-{}-{}-{}", hash, self.size, self.xres, self.yres, self.mime_type)
        } else {
            format!("{}-{}-{}", hash, self.size, self.mime_type)
        };
        let base = cache_dir.as_os_str();
        let mut path = PathBuf::with_capacity(base.len() + 7 + filename.len()); // base + 2 level dir + filename length
        path.push(base);
        path.push(&hash[0..2]);
        path.push(&hash[2..4]);
        path.push(filename);
        path
    }

    fn static_range(&self) -> String {
        hex::encode(&self.hash[0..2])
    }

    pub fn mime_type(&self) -> Mime {
        self.mime_type.to_mime()
    }
}
