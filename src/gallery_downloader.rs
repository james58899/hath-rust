use log::{warn, debug};
use regex::Regex;

use crate::rpc::RPCClient;

pub struct GalleryDownloader<'a> {
    client: &'a RPCClient,
    failures: Vec<String>,
}

struct FileMeta {
    page: usize,
    fileindex: usize,
    xres: String,
    sha1hash: String,
    filetype: String,
    filename: String,
}

struct GalleryMeta {
    gid: i32,
    filecount: usize,
    minxres: String,
    title: String,
    filemeta: Vec<FileMeta>,
    information: String,
}

impl GalleryDownloader<'_> {
    pub async fn new(client: &RPCClient) -> GalleryDownloader<'_> {
        GalleryDownloader {
            client,
            failures: Vec::new(),
        }
    }

    async fn get_meta(&self, downloaded_payload: Option<&str>) -> Vec<String> {
        let client = self.client;

        // todo downloaded payload.
        client.fetch_queue().await.unwrap()
    }

    async fn parse_meta(&self, meta: &str) -> GalleryMeta {
        match meta {
            "INVALID_REQUEST" => warn!("GalleryDownloader: Request was rejected by the server"),
            "NO_PENDING_DOWNLOADS" => (),
            _ => debug!("GalleryDownloader: Started gallery metadata parsing.")
        }

        let mut gid = 0;
        let mut filecount: usize = 0;
        let mut minxres = String::new();
        let mut title = String::new();
        let mut filemeta: Vec<FileMeta> = Vec::new();
        let mut information = String::new();

        let mut parse_state = 0;
        for s in meta.split('\n') {
            if s == "FILELIST" && parse_state == 0 {
                parse_state = 1;
                continue;
            }

            if s == "INFORMATION" && parse_state == 1 {
                parse_state = 2;
                continue;
            }

            if parse_state < 2 && s.is_empty() { continue; }

            if parse_state == 0 {
                let split: Vec<&str> = s.splitn(2, ' ').collect();

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
                        let pattern = Regex::new(r"^org|\\d+$").unwrap();
                        if pattern.is_match(split[1]) {
                            minxres = split[1].to_string();
                            debug!("GalleryDownloader: Parsed minxres={}", minxres);
                        }
                        else {
                            println!("Encountered invalid minxres");
                        }
                    },
                    "TITLE" => {
                        let pattern = Regex::new(r#"(\*|"|\\|<|>|:\|\?)"#).unwrap().replace_all(split[1], "");
                        title = pattern.as_ref().to_string();
                        debug!("GalleryDownloader: Parsed title={}", title);

                        // MINXRES must be passed before TITLE for this to work. the only purpose is to make distinct titles
                        // let xres_title = if minxres == "org" { String::new() } else { String::from("-") + minxres + "x" };
                        // let xres_title = xres_title.as_str();
                    }
                    _ => (),
                }
            }
            else if parse_state == 1 {
                // entries are on the form: page fileindex xres sha1hash filetype filename
                let split: Vec<&str> = s.splitn(6, ' ').collect();
                // let page = split[0].parse::<usize>().unwrap();
                // let fileindex = split[1].parse::<usize>().unwrap();
                // let xres = split[2];

                // sha1hash can be "unknown" if the file has not been generated yet
                // let sha1hash = if split[3] == "unknown" { "" } else { split[3] };

                // the server guarantees that all filenames in the meta file are unique, and that none of them are reserved device filenames
                // let filetype = split[4];
                // let filename = split[5];

                // savepoint Note: perform download task at another function.
                filemeta.push(
                    FileMeta {
                        page: split[0].parse::<usize>().unwrap(), 
                        fileindex: split[1].parse::<usize>().unwrap(),
                        xres: split[2].to_string(),
                        sha1hash: if split[3] == "unknown" { "".to_string() } else { split[3].to_string() },
                        filetype: split[4].to_string(),
                        filename: split[5].to_string(),
                    }
                );
            }
            else {
                information+= s;
                information+= "\n";
            }
        }

        GalleryMeta {
            gid,
            filecount,
            minxres,
            title,
            filemeta,
            information,
        }
    }
}