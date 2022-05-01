use std::{
    collections::HashMap,
    fmt::Display,
    fs::{File, Metadata},
    io::Error,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::{Duration, SystemTime},
};

use filesize::{file_real_size, file_real_size_fast};
use filetime::{set_file_mtime, FileTime};
use futures::{stream, StreamExt, TryFutureExt};
use hex::FromHex;
use log::{debug, error, warn};
use mime::Mime;
use openssl::sha::Sha1;
use parking_lot::{Mutex, RwLock};
use tempfile::TempPath;
use tokio::{
    fs::{copy, create_dir_all, metadata, read_dir, rename, DirEntry},
    io::AsyncReadExt,
    runtime::Handle,
    sync::mpsc::UnboundedSender,
    task::spawn_blocking,
    time::{sleep_until, Instant},
};
use tokio_stream::wrappers::ReadDirStream;

use crate::rpc::Settings;

const LRU_SIZE: usize = 1048576;

pub struct CacheManager {
    cache_dir: PathBuf,
    cache_date: Mutex<HashMap<PathBuf, FileTime>>,
    lru_cache: RwLock<Vec<u16>>, // 2MiB
    lru_clear_pos: Mutex<usize>,
    runtime: Handle,
    settings: Arc<Settings>,
    temp_dir: PathBuf,
    total_size: Arc<AtomicU64>,
}

impl CacheManager {
    pub async fn new<P: AsRef<Path>>(
        runtime: Handle,
        cache_dir: P,
        temp_dir: P,
        settings: Arc<Settings>,
        shutdown: UnboundedSender<()>,
    ) -> Result<Arc<Self>, Error> {
        let new = Arc::new(Self {
            cache_dir: cache_dir.as_ref().to_path_buf(),
            cache_date: Mutex::new(HashMap::new()),
            lru_cache: RwLock::new(vec![0; LRU_SIZE]),
            lru_clear_pos: Mutex::new(0),
            runtime,
            settings,
            temp_dir: temp_dir.as_ref().to_path_buf(),
            total_size: Arc::new(AtomicU64::new(0)),
        });

        if new.settings.verify_cache() {
            // Force check cache
            new.scan_cache(16).await?;
            CacheManager::start_background_task(new.clone());
        } else {
            // Background cache scan
            let manager = new.clone();
            new.runtime.spawn(async move {
                if let Err(err) = manager.scan_cache(1).await {
                    error!("Cache scan error: {}", err);
                    let _ = shutdown.send(());
                }
                CacheManager::start_background_task(manager);
            });
        }

        Ok(new)
    }

    pub fn create_temp_file(&self) -> TempPath {
        tempfile::Builder::new()
            .prefix("proxyfile_")
            .tempfile_in(&self.temp_dir)
            .unwrap()
            .into_temp_path()
    }

    pub async fn get_file(&self, info: &CacheFileInfo) -> Option<File> {
        let file = info.get_file(&self.cache_dir);

        if file.is_some() {
            let rt = self.runtime.enter();
            self.mark_recently_accessed(info, true).await;
            drop(rt);
        }

        file
    }

    pub async fn import_cache(&self, info: &CacheFileInfo, file_path: &TempPath) {
        let _rt = self.runtime.enter();
        let path = info.to_path(&self.cache_dir);
        let dir = path.parent().unwrap();

        if !dir.exists() {
            if let Err(err) = create_dir_all(dir).await {
                error!("Create cache directory fail: {}", err);
                return;
            }

            self.cache_date.lock().insert(dir.to_path_buf(), FileTime::now());
        }

        if rename(&file_path, &path).await.is_err() {
            // Can't cross fs move file, try copy.
            if let Err(err) = copy(file_path, &path).await {
                error!("Import cache failed: {}", err);
            }
        }

        self.mark_recently_accessed(info, false).await;

        let total_size = self.total_size.clone();
        let _ = spawn_blocking(move || match file_real_size(&path) {
            Ok(size) => {
                total_size.fetch_add(size, Relaxed);
            }
            Err(err) => error!("Read cache file size error: path={:?}, err={}", &path, err),
        })
        .await;
    }

    pub async fn remove_cache(&self, info: &CacheFileInfo) {
        let path = info.to_path(&self.cache_dir);
        if path.exists() {
            debug!("Delete cache: {:?}", path);
            let total_size = self.total_size.clone();
            let _ = self
                .runtime
                .spawn_blocking(move || match file_real_size(&path) {
                    Ok(size) => {
                        // TODO real delete
                        // if remove_file(&path).is_ok() {
                        total_size.fetch_sub(size, Relaxed);
                        // }
                    }
                    Err(err) => error!("Read cache file size error: path={:?}, err={}", &path, err),
                })
                .await;
        }
    }

    fn start_background_task(new: Arc<Self>) {
        let manager = Arc::downgrade(&new);
        new.runtime.spawn(async move {
            let mut counter: u32 = 0;
            let mut next_run = Instant::now() + Duration::from_secs(10);
            while let Some(manager) = manager.upgrade() {
                sleep_until(next_run).await;

                // Cycle LRU cache
                manager.cycle_lru_cache();
                // Check cache size every 10min
                if counter % 60 == 0 {
                    manager.check_cache_usage().await;
                }

                counter = counter.wrapping_add(1);
                next_run = Instant::now() + Duration::from_secs(10);
            }
        });
    }

    async fn mark_recently_accessed(&self, info: &CacheFileInfo, update_file: bool) {
        let hash = info.hash();
        let index = (u32::from_be_bytes([0, hash[2], hash[3], hash[4]]) >> 4) as usize;
        let bitmask: u16 = 1 << (hash[4] & 0b0000_1111);

        // Check if the file is already in the cache.
        if self.lru_cache.read()[index] & bitmask != 0 {
            // Already marked, return
            return;
        }

        // Mark the file as recently accessed.
        self.lru_cache.write()[index] |= bitmask;

        if update_file {
            let path = info.to_path(&self.cache_dir);
            if let Ok(metadata) = metadata(&path).await {
                let one_week_ago = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 7);
                if FileTime::from_last_modification_time(&metadata) < one_week_ago.into() {
                    // Update file modification time
                    let _ = spawn_blocking(move || {
                        if let Err(err) = set_file_mtime(&path, FileTime::now()) {
                            error!("Update cache file time error: path={:?}, err={}", &path, err);
                        }
                    })
                    .await;
                }
            }
        }
    }

    async fn scan_cache(&self, parallelism: usize) -> Result<(), Error> {
        let static_range = self.settings.static_range();
        let mut dirs = Vec::with_capacity(static_range.len());

        let mut root = read_dir(&self.cache_dir).map_ok(ReadDirStream::new).await?;
        while let Some(l1) = root.next().await.transpose()? {
            let l1_path = l1.path();
            if l1_path.is_dir() {
                let mut l1_stream = read_dir(&l1_path).map_ok(ReadDirStream::new).await?;
                while let Some(l2) = l1_stream.next().await.transpose()? {
                    let l2_path = l2.path();
                    if l2_path.is_dir() {
                        if static_range
                            .iter()
                            .any(|sr| l1.file_name().eq_ignore_ascii_case(&sr[0..2]) && l2.file_name().eq_ignore_ascii_case(&sr[2..4]))
                        {
                            dirs.push(l2_path);
                        } else {
                            warn!("Found cache dir but not in static range: {}", l2_path.to_str().unwrap_or_default());
                        }
                    }
                }
            }
        }

        debug!("Cache dir number: {}", &dirs.len());

        let lru_cutoff = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 7));
        let scan_task = stream::iter(dirs.into_iter().map(|dir| async move {
            let mut time = FileTime::now();
            let mut stream = read_dir(&dir).map_ok(ReadDirStream::new).await?;
            while let Some(entry) = stream.next().await.transpose()? {
                let metadata = metadata(entry.path()).await;
                if let Err(err) = metadata {
                    error!("Read cache file metadata error: path={:?}, err={}", &entry.path(), err);
                    continue;
                }
                let metadata = metadata.unwrap();
                if metadata.is_dir() {
                    continue;
                }

                // Parse info
                let info = entry.file_name().to_str().and_then(CacheFileInfo::from_file_id);
                if info.is_none() {
                    warn!("Invalid cache file: {:?}", entry.path());
                    continue;
                }
                let info = info.unwrap();

                if self.settings.verify_cache() {
                    let path = entry.path();
                    let mut hasher = Sha1::new();
                    let mut file = tokio::fs::File::open(&path).await?;
                    let mut buf = vec![0; 1024 * 1024]; // 1MiB
                    loop {
                        let n = file.read(&mut buf).await?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&buf[0..n]);
                    }
                    let actual_hash = hasher.finish();
                    if actual_hash != info.hash {
                        error!(
                            "Cache file hash mismatch: path={:?}, hash={:x?}, actual={:x?}",
                            &path, &info.hash, &actual_hash
                        );
                        // TODO delete broken cache file
                        continue;
                    }
                }

                self.total_size.fetch_add(file_real_size_fast(entry.path(), &metadata)?, Relaxed);

                // Add recently accessed file to the cache.
                let mtime = FileTime::from_last_modification_time(&metadata);
                if mtime > lru_cutoff {
                    self.mark_recently_accessed(&info, false).await;
                }
                if mtime < time {
                    time = mtime;
                }
            }

            Ok::<(PathBuf, FileTime), Error>((dir, time))
        }))
        .buffer_unordered(parallelism)
        .collect::<Vec<_>>()
        .await;

        debug!("Cache size: {}", self.total_size.load(Relaxed));

        // Save oldest mtime
        let mut map = self.cache_date.lock();
        map.clear();
        for task in scan_task {
            match task {
                Ok((dir, time)) => {
                    map.insert(dir, time);
                }
                Err(err) => error!("Scan cache dir error: {}", err),
            }
        }

        Ok(())
    }

    fn cycle_lru_cache(&self) {
        let mut pos = self.lru_clear_pos.lock();
        // 1048576 / (1week / 10s) =~ 17
        for _ in 0..17 {
            self.lru_cache.write()[*pos] = 0;
            *pos = (*pos + 1) % LRU_SIZE;
        }
    }

    async fn check_cache_usage(&self) {
        let mut need_free = self.total_size.load(Relaxed).saturating_sub(self.settings.size_limit());
        while need_free > 0 {
            debug!("Start cache cleaner");
            let map = self.cache_date.lock().clone();
            let mut dirs = map.iter().collect::<Vec<_>>();
            dirs.sort_by(|(_, a), (_, b)| a.cmp(b));

            let files = read_dir(dirs[0].0).map_ok(ReadDirStream::new).await;
            if let Err(err) = files {
                error!("Read cache dir error: {}", err);
                break;
            }
            let cut_off = dirs[1].1;

            let mut files: Vec<(DirEntry, FileTime, Metadata)> = files
                .unwrap()
                .filter_map(|entry| async move {
                    if let Err(err) = entry {
                        error!("Read cache file error: {}", err);
                        return None;
                    }
                    let entry = entry.unwrap();

                    let metadata = metadata(entry.path()).await;
                    if let Err(err) = metadata {
                        error!("Read cache file metadata error: {}", err);
                        return None;
                    }
                    let metadata = metadata.unwrap();

                    if metadata.is_file() {
                        Some((entry, FileTime::from_last_modification_time(&metadata), metadata))
                    } else {
                        None
                    }
                })
                .collect()
                .await;
            files.sort_by(|(_, a, _), (_, b, _)| a.cmp(b));

            let mut new_oldest = *cut_off;
            for file in files {
                let (entry, mtime, metadata) = file;

                if &mtime > cut_off || need_free == 0 {
                    new_oldest = mtime;
                    break;
                }

                let info = entry.file_name().to_str().and_then(CacheFileInfo::from_file_id);
                let path = entry.path();
                if info.is_none() {
                    warn!("Invalid cache file: {:?}", path);
                    continue;
                }

                let size = file_real_size_fast(&path, &metadata);
                if let Err(err) = size {
                    error!("Read cache file size error: {}", err);
                    continue;
                }
                let size = size.unwrap();

                debug!("Remove old cache file: {:?}", &path);
                self.remove_cache(&info.unwrap()).await;

                need_free = need_free.saturating_sub(size);
            }

            self.cache_date.lock().insert(dirs[0].0.to_path_buf(), new_oldest);
        }
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
enum FileType {
    Jpeg,
    Png,
    Gif,
    Webm,
    Other(String),
}

impl FileType {
    fn to_mime(&self) -> Mime {
        match self {
            Self::Jpeg => mime::IMAGE_JPEG,
            Self::Png => mime::IMAGE_PNG,
            Self::Gif => mime::IMAGE_GIF,
            Self::Webm => "video/webm".parse().unwrap(),
            Self::Other(_) => mime::APPLICATION_OCTET_STREAM,
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Webm => "wbm",
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
            "wbm" => Self::Webm,
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
        Some(CacheFileInfo {
            hash: <[u8; 20]>::from_hex(part.next()?).ok()?,
            size: part.next().and_then(|s| s.parse().ok())?,
            xres: part.next().and_then(|s| s.parse().ok())?,
            yres: part.next().and_then(|s| s.parse().ok())?,
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
        cache_dir
            .join(&hash[0..2])
            .join(&hash[2..4])
            .join(format!("{}-{}-{}-{}-{}", hash, self.size, self.xres, self.yres, self.mime_type))
    }

    fn get_file(&self, cache_dir: &Path) -> Option<File> {
        let path = self.to_path(cache_dir);

        if path.exists() && path.is_file() {
            File::open(path).ok()
        } else {
            None
        }
    }

    pub fn mime_type(&self) -> Mime {
        self.mime_type.to_mime()
    }
}
