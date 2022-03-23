use std::{
    fs::File,
    io::Error,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
};

use filesize::file_real_size;
use futures::{StreamExt, TryFutureExt};
use log::{debug, error};
use mime::Mime;
use tokio::fs::{copy, create_dir_all, read_dir, rename};
use tokio_stream::wrappers::ReadDirStream;

pub struct CacheManager {
    cache_dir: PathBuf,
    temp_dir: PathBuf,
    size_limit: Arc<AtomicU64>,
    total_size: Arc<AtomicU64>,
}

impl CacheManager {
    pub async fn new<P: AsRef<Path>>(
        cache_dir: P,
        temp_dir: P,
        size_limit: u64,
        static_range: Vec<String>,
        _verify_cache: bool,
    ) -> Result<CacheManager, Error> {
        let mut dirs = Vec::with_capacity(static_range.len());

        // TODO error handle
        let mut root = read_dir(&cache_dir).map_ok(ReadDirStream::new).await?;
        while let Some(l1) = root.next().await.transpose()? {
            let l1_path = l1.path();
            if l1_path.is_dir() {
                let mut l1_stream = read_dir(&l1_path).map_ok(ReadDirStream::new).await?;
                while let Some(l2) = l1_stream.next().await.transpose()? {
                    let l2_path = l2.path();
                    if l2_path.is_dir()
                        && static_range
                            .iter()
                            .any(|sr| l1.file_name().eq_ignore_ascii_case(&sr[0..2]) && l2.file_name().eq_ignore_ascii_case(&sr[2..4]))
                    {
                        dirs.push(l2_path);
                    }
                }
            }
        }

        debug!("Cache dirs: {:?}", dirs);

        // TODO check file & build lru

        Ok(Self {
            cache_dir: cache_dir.as_ref().to_path_buf(),
            temp_dir: temp_dir.as_ref().to_path_buf(),
            size_limit: Arc::new(AtomicU64::new(size_limit)),
            total_size: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn cache_dir(&self) -> &Path {
        self.cache_dir.as_path()
    }

    pub fn temp_dir(&self) -> &Path {
        self.temp_dir.as_path()
    }

    pub async fn import_cache<P: AsRef<Path>>(&self, info: &CacheFileInfo, file_path: P) {
        let path = info.to_path(&self.cache_dir);
        let dir = path.parent().unwrap();

        if !dir.exists() {
            if let Err(err) = create_dir_all(dir).await {
                error!("Create cache directory fail: {}", err);
                return;
            }
        }

        if rename(&file_path, &path).await.is_err() {
            // Can't cross fs move file, try copy.
            if let Err(err) = copy(file_path, &path).await {
                error!("Import cache failed: {}", err);
            }
        }

        // TODO update LRU cache
        let total_size = self.total_size.clone();
        tokio::task::spawn_blocking(move || match file_real_size(&path) {
            Ok(size) => {
                total_size.fetch_add(size, Relaxed);
            }
            Err(err) => error!("Read cache file size error: path={:?}, err={}", &path, err),
        });
    }

    pub async fn remove_cache(&self, info: &CacheFileInfo) {
        let path = info.to_path(self.cache_dir());
        if path.exists() {
            debug!("Delete cache: {}", path.to_str().unwrap_or_default());
            let total_size = self.total_size.clone();
            tokio::task::spawn_blocking(move || match file_real_size(&path) {
                Ok(size) => {
                    total_size.fetch_sub(size, Relaxed);
                }
                Err(err) => error!("Read cache file size error: path={:?}, err={}", &path, err),
            });
            // TODO real delete & update LRU cache
        }
    }

    pub async fn mark_recently_accessed(&self, _info: &CacheFileInfo) {
        // TODO
    }
}

#[derive(Clone)]
pub struct CacheFileInfo {
    hash: String,
    size: u64,
    xres: u32,
    yres: u32,
    mime_type: String,
}

impl CacheFileInfo {
    pub fn from_file_id<T: AsRef<str>>(id: T) -> Option<CacheFileInfo> {
        let mut part = id.as_ref().split('-');
        Some(CacheFileInfo {
            hash: part.next()?.to_string(),
            size: part.next().and_then(|s| s.parse().ok())?,
            xres: part.next().and_then(|s| s.parse().ok())?,
            yres: part.next().and_then(|s| s.parse().ok())?,
            mime_type: part.next()?.to_string(),
        })
    }

    /// Get the cache file's size.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Get a reference to the cache file info's hash.
    pub fn hash(&self) -> &str {
        self.hash.as_ref()
    }

    pub fn to_file_id(&self) -> String {
        format!("{}-{}-{}-{}-{}", self.hash, self.size, self.xres, self.yres, self.mime_type)
    }

    pub fn to_path(&self, cache_dir: &Path) -> PathBuf {
        cache_dir.join(&self.hash[0..2]).join(&self.hash[2..4]).join(&self.to_file_id())
    }

    pub fn get_file(&self, cache_dir: &Path) -> Option<File> {
        let path = self.to_path(cache_dir);

        if path.exists() && path.is_file() {
            File::open(path).ok()
        } else {
            None
        }
    }

    pub fn mime_type(&self) -> Mime {
        match self.mime_type.as_str() {
            "jpg" => mime::IMAGE_JPEG,
            "png" => mime::IMAGE_PNG,
            "gif" => mime::IMAGE_GIF,
            "wbm" => "video/webm".parse().unwrap(),
            _ => mime::APPLICATION_OCTET_STREAM,
        }
    }
}
