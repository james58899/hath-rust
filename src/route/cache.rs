use std::{io::SeekFrom, ops::RangeInclusive, sync::Arc, time::Duration};

use actix_files::NamedFile;
use actix_web::{
    body::SizedStream,
    http::header::{ContentType, TryIntoHeaderPair},
    route,
    web::{BytesMut, Data},
    HttpRequest, HttpResponse, Responder,
};
use actix_web_lab::{
    extract::Path,
    header::{CacheControl, CacheDirective},
};
use async_stream::stream;
use futures::StreamExt;
use log::error;
use openssl::sha::Sha1;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::watch,
};

use crate::{
    cache_manager::CacheFileInfo,
    route::{forbidden, parse_additional},
    util::string_to_hash,
    AppState,
};

static TTL: RangeInclusive<i64> = -900..=900; // Token TTL 15 minutes

#[route("/h/{fileid}/{additional}/{filename:.*}", method = "GET", method = "HEAD")]
async fn hath(
    req: HttpRequest,
    Path((file_id, additional, file_name)): Path<(String, String, String)>,
    data: Data<AppState>,
) -> impl Responder {
    let additional = parse_additional(&additional);

    let keystamp: Vec<&str> = additional.get("keystamp").map(|s| s.split('-').collect()).unwrap_or_default();
    let file_index = additional.get("fileindex").map(|s| s.as_str()).unwrap_or("");
    let xres = additional.get("xres").map(|s| s.as_str()).unwrap_or("");

    // keystamp check
    let time = keystamp.get(0).unwrap_or(&"");
    let hash = keystamp.get(1).unwrap_or(&"");
    let time_diff = &(data.rpc.get_timestemp() - time.parse::<i64>().unwrap_or_default());
    let hash_string = format!("{}-{}-{}-hotlinkthis", time, file_id, data.key);
    if time.is_empty() || hash.is_empty() || !TTL.contains(time_diff) || !string_to_hash(hash_string).starts_with(hash) {
        return forbidden();
    };

    if let Some(info) = CacheFileInfo::from_file_id(&file_id) {
        if let Some(file) = data.cache_manager
            .get_file(&info)
            .await
            .and_then(|f| NamedFile::from_file(f, &file_name).ok())
        {
            let cache_header = CacheControl(vec![CacheDirective::Public, CacheDirective::MaxAge(31536000)])
                .try_into_pair()
                .unwrap();
            let mut res = file
                .use_etag(false)
                .disable_content_disposition()
                .set_content_type(info.mime_type())
                .into_response(&req);
            res.headers_mut().insert(cache_header.0, cache_header.1); // TODO bandwidth limit
            return res;
        } else {
            let sources = data.rpc.sr_fetch(file_index, xres, &file_id).await;
            if sources.is_none() {
                return HttpResponse::NotFound().body("An error has occurred. (404)");
            }

            let temp_path = Arc::new(data.cache_manager.create_temp_file());
            let file_size = info.size() as u64;
            let (tx, mut rx) = watch::channel(0); // Download progress

            // Download worker
            let into2 = info.clone();
            let temp_path2 = temp_path.clone();
            data.runtime.clone().spawn(async move {
                let mut hasher = Sha1::new();
                let mut progress = 0;
                'source: for source in sources.unwrap() {
                    'retry: for _ in 0..3 {
                        let mut file = match OpenOptions::new().write(true).create(true).open(temp_path2.as_ref()).await {
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

                        let mut download = 0;
                        if let Ok(mut stream) = data
                            .reqwest
                            .get(&source)
                            .timeout(Duration::from_secs(300))
                            .send()
                            .await
                            .map(|r| r.bytes_stream())
                        {
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
                                let write_size = (download - progress) as usize;
                                let start = bytes.len() - write_size;
                                let data = &bytes[start..];
                                if let Err(err) = file.write_all(data).await {
                                    error!("Proxy temp file write fail: {}", err);
                                    continue 'retry;
                                }
                                hasher.update(data);
                                progress += write_size as u64;
                                let _ = tx.send(progress); // Ignore error
                            }
                            if progress == file_size {
                                if let Err(err) = file.flush().await {
                                    error!("Proxy temp file flush fail: {}", err);
                                    break 'source;
                                }
                                tx.closed().await;
                                let hash = hasher.finish();
                                if hash == into2.hash() {
                                    data.cache_manager.import_cache(&into2, &temp_path2).await;
                                } else {
                                    error!("Cache hash mismatch: expected: {:x?}, got: {:x?}", into2.hash(), hash);
                                }
                                break 'source;
                            }
                        }
                    }
                }
            });

            let mut builder = HttpResponse::Ok();
            builder.insert_header(ContentType(info.mime_type()));
            builder.insert_header(CacheControl(vec![CacheDirective::Public, CacheDirective::MaxAge(31536000)]));
            return builder.body(SizedStream::new(
                file_size,
                stream! { // TODO bandwidth limit
                    let mut file = File::open(temp_path.as_ref()).await.unwrap();
                    let mut read_off = 0;

                    'watch: while rx.changed().await.is_ok() {
                        let write_off = *rx.borrow();

                        while write_off > read_off {
                            let mut buffer = BytesMut::with_capacity(8*1024);
                            match file.read_buf(&mut buffer).await {
                                Ok(s) => read_off += s as u64,
                                Err(err) => yield Err(err)
                            }
                            yield Ok(buffer.freeze());

                            // EOF
                            if read_off == file_size {
                                break 'watch;
                            }
                        }
                    }
                },
            ));
        }
    }

    HttpResponse::NotFound().body("An error has occurred. (404)")
}
