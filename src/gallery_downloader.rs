use std::{path::Path, time::Duration, sync::Arc, error::Error};

use actix_web_lab::__reexports::futures_util;
use log::{debug, error};
use openssl::sha::Sha1;
use regex::Regex;
use reqwest::Client;
use tokio::{fs::{self, OpenOptions}, io::{AsyncWriteExt, AsyncSeekExt}, sync::watch};
use futures_util::StreamExt;
use tokio::io::SeekFrom;

use crate::{rpc::RPCClient, util};

struct GalleryDownloader {
    client: Arc<RPCClient>,
    reqwest: reqwest::Client,
}

impl GalleryDownloader {
    pub fn new(client: Arc<RPCClient>) -> GalleryDownloader {
        GalleryDownloader {
            client,
            reqwest: util::create_http_client(),
        }
    }

    pub fn run(&self) {}

    fn parser(raw_gallery: String) -> GalleryMeta {
        debug!("GalleryDownloader: Started gallery metadata parsing");
        
        let mut gid = 0;
        let mut filecount = 0;
        let mut minxres = String::new();
        let mut xres_title = String::new();
        let mut title = String::new();
        let mut information = String::new();
        let mut gallery_files: Vec<GalleryFile> = Vec::new();

        let mut parse_state = 0;
        for s in raw_gallery.split('\n') {
            if s == "FILELIST" && parse_state == 0 {
                parse_state = 1;
                continue;
            }

            if s == "INFORMATION" && parse_state == 1 {
                parse_state = 2;
                continue;
            }

            if parse_state < 2 && s.is_empty() {
                continue;
            }

            if parse_state == 0 {
                let split = s.splitn(2, ' ').collect::<Vec<&str>>();

                match split[0] {
                    "GID" => {
                        gid = split[1].parse::<i32>().unwrap();
                        debug!("GalleryDownloader: Parsed gid={}", gid);
                    },
                    "FILECOUNT" => {
                        filecount = split[1].parse::<usize>().unwrap();
                        debug!(" GalleryDownloader: Parsed filecount={}", filecount);
                    },
                    "MINXRES" => {
                        if Regex::new(r"^org|\\d+$").unwrap().is_match(split[1]) {
                            minxres = split[1].to_string();
                            debug!("GalleryDownloader: Parsed minxres={}", minxres);
                        }
                        else {
                            error!("Encountered invalid minxres");
                        }
                    },
                    "TITLE" => {
                        let pattern = Regex::new(r#"(\*|"|\\|<|>|:\|\?)"#).unwrap().replace_all(split[1], "").as_ref().to_string();
                        let pattern = Regex::new(r"\s+").unwrap().replace_all(&pattern, " ").as_ref().to_string();
                        let pattern = Regex::new(r"(^\s+|\s+$)").unwrap().replace_all(&pattern, "").as_ref().to_string();
                        title = pattern;
                        debug!("GalleryDownloader: Parsed title={}", title);

                        // MINXRES must be passed before TITLE for this to work. the only purpose is to make distinct titles
                        xres_title = if minxres == "org" { String::new() } else { format!("-{}x", minxres) };
                    }
                    _ => (),
                }

            }
            else if parse_state == 1 {
                let split = s.splitn(6, ' ').collect::<Vec<&str>>();

                // entries are on the form: page fileindex xres sha1hash filetype filename
                let page = split[0].parse::<usize>().unwrap();
                let fileindex = split[1].parse::<usize>().unwrap();
                let xres = split[2].to_string();

                // sha1hash can be "unknown" if the file has not been generated yet
                let sha1hash = if split[3] == "unknown" { String::new() } else { split[3].to_string() };

                // the server guarantees that all filenames in the meta file are unique, and that none of them are reserved device filenames
                let filetype = split[4].to_string();
                let filename = split[5].to_string();

                gallery_files.push(GalleryFile::new(
                    page,
                    fileindex,
                    xres,
                    sha1hash,
                    filetype,
                    filename
                ))
            }
            else {
                information += format!("{}\n", s).as_str();
            }
        }

        GalleryMeta::new(
            gid,
            filecount,
            minxres,
            xres_title,
            title,
            information,
            gallery_files,
            vec![], // TODO
        )
    }
}

struct GalleryFile {
    page: usize,
    fileindex: usize,
    xres: String,
    expected_sha1_hash: String,
    filetype: String,
    filename: String,
}

impl GalleryFile {
    pub fn new(
        page: usize, 
        fileindex: usize, 
        xres: String,
        expected_sha1_hash: String,
        filetype: String,
        filename: String,
    ) -> GalleryFile {
        GalleryFile {
            page,
            fileindex,
            xres,
            expected_sha1_hash,
            filetype,
            filename,
        }
    }

    pub async fn download(self, client: Arc<RPCClient>, reqwest: Client, gid: i32, dir_path: &str) -> Result<(), Box<dyn Error>> {
        let sources = client.dl_fetch(gid, self.page, self.fileindex, self.xres, false).await.unwrap();
        let file_path = format!("{}/{}", dir_path, self.filename);

        tokio::spawn(async move {
            let mut hasher = Sha1::new();
            let mut progress = 0;
            'source: for source in sources {
                'retry: for _ in 0..3 {
                    let mut file = match OpenOptions::new()
                    .write(true)
                    .create(true)
                    .open(file_path.clone())
                    .await {
                        Ok(mut f) => {
                            if let Err(err) = f.seek(SeekFrom::Start(progress)).await {
                                error!("Proxy temp file seek fail: {}", err);
                                continue 'retry;
                            }
                            f
                        }
                        Err(err) => {
                            error!("Proxy temp file create fail: {}", err);
                            continue 'retry;
                        }
                    };

                    let (tx, mut rx) = watch::channel(0); // Download progress
                    
                    let mut download = 0;
                    if let Ok(mut res) = reqwest.get(&source).send().await {
                        let file_size = res.content_length().unwrap();
                        let mut stream = res.bytes_stream();
                        while let Some(bytes) = stream.next().await {
                            let bytes = match &bytes {
                                Ok(it) => it,
                                Err(err) => {
                                    error!("Proxy download fail: {}", err);
                                    continue 'retry; // Try next source
                                }
                            };
                            download += bytes.len() as u64;

                            // Skip downloaded data
                            if download <= progress {
                                continue;
                            }

                            let write_size = download - progress;
                            let data = &bytes[..write_size as usize];
                            if let Err(err) = file.write_all(data).await {
                                error!("Proxy temp file write fail: {}", err);
                                continue 'retry;
                            }

                            hasher.update(data);
                            progress += write_size;
                            let _ = tx.send(progress); // Ignore error
                        }

                        if progress == file_size {
                            if let Err(err) = file.flush().await {
                                error!("Proxy temp file flush fail: {}", err);
                                break 'source;
                            }
                            
                            tx.closed().await;

                            let hash = hex::encode(hasher.finish());
                            if hash == self.expected_sha1_hash {
                                debug!("File {} is expected.", &file_path)
                            } // TODO
                            else {
                                println!("File {} not expected.", &file_path)
                            }
                            
                            break 'source;
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

struct GalleryMeta {
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
    pub fn new(
        gid: i32,
        filecount: usize,
        minxres: String,
        xres_title: String,
        title: String,
        information: String,
        gallery_files: Vec<GalleryFile>,
        failures: Vec<String>,
    ) -> GalleryMeta {
        GalleryMeta { 
            gid,
            filecount,
            minxres,
            xres_title,
            title,
            information,
            gallery_files,
            failures,
        }
    }

    pub async fn download(self, client: Arc<RPCClient>, reqwest: Client) -> Result<(), Box<dyn Error>> {
        let download_dir = "download";
        let download_path = Path::new(download_dir);

        if Path::exists(download_path) && !Path::is_dir(download_path) {
            fs::create_dir(download_dir).await?;
        }

        if self.title.len() > 100 {
            fs::create_dir(
                format!(
                    "{}/{}... [{}]", 
                    download_dir, 
                    &self.title[..97],
                    self.xres_title
                )
            ).await?;
        }
        else {
            fs::create_dir(
                format!(
                    "{}/{} [{}{}]",
                    download_dir,
                    self.title,
                    self.gid,
                    self.xres_title
                )
            ).await?;
        }

        for file in self.gallery_files {
            file.download(client.clone(), reqwest.clone(), self.gid, download_dir).await?;
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        Ok(())
    }
}