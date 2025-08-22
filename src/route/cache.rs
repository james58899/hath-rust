use std::{
    io::SeekFrom,
    ops::RangeInclusive,
    sync::Arc,
    time::{Duration, Instant},
};

use async_stream::stream;
use aws_lc_rs::digest;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{
        HeaderValue,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use bytes::BytesMut;
use futures::StreamExt;
use log::error;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::watch,
    time::{sleep, timeout},
};

use crate::{
    AppState,
    cache_manager::CacheFileInfo,
    route::{forbidden, not_found, parse_additional},
    util::{create_http_client, string_to_hash},
};

const TTL: RangeInclusive<i64> = -900..=900; // Token TTL 15 minutes
const CACHE_HEADER: HeaderValue = HeaderValue::from_static("public, max-age=31536000");
const DEFAULT_CD: HeaderValue = HeaderValue::from_static("inline");

pub(super) async fn hath(
    Path((file_id, additional, file_name)): Path<(String, String, String)>,
    data: State<Arc<AppState>>,
) -> impl IntoResponse {
    let additional = parse_additional(&additional);
    let mut keystamp = additional.get("keystamp").unwrap_or(&"").split('-');
    let file_index = additional.get("fileindex").unwrap_or(&"");
    let xres = additional.get("xres").unwrap_or(&"");

    // keystamp check
    let time = keystamp.next().unwrap_or_default();
    let hash = keystamp.next().unwrap_or_default();
    let time_diff = &(data.rpc.get_timestemp() - time.parse::<i64>().unwrap_or_default());
    let hash_string = format!("{}-{}-{}-hotlinkthis", time, file_id, data.rpc.key());
    if time.is_empty() || hash.is_empty() || !TTL.contains(time_diff) || !string_to_hash(hash_string).starts_with(hash) {
        return forbidden();
    };

    // Parse file info
    let info = match CacheFileInfo::from_file_id(&file_id) {
        Some(info) => info,
        None => return not_found(),
    };
    let file_size = info.size() as u64;
    let content_type = HeaderValue::from_maybe_shared(info.mime_type().to_string()).unwrap();
    let content_disposition = HeaderValue::from_maybe_shared(format!("inline; filename=\"{file_name}\"")).unwrap_or(DEFAULT_CD);

    // Check cache hit
    if let Some(file) = data.cache_manager.get_file(&info).await {
        return Response::builder()
            .header(CACHE_CONTROL, CACHE_HEADER)
            .header(CONTENT_LENGTH, file_size)
            .header(CONTENT_TYPE, content_type)
            .header(CONTENT_DISPOSITION, content_disposition)
            .body(Body::from_stream(file))
            .unwrap();
    }

    // Cache miss, proxy request
    // Check if the file is already downloading
    let (temp_tx, temp_rx) = watch::channel(None); // Tempfile
    let tx = Arc::new(watch::channel(0).0); // Download progress
    let state;
    {
        let mut download_state = data.download_state.lock();
        state = download_state.get(&info.hash()).cloned();
        // Tracking download progress
        if state.is_none() {
            download_state.insert(info.hash(), (temp_rx.clone(), tx.clone()));
        }
    }

    let (temp_path, mut rx) = if let Some((mut tempfile, progress)) = state {
        let tempfile = tempfile.wait_for(Option::is_some).await;
        if let Err(err) = tempfile {
            error!("Waiting tempfile create error: {}", err);
            data.download_state.lock().remove(&info.hash());
            return not_found();
        }
        (tempfile.unwrap().as_ref().unwrap().clone(), progress.subscribe())
    } else {
        // Make sure the state will be removed when cancellation.
        let data2 = data.clone();
        let state_guard = scopeguard::guard(info.hash(), move |hash| {
            data2.download_state.lock().remove(&hash);
        });

        let temp_path = Arc::new(data.cache_manager.create_temp_file().await);
        temp_tx.send_replace(Some(temp_path.clone()));

        let sources = match data.rpc.sr_fetch(file_index, xres, &file_id).await {
            Some(v) => v,
            None => return not_found(),
        };

        // Download worker
        let tx2: Arc<watch::Sender<u64>> = tx.clone();
        let info2 = info.clone();
        let temp_path2 = temp_path.clone();
        let metrics = data.metrics.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            let mut hasher = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);
            let mut progress = 0;
            let mut reqwest = data.reqwest.clone();
            let mut sources = sources.iter().cycle();
            'retry: for retry in 0..3 {
                let mut file = match OpenOptions::new().write(true).create(true).truncate(false).open(&*temp_path2).await {
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
                    error!("Cache download fail: url={}, err={}", source, err);

                    // Disable proxy on third retry
                    if retry == 1 && data.has_proxy {
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
                                error!("Proxy download fail: url={}, err={}", source, err);
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
                        metrics.cache_received_size.inc_by(write_size as u64);
                        tx2.send_replace(progress);
                    }
                    if progress == file_size {
                        if let Err(err) = file.flush().await {
                            error!("Proxy temp file flush fail: {}", err);
                            break 'retry;
                        }
                        let hash = hasher.finish();
                        let duration = start.elapsed();
                        tx2.send_replace(progress);
                        tx2.closed().await; // Wait all request done
                        data.download_state.lock().remove(&info2.hash());
                        if hash.as_ref() == info2.hash() {
                            tx2.closed().await; // Wait again to avoid race conditions
                            data.cache_manager.import_cache(&info2, &temp_path2).await;
                            metrics.cache_received.inc();
                            metrics.cache_received_duration.observe(duration.as_secs_f64());
                        } else {
                            error!("Cache hash mismatch: expected: {:x?}, got: {:x?}", info2.hash(), hash);
                        }
                        return;
                    }
                }
            }

            // Try remove from download state anyway
            drop(state_guard);
        });

        (temp_path, tx.subscribe())
    };

    // Wait download start or 404
    if *rx.borrow() == 0 && rx.changed().await.is_err() {
        return not_found();
    }

    Response::builder()
        .header(CACHE_CONTROL, CACHE_HEADER)
        .header(CONTENT_LENGTH, file_size)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_DISPOSITION, content_disposition)
        .body(Body::from_stream(stream! {
            let mut file = File::open(temp_path.as_ref()).await.unwrap();
            let mut read_off = 0;
            let mut write_off = *rx.borrow();

            let wait_time = Duration::from_secs(30);
            'watch: while write_off > read_off || timeout(wait_time, rx.changed()).await.is_ok_and(|r| r.is_ok()) {
                write_off = *rx.borrow();

                let mut buffer = BytesMut::with_capacity(64*1024); // 64 KiB
                while write_off > read_off {
                    buffer.reserve(64*1024);
                    match file.read_buf(&mut buffer).await {
                        Ok(0) => {
                            // Data may not synced to disk, retry later
                            sleep(Duration::from_millis(10)).await;
                            continue;
                        }
                        Ok(s) => read_off += s as u64,
                        Err(err) => yield Err(err)
                    }
                    yield Ok(buffer.split().freeze());

                    // EOF
                    if read_off == file_size {
                        break 'watch;
                    }
                }
            }
        }))
        .unwrap()
}
