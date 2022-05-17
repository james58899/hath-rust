use log::debug;
use regex::Regex;

struct GalleryDownloader {}

impl GalleryDownloader {
    pub fn new() -> GalleryDownloader {
        GalleryDownloader {}
    }

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
                            panic!("Encountered invalid minxres");
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
                panic!("GalleryDownloader: Failed to parse metadata for new gallery.");
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
}