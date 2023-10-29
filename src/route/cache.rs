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
use futures::{FutureExt, StreamExt};
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
    util::{create_http_client, string_to_hash},
    AppState,
};

static TTL: RangeInclusive<i64> = -900..=900; // Token TTL 15 minutes

#[route("/h/{fileid}/{additional}/{filename:.*}", method = "GET", method = "HEAD")]
async fn hath(
    req: HttpRequest,
    Path((file_id, additional, _file_name)): Path<(String, String, String)>,
    data: Data<AppState>,
) -> impl Responder {
    let additional = parse_additional(&additional);

    let mut keystamp = additional.get("keystamp").map(String::as_str).unwrap_or_default().split('-');
    let file_index = additional.get("fileindex").map(String::as_str).unwrap_or_default();
    let xres = additional.get("xres").map(String::as_str).unwrap_or_default();

    // keystamp check
    let time = keystamp.next().unwrap_or_default();
    let hash = keystamp.next().unwrap_or_default();
    let time_diff = &(data.rpc.get_timestemp() - time.parse::<i64>().unwrap_or_default());
    let hash_string = format!("{}-{}-{}-hotlinkthis", time, file_id, data.rpc.key());
    if time.is_empty() || hash.is_empty() || !TTL.contains(time_diff) || !string_to_hash(hash_string).starts_with(hash) {
        return forbidden();
    };

    // Check cache hit
    let info = match CacheFileInfo::from_file_id(&file_id) {
        Some(info) => info,
        None => return HttpResponse::NotFound().body("An error has occurred. (404)"),
    };
    if let Some(file) = data
        .cache_manager
        .get_file(&info)
        .then(|f| async move { tokio::task::spawn_blocking(|| NamedFile::open(f?).ok()).await.ok().flatten() })
        .await
    {
        let cache_header = CacheControl(vec![CacheDirective::Public, CacheDirective::MaxAge(31536000)])
            .try_into_pair()
            .unwrap();
        let mut res = file
            .use_etag(false)
            .disable_content_disposition()
            .set_content_type(info.mime_type())
            .into_response(&req);
        res.headers_mut().insert(cache_header.0, cache_header.1);
        return res;
    }

    // Cache miss, proxy request
    let file_size = info.size() as u64;

    // Check if the file is already downloading
    let download_state = data.download_state.read().get(&info.hash()).cloned();
    let (temp_path, mut rx) = if let Some(state) = download_state {
        state
    } else {
        let temp_path = Arc::new(data.cache_manager.create_temp_file().await);
        let (tx, rx) = watch::channel(0); // Download progress

        // Tracking download progress
        data.download_state.write().insert(info.hash(), (temp_path.clone(), rx.clone()));

        let sources = match data.rpc.sr_fetch(file_index, xres, &file_id).await {
            Some(v) => v,
            None => {
                data.download_state.write().remove(&info.hash());
                return HttpResponse::NotFound().body("An error has occurred. (404)");
            }
        };

        // Download worker
        let info2 = info.clone();
        let temp_path2 = temp_path.clone();
        data.runtime.clone().spawn(async move {
            let mut hasher = Sha1::new();
            let mut progress = 0;
            let mut reqwest = data.reqwest.clone();
            let mut sources = sources.iter().cycle();
            'retry: for retry in 0..3 {
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

                // Send request
                let source = sources.next().unwrap();
                let request = reqwest.get(source).send().await;
                if let Err(ref err) = request {
                    error!("Cache download fail: url={}, error={}", source, err);

                    // Disable proxy on third retry
                    if retry == 1 && data.has_proxy && err.is_connect() {
                        reqwest = create_http_client(Duration::from_secs(30), None);
                    }
                };

                // Start download
                let mut download = 0;
                if let Ok(mut stream) = request.and_then(|r| r.error_for_status()).map(|r| r.bytes_stream()) {
                    while let Some(bytes) = stream.next().await {
                        let bytes = match &bytes {
                            Ok(it) => it,
                            Err(err) => {
                                error!("Proxy download fail: url={}, error{}", source, err);
                                continue 'retry;
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
                            break 'retry;
                        }
                        hasher.update(data);
                        progress += write_size as u64;
                        tx.send_replace(progress);
                    }
                    if progress == file_size {
                        if let Err(err) = file.flush().await {
                            error!("Proxy temp file flush fail: {}", err);
                            break 'retry;
                        }
                        let hash = hasher.finish();
                        data.download_state.write().remove(&info2.hash());
                        tx.send_replace(progress);
                        if hash == info2.hash() {
                            tx.closed().await; // Wait all request done
                            data.cache_manager.import_cache(&info2, &temp_path2).await;
                        } else {
                            error!("Cache hash mismatch: expected: {:x?}, got: {:x?}", info2.hash(), hash);
                        }
                        return;
                    }
                }
            }

            // Try remove from download state anyway
            data.download_state.write().remove(&info2.hash());
        });

        (temp_path, rx)
    };

    // Wait download start or 404
    if *rx.borrow() == 0 && rx.changed().await.is_err() {
        return HttpResponse::NotFound().body("An error has occurred. (404)");
    }

    let mut builder = HttpResponse::Ok();
    builder.insert_header(ContentType(info.mime_type()));
    builder.insert_header(CacheControl(vec![CacheDirective::Public, CacheDirective::MaxAge(31536000)]));
    builder.body(SizedStream::new(
        file_size,
        stream! {
            let mut file = File::open(temp_path.as_ref()).await.unwrap();
            let mut read_off = 0;
            let mut write_off = *rx.borrow();

            'watch: while rx.changed().await.is_ok() || write_off != read_off {
                write_off = *rx.borrow();

                while write_off > read_off {
                    let mut buffer = BytesMut::with_capacity(64*1024); // 64 KiB
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
    ))
}
