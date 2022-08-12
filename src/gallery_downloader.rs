use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use hex::FromHex;
use log::{debug, error, info, warn};
use openssl::sha::Sha1;
use regex::Regex;
use reqwest::Url;
use tokio::{
    fs::{self, create_dir_all},
    io::{AsyncReadExt, AsyncWriteExt},
    time::{sleep, sleep_until, Instant},
};

use crate::{error::Error, rpc::RPCClient, util};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct GalleryDownloader {
    client: Arc<RPCClient>,
    reqwest: reqwest::Client,
    download_dir: PathBuf,
}

impl GalleryDownloader {
    pub fn new<P: AsRef<Path>>(client: Arc<RPCClient>, download_dir: P) -> GalleryDownloader {
        GalleryDownloader {
            client,
            reqwest: util::create_http_client(Duration::from_secs(300)),
            download_dir: download_dir.as_ref().to_path_buf(),
        }
    }

    pub async fn run(&self) {
        let mut task = self.client.fetch_queue(None).await.map(GalleryDownloader::parser);

        'task: while let Some(meta) = task {
            if !self.client.is_running() {
                break;
            }

            let mut meta = match meta {
                Ok(meta) => meta,
                Err(e) => {
                    warn!("Failed to parse metadata for new gallery. {}", e);

                    // Sleep 5s and retry
                    sleep(Duration::from_secs(5)).await;
                    task = self.client.fetch_queue(None).await.map(GalleryDownloader::parser);
                    continue;
                }
            };

            let dir = self.download_dir.join(if meta.title.len() > 100 {
                format!("{}... [{}{}]", &meta.title[..97], meta.gid, meta.xres_title)
            } else {
                format!("{} [{}{}]", meta.title, meta.gid, meta.xres_title)
            });

            if !dir.exists() {
                if let Err(err) = create_dir_all(&dir).await {
                    error!("Create download directory fail: {}", err);
                    return;
                }
            }

            let mut downloaded_files = HashSet::new();
            'retry: for retry in 0..10 {
                for info in &meta.gallery_files {
                    if !self.client.is_running() {
                        break 'task;
                    }

                    // Check if file already downloaded
                    if downloaded_files.contains(info) {
                        continue;
                    }
                    let path = dir.join(format!("{}.{}", info.filename, info.filetype));
                    if info.check_hash(&path).await {
                        downloaded_files.insert(info);
                        continue;
                    }

                    // Get download URL and download file
                    let mut start_time = Instant::now();
                    match self
                        .client
                        .dl_fetch(meta.gid, info.page, info.fileindex, &info.xres, retry % 2 != 0)
                        .await
                        .and_then(|s| Url::parse(&s[0]).ok())
                    {
                        Some(url) => {
                            start_time = Instant::now();
                            match self.download(url.clone(), &path, info.expected_sha1_hash).await {
                                Ok(_) => {
                                    info!(
                                        "Finished downloading gid={} page={}: {}.{}",
                                        meta.gid, info.page, info.filename, info.filetype
                                    );
                                    downloaded_files.insert(info);
                                }
                                Err(err) => {
                                    warn!("Gallery file download error: {}", err);

                                    if err.is::<reqwest::Error>() || err.is::<Error>() {
                                        if let Some(host) = url.host_str() {
                                            meta.failures.push(format!("{}-{}-{}", host, info.fileindex, info.xres))
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            warn!(
                                "Fetch gallery file download url fail. gid={}, page={}, fileindex={}",
                                meta.gid, info.page, info.fileindex
                            );
                        }
                    };

                    // Wait 1s before next download, or 5s if download not success
                    sleep_until(start_time + Duration::from_secs(if downloaded_files.contains(info) { 1 } else { 5 })).await;
                }

                if downloaded_files.len() == meta.filecount {
                    info!("Finished download of gallery: {}", meta.title);

                    if let Err(e) = fs::write(&dir.join("galleryinfo.txt"), &meta.information).await {
                        error!("Could not write galleryinfo.txt: {}", e);
                    }

                    self.client.dl_fails(meta.failures.iter().map(|s| s.as_str()).collect()).await;
                    break 'retry;
                }
            }

            if downloaded_files.len() != meta.filecount {
                warn!("Permanently failed downloading gallery: {}", meta.title);
            }

            task = self.client.fetch_queue(Some(meta)).await.map(GalleryDownloader::parser);
        }
    }

    fn parser(raw_gallery: Vec<String>) -> Result<GalleryMeta, BoxError> {
        debug!("GalleryDownloader: Started gallery metadata parsing");

        let mut gid = 0;
        let mut filecount = 0;
        let mut minxres = String::new();
        let mut xres_title = String::new();
        let mut title = String::new();
        let mut information = String::new();
        let mut gallery_files: Vec<GalleryFile> = Vec::new();

        let mut parse_state = 0;
        for s in raw_gallery {
            if s == "FILELIST" && parse_state == 0 {
                parse_state = 1;
                continue;
            }

            if s == "INFORMATION" && parse_state == 1 {
                parse_state = 2;
                continue;
            }

            // Skip empty line
            if parse_state < 2 && s.is_empty() {
                continue;
            }

            if parse_state == 0 {
                // Basic metadata
                let split = s.splitn(2, ' ').collect::<Vec<&str>>();

                match split[0] {
                    "GID" => {
                        gid = split[1].parse()?;
                        debug!("GalleryDownloader: Parsed gid={}", gid);
                    }
                    "FILECOUNT" => {
                        filecount = split[1].parse()?;
                        debug!("GalleryDownloader: Parsed filecount={}", filecount);
                    }
                    "MINXRES" => {
                        if Regex::new(r"^org|\\d+$").unwrap().is_match(split[1]) {
                            minxres = split[1].to_string();
                            debug!("GalleryDownloader: Parsed minxres={}", minxres);
                        } else {
                            error!("Encountered invalid minxres");
                        }
                    }
                    "TITLE" => {
                        let pattern = Regex::new(r#"(\*|"|\\|<|>|:\|\?)"#).unwrap().replace_all(split[1], "");
                        let pattern = Regex::new(r"\s+").unwrap().replace_all(&pattern, " ");
                        let pattern = Regex::new(r"(^\s+|\s+$)").unwrap().replace_all(&pattern, "").to_string();
                        title = pattern;
                        debug!("GalleryDownloader: Parsed title={}", title);

                        // MINXRES must be passed before TITLE for this to work. the only purpose is to make distinct titles
                        xres_title = if minxres == "org" {
                            String::new()
                        } else {
                            format!("-{}x", minxres)
                        };
                    }
                    _ => (),
                }
            } else if parse_state == 1 {
                // File list
                let split = s.splitn(6, ' ').collect::<Vec<&str>>();

                // entries are on the form: page fileindex xres sha1hash filetype filename
                let page = split[0].parse()?;
                let fileindex = split[1].parse()?;
                let xres = split[2].to_string();

                // sha1hash can be "unknown" if the file has not been generated yet
                let sha1hash = <[u8; 20]>::from_hex(split[3]).ok();

                // the server guarantees that all filenames in the meta file are unique, and that none of them are reserved device filenames
                let filetype = split[4].to_string();
                let filename = split[5].to_string();

                gallery_files.push(GalleryFile {
                    page,
                    fileindex,
                    xres,
                    expected_sha1_hash: sha1hash,
                    filetype,
                    filename,
                })
            } else {
                // Gallery info
                information += &(s + "\n");
            }
        }

        Ok(GalleryMeta {
            gid,
            filecount,
            minxres,
            xres_title,
            title,
            information,
            gallery_files,
            failures: vec![],
        })
    }

    async fn download<P: AsRef<Path>>(&self, url: Url, path: P, hash: Option<[u8; 20]>) -> Result<(), BoxError> {
        let mut file = fs::File::create(&path).await?;
        let mut stream = self
            .reqwest
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map(|r| r.bytes_stream())?;
        let mut hasher = Sha1::new();
        while let Some(bytes) = stream.next().await {
            let bytes = &bytes?;
            file.write_all(bytes).await?;
            hasher.update(bytes);
        }

        if let Some(expected) = hash {
            let hash = hasher.finish();
            if hash != expected {
                return Err(Box::new(Error::HashMismatch { expected, actual: hash }));
            }
        }

        Ok(())
    }
}

#[derive(Hash, Eq, PartialEq)]
struct GalleryFile {
    page: usize,
    fileindex: usize,
    xres: String,
    expected_sha1_hash: Option<[u8; 20]>,
    filetype: String,
    filename: String,
}

impl GalleryFile {
    async fn check_hash(&self, path: &Path) -> bool {
        if !path.exists() || !path.is_file() {
            return false;
        }

        if self.expected_sha1_hash.is_none() {
            // Missing hash, can't verify file.
            return true;
        }

        // Check hash
        if let Ok(mut file) = fs::File::open(&path).await {
            let mut buf = vec![0; 1024 * 1024]; // 1MiB
            let mut hasher = Sha1::new();
            loop {
                if let Ok(n) = file.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[0..n]);
                }
            }
            if hasher.finish() == self.expected_sha1_hash.unwrap() {
                return true;
            }
        }

        false
    }
}

pub struct GalleryMeta {
    gid: i32,
    filecount: usize,
    minxres: String,
    xres_title: String,
    title: String,
    information: String,
    gallery_files: Vec<GalleryFile>,
    failures: Vec<String>,
}

impl GalleryMeta {
    pub fn gid(&self) -> i32 {
        self.gid
    }

    pub fn minxres(&self) -> &str {
        self.minxres.as_ref()
    }
}
