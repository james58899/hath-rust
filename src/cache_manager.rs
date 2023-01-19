use std::{
    collections::HashMap,
    fmt::Display,
    fs::Metadata,
    io::Error,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed},
        Arc,
    },
    time::{Duration, SystemTime},
};

use filesize::{file_real_size, file_real_size_fast};
use filetime::{set_file_mtime, FileTime};
use futures::{stream, StreamExt, TryFutureExt};
use hex::FromHex;
use log::{debug, error, info, warn};
use mime::Mime;
use openssl::sha::Sha1;
use parking_lot::{Mutex, RwLock};
use tempfile::TempPath;
use tokio::{
    fs::{copy, create_dir_all, metadata, read_dir, remove_dir_all, remove_file, rename, DirEntry, File},
    io::AsyncReadExt,
    spawn,
    sync::mpsc::UnboundedSender,
    task::spawn_blocking,
    time::{sleep_until, Instant},
};
use tokio_stream::wrappers::ReadDirStream;

use crate::rpc::{InitSettings, Settings};

const LRU_SIZE: usize = 1048576;

pub struct CacheManager {
    cache_dir: PathBuf,
    cache_date: Mutex<HashMap<PathBuf, FileTime>>,
    lru_cache: RwLock<Vec<u16>>, // 2MiB
    lru_clear_pos: Mutex<usize>,
    settings: Arc<Settings>,
    temp_dir: PathBuf,
    total_size: Arc<AtomicU64>,
}

impl CacheManager {
    pub async fn new<P: AsRef<Path>>(
        cache_dir: P,
        temp_dir: P,
        settings: Arc<Settings>,
        init_settings: &InitSettings,
        shutdown: UnboundedSender<()>,
    ) -> Result<Arc<Self>, Error> {
        let new = Arc::new(Self {
            cache_dir: cache_dir.as_ref().to_path_buf(),
            cache_date: Mutex::new(HashMap::new()),
            lru_cache: RwLock::new(vec![0; LRU_SIZE]),
            lru_clear_pos: Mutex::new(0),
            settings,
            temp_dir: temp_dir.as_ref().to_path_buf(),
            total_size: Arc::new(AtomicU64::new(0)),
        });

        let manager = new.clone();
        let verify_cache = init_settings.verify_cache();
        let static_range = init_settings.static_range();
        if verify_cache {
            // Force check cache
            info!("Start force cache check");
            new.scan_cache(static_range, 16, verify_cache).await?;
            CacheManager::start_background_task(manager);
        } else {
            // Background cache scan
            info!("Start background cache scan");
            spawn(async move {
                if let Err(err) = manager.scan_cache(static_range, 1, verify_cache).await {
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

    pub async fn get_file(&self, info: &CacheFileInfo) -> Option<PathBuf> {
        let file = info.get_file(&self.cache_dir).await;
        if file.is_some() {
            self.mark_recently_accessed(info, true).await;
        }

        file
    }

    pub async fn import_cache(&self, info: &CacheFileInfo, file_path: &TempPath) {
        let path = info.to_path(&self.cache_dir);
        let dir = path.parent().unwrap();

        if metadata(dir).await.is_err() {
            if let Err(err) = create_dir_all(dir).await {
                error!("Create cache directory fail: {}", err);
                return;
            }

            self.cache_date.lock().insert(dir.to_path_buf(), FileTime::now());
        }

        // Try remove existing file
        self.remove_cache(info).await;

        // Importing
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
        if metadata(&path).await.is_ok() {
            debug!("Delete cache: {:?}", path);
            let total_size = self.total_size.clone();
            let _ = spawn_blocking(move || match file_real_size(&path) {
                Ok(size) => match std::fs::remove_file(&path) {
                    Ok(_) => {
                        total_size.fetch_sub(size, Relaxed);
                    }
                    Err(err) => error!("Delete cache file error: path={:?}, err={}", &path, err),
                },
                Err(err) => error!("Read cache file size error: path={:?}, err={}", &path, err),
            })
            .await;
        }
    }

    fn start_background_task(new: Arc<Self>) {
        let manager = Arc::downgrade(&new);
        spawn(async move {
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

    async fn scan_cache(&self, static_range: Vec<String>, parallelism: usize, verify_cache: bool) -> Result<(), Error> {
        let mut dirs = Vec::with_capacity(static_range.len());

        let mut root = read_dir(&self.cache_dir).map_ok(ReadDirStream::new).await?;
        while let Some(l1) = root.next().await.transpose()? {
            let l1_path = l1.path();
            if !l1_path.is_dir() {
                warn!("Found unexpected file in cache dir: {}", l1_path.to_str().unwrap_or_default());
                continue;
            };

            let mut l1_stream = read_dir(&l1_path).map_ok(ReadDirStream::new).await?;
            while let Some(l2) = l1_stream.next().await.transpose()? {
                let l2_path = l2.path();
                if !l2_path.is_dir() {
                    warn!("Found unexpected file in cache dir: {}", l2_path.to_str().unwrap_or_default());
                    continue;
                };

                let mut hash = l1.file_name().clone();
                hash.push(l2.file_name());
                if static_range.iter().any(|sr| hash.eq_ignore_ascii_case(sr)) {
                    dirs.push(l2_path);
                } else {
                    warn!("Delete not in static range dir: {}", l2_path.to_str().unwrap_or_default());
                    if let Err(err) = remove_dir_all(&l2_path).await {
                        error!("Delete cache dir error: path={:?}, err={}", &l2_path, err);
                    }
                }
            }
        }

        debug!("Cache dir number: {}", &dirs.len());

        let lru_cutoff = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 7)); // 1 week
        let counter = &AtomicUsize::new(0);
        let total = dirs.len();
        let scan_task = stream::iter(dirs)
            .map(|dir| async move {
                let mut time = FileTime::now();
                let mut stream = read_dir(&dir).map_ok(ReadDirStream::new).await?;
                while let Some(entry) = stream.next().await.transpose()? {
                    let path = entry.path();
                    let metadata = metadata(&path).await;
                    if let Err(err) = metadata {
                        error!("Read cache file metadata error: path={:?}, err={}", path, err);
                        continue;
                    }
                    let metadata = metadata.unwrap();
                    if metadata.is_dir() {
                        warn!("Found unexpected dir in cache dir: {:?}", path);
                        continue;
                    }

                    // Parse info
                    let info = entry.file_name().to_str().and_then(CacheFileInfo::from_file_id);
                    if info.is_none() {
                        warn!("Invalid cache file: {:?}", path);
                        continue;
                    }
                    let info = info.unwrap();

                    let size = metadata.len();
                    if size != info.size() as u64 {
                        warn!(
                            "Delete corrupt cache file: path={:?}, size={:x?}, actual={:x?}",
                            &path,
                            &info.size(),
                            size
                        );

                        if let Err(err) = remove_file(&path).await {
                            error!("Delete corrupt cache file error: path={:?}, err={}", &path, err);
                        }
                        continue;
                    }

                    if verify_cache {
                        let mut hasher = Sha1::new();
                        let mut file = File::open(&path).await?;
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
                            warn!(
                                "Delete corrupt cache file: path={:?}, hash={:x?}, actual={:x?}",
                                path, &info.hash, &actual_hash
                            );
                            if let Err(err) = remove_file(&path).await {
                                error!("Delete corrupt cache file error: path={:?}, err={}", path, err);
                            }
                            continue;
                        }
                    }

                    self.total_size.fetch_add(file_real_size_fast(&path, &metadata)?, Relaxed);

                    // Add recently accessed file to the cache.
                    let mtime = FileTime::from_last_modification_time(&metadata);
                    if mtime > lru_cutoff {
                        self.mark_recently_accessed(&info, false).await;
                    }
                    if mtime < time {
                        time = mtime;
                    }
                }

                let count = counter.fetch_add(1, Relaxed) + 1;
                if count % 100 == 0 || count == total {
                    info!("Scanned {}/{} static ranges.", count, total);
                }

                Ok::<(PathBuf, FileTime), Error>((dir, time))
            })
            .buffer_unordered(parallelism)
            .collect::<Vec<_>>()
            .await;

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

        info!("Finished cache scan. Cache size: {}", self.total_size.load(Relaxed));

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
        if need_free == 0 {
            return;
        }

        debug!("Start cache cleaner: need_free={}", need_free);
        while need_free > 0 {
            let map = self.cache_date.lock().clone();
            let mut dirs = map.iter().collect::<Vec<_>>();
            dirs.sort_by(|(_, a), (_, b)| a.cmp(b));

            let files = read_dir(dirs[0].0).map_ok(ReadDirStream::new).await;
            if let Err(err) = files {
                error!("Read cache dir {:?} error: {}", dirs[0].0, err);
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

                    let entry = entry.ok()?;
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

                let size = match file_real_size_fast(&path, &metadata) {
                    Ok(size) => size,
                    Err(err) => {
                        error!("Read cache file {:?} size error: {}", path, err);
                        continue;
                    }
                };

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

    async fn get_file(&self, cache_dir: &Path) -> Option<PathBuf> {
        let path = self.to_path(cache_dir);
        let metadata = metadata(&path).await;
        if metadata.is_ok() && metadata.unwrap().is_file() {
            Some(path)
        } else {
            None
        }
    }

    pub fn mime_type(&self) -> Mime {
        self.mime_type.to_mime()
    }
}
