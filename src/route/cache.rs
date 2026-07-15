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
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{
            ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH, IF_RANGE,
            RANGE,
        },
    },
    response::{IntoResponse, Response},
};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use http_range_header::parse_range_header;
use log::error;
use prometheus_client::metrics::counter::Counter;
use reqwest::IntoUrl;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::watch,
    time::{sleep, timeout},
};

use crate::{
    AppState,
    cache_manager::{CacheFileInfo, CacheFileResult},
    metrics::{LABEL_CACHE_DOWNLOAD, LABEL_CACHE_FETCH_URL},
    ratelimit::{GrantResult, Permit, ReqInfo, SchedulerHandle, SlowDrainPermit},
    route::{forbidden, not_found, parse_additional},
    server::util::{ResetConnection, TruncateBody},
    util::{create_http_client, string_to_hash},
};

const TTL: RangeInclusive<i64> = -900..=900; // Token TTL 15 minutes
const CACHE_HEADER: HeaderValue = HeaderValue::from_static("public, max-age=31536000, immutable");
const DEFAULT_CD: HeaderValue = HeaderValue::from_static("inline");

enum TransferPermit {
    Normal { _permit: Permit },
    SlowDrain(SlowDrainPermit),
}

impl TransferPermit {
    fn is_normal(&self) -> bool {
        matches!(self, Self::Normal { .. })
    }

    fn slow_drain(&self) -> Option<&SlowDrainPermit> {
        match self {
            Self::Normal { .. } => None,
            Self::SlowDrain(permit) => Some(permit),
        }
    }
}

pub(super) async fn hath(
    Path((file_id, additional, file_name)): Path<(String, String, String)>,
    method: Method,
    headers: HeaderMap,
    data: State<Arc<AppState>>,
) -> impl IntoResponse {
    let recv_time = Instant::now();
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

    // ETag
    // Browser only send range request if have etag
    let etag = {
        let mut builder = String::with_capacity(10);
        builder.push('"');
        builder.push_str(&const_hex::encode(&info.hash()[..4])); // First 8 hex
        builder.push('"');
        builder
    };
    // Handle If-None-Match
    if headers.get(IF_NONE_MATCH).is_some_and(|h| h.to_str().is_ok_and(|s| s == etag)) {
        // 304 Not Modified
        return Response::builder()
            .header(CACHE_CONTROL, CACHE_HEADER)
            .header(CONTENT_LENGTH, file_size) // rfc9110 section 8.6
            .header(ETAG, etag)
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap();
    }

    // Range request
    let mut range = headers
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_range_header(h).ok())
        .and_then(|r| r.validate(file_size).ok())
        .filter(|r| r.len() == 1)
        .and_then(|r| r.into_iter().next());
    // Check If-Range
    if headers.get(IF_RANGE).is_some_and(|h| h.to_str().is_ok_and(|s| s != etag)) {
        range.take(); // etag not match, ignore range request
    }

    // Build response
    let response = Response::builder()
        .header(CACHE_CONTROL, CACHE_HEADER)
        .header(CONTENT_DISPOSITION, content_disposition)
        .header(CONTENT_TYPE, content_type)
        .header(ETAG, etag);

    // A request becomes eligible for scheduling once its response size is known
    // and its data is ready to stream. At that point the response *head* is built
    // and returned immediately; the wait for scheduler capacity happens inside the
    // response *body* (see `build_body`). Headers are therefore delivered as soon
    // as possible, and only the body blocks on available resources.
    //
    // The head-producing phase below is bounded by the serve timeout (a cache miss
    // does network work here); exceeding it resets the connection *before* any head
    // is sent. Once the head is built, the in-body wait is bounded by the remaining
    // serve timeout only until either a normal or slow-drain grant arrives.
    //
    // Clone shared handles here because `data` is moved into the download worker on
    // a cache miss, so it is not available after that point.
    let scheduler = data.scheduler.clone();
    let timeout_metric = data.metrics.serve_timeout.clone();
    let body_timeout_metric = timeout_metric.clone();
    let serve_timeout = data.serve_timeout;
    // Everything except HEAD carries a body and is scheduled; HEAD serves no body
    // and bypasses the rate limiters entirely.
    let schedule_body = method != Method::HEAD;
    let remaining = serve_timeout.saturating_sub(recv_time.elapsed());

    let served = timeout(remaining, async move {
        // Check cache hit
        let file_stream = match data.cache_manager.get_file(&info, range.as_ref().map(|r| *r.start())).await {
            CacheFileResult::Full(stream) => {
                if range.as_ref().is_some_and(|r| *r.start() != 0) {
                    // File seek fail, reject range request
                    range.take();
                }
                Some(stream)
            }
            CacheFileResult::Partial(stream) => Some(stream),
            CacheFileResult::NotFound => None,
        };

        if let Some(stream) = file_stream {
            // Cache hit: data is available immediately. The response size charged is
            // what we will actually send (range length, else the full file).
            let resp_bytes = match range.as_ref() {
                Some(r) => (r.end() - r.start() + 1) as u32,
                None => info.size(),
            };
            let body = build_body(
                schedule_body,
                scheduler,
                ReqInfo { size: resp_bytes },
                recv_time,
                serve_timeout,
                body_timeout_metric,
                stream,
            );
            let response = if let Some(r) = range {
                let content_length = r.end() - r.start() + 1;
                let content_range = format!("bytes {}-{}/{}", r.start(), r.end(), file_size);
                response
                    .header(ACCEPT_RANGES, "bytes")
                    .header(CONTENT_LENGTH, content_length)
                    .header(CONTENT_RANGE, content_range)
                    .status(StatusCode::PARTIAL_CONTENT)
                    .body(Body::new(TruncateBody::new(body, content_length)))
                    .unwrap()
            } else {
                response
                    .header(ACCEPT_RANGES, "bytes")
                    .header(CONTENT_LENGTH, file_size)
                    .body(body)
                    .unwrap()
            };
            return response;
        }

        // Cache miss, proxy request
        // Check if the file is already downloading
        let (temp_tx, temp_rx) = watch::channel(None); // Tempfile
        let tx = Arc::new(watch::channel(0).0); // Download progress
        let state;
        let refill_permit;
        {
            let mut download_state = data.download_state.lock();
            state = download_state.get(&info.hash()).cloned();
            // We only initiate a refill when no download is in flight for this file.
            // Reserve a refill slot first; if we are at capacity, skip the refill
            // entirely and behave as if the serve timed out.
            if state.is_none() {
                match data.refill_limiter.try_acquire() {
                    Some(permit) => refill_permit = Some(permit),
                    None => return reset_connection(),
                }
                download_state.insert(info.hash(), (temp_rx.clone(), tx.clone()));
            } else {
                refill_permit = None;
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

            let metrics = data.metrics.clone();
            let fetch_time = Instant::now();
            let sources = match data.rpc.sr_fetch(file_index, xres, &file_id).await {
                Some(v) => v,
                None => return not_found(),
            };
            metrics
                .cache_received_duration
                .get_or_create(&LABEL_CACHE_FETCH_URL)
                .observe(fetch_time.elapsed().as_secs_f64());

            // Download worker
            let tx2: Arc<watch::Sender<u64>> = tx.clone();
            let info2 = info.clone();
            let temp_path2 = temp_path.clone();
            tokio::spawn(async move {
                let _refill_permit = refill_permit; // released when refill finishes
                let download_time = Instant::now();
                let mut hasher = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);
                let mut progress = 0;
                let mut reqwest = data.reqwest.clone();
                let mut sources = sources.iter().cycle();
                let mut use_proxy = data.has_proxy;
                'retry: for retry in 0..3 {
                    let mut file = match OpenOptions::new().write(true).create(true).truncate(false).open(&*temp_path2).await {
                        Ok(mut f) => {
                            if let Err(err) = f.seek(SeekFrom::Start(progress)).await {
                                error!("Cache tempfile seek fail: {}", err);
                                continue;
                            }
                            f
                        }
                        Err(err) => {
                            error!("Cache tempfile create fail: {}", err);
                            continue;
                        }
                    };

                    // Send request
                    let source = sources.next().unwrap();
                    let request = reqwest.get(source).send().await;
                    let mut stream = match request.and_then(|r| r.error_for_status()) {
                        Ok(r) => r.bytes_stream(),
                        Err(mut err) => {
                            err = err_with_url(err, source);
                            error!("Cache download request fail: {err:?}");

                            // Disable proxy on connect error and third retry
                            if use_proxy && (err.is_connect() || retry == 1) {
                                reqwest = create_http_client(Duration::from_secs(30), None, data.local_addr.clone());
                                use_proxy = false;
                            }

                            continue;
                        }
                    };

                    // Start download
                    let mut download = 0;
                    while let Some(bytes) = stream.next().await {
                        let bytes = match bytes {
                            Ok(it) => it,
                            Err(mut err) => {
                                err = err_with_url(err, source);
                                error!("Cache download fail: {err:?}");
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
                            error!("Cache tempfile write fail: {}", err);
                            break 'retry;
                        }
                        hasher.update(data);
                        progress += write_size as u64;
                        metrics.cache_received_size.inc_by(write_size as u64);
                        tx2.send_replace(progress);
                    }
                    if progress == file_size {
                        if let Err(err) = file.flush().await {
                            error!("Cache tempfile flush fail: {}", err);
                            break 'retry;
                        }
                        let hash = hasher.finish();
                        let duration = download_time.elapsed();
                        tx2.send_replace(progress);
                        let _ = file.sync_all().await; // fsync
                        drop(file); // Close file
                        tx2.closed().await; // Wait all request done
                        data.download_state.lock().remove(&info2.hash());
                        if hash.as_ref() == info2.hash() {
                            tx2.closed().await; // Wait again to avoid race conditions
                            data.cache_manager.import_cache(&info2, &temp_path2).await;
                            metrics.cache_received.inc();
                            metrics
                                .cache_received_duration
                                .get_or_create(&LABEL_CACHE_DOWNLOAD)
                                .observe(duration.as_secs_f64());
                        } else {
                            error!("Cache tempfile hash mismatch: expected: {:x?}, got: {:x?}", info2.hash(), hash);
                        }
                        return;
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

        // Refill has started: data is now available to stream, so scheduling can
        // proceed inside the body. A miss does not honor the range — the whole file
        // is streamed — so the charged size is the full file.
        let inner = stream! {
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
        };
        response
            .header(CONTENT_LENGTH, file_size)
            .body(build_body(
                schedule_body,
                scheduler,
                ReqInfo { size: info.size() },
                recv_time,
                serve_timeout,
                body_timeout_metric,
                inner,
            ))
            .unwrap()
    })
    .await;

    match served {
        Ok(response) => response,
        Err(_) => {
            // Timed out during the head-producing phase (a cache miss doing network
            // work), before any head was sent. Reset the connection.
            timeout_metric.inc();
            reset_connection()
        }
    }
}

/// Build the response body for a ready-to-serve request.
///
/// HEAD requests carry no body and are not scheduled: they return immediately with
/// an empty body, bypassing the rate limiters.
///
/// For every other method the returned body simultaneously requests a normal
/// scheduler grant and a slow-drain concurrency grant. The serve timeout applies
/// only until either grant arrives. A slow-draining response remains queued for a
/// normal grant and upgrades immediately when one arrives, releasing its
/// slow-drain slot. All queue tickets and permits live in the stream so dropping
/// the response releases every resource it still owns.
fn build_body<S>(
    schedule: bool,
    scheduler: Arc<SchedulerHandle>,
    info: ReqInfo,
    recv_time: Instant,
    serve_timeout: Duration,
    timeout_metric: Counter,
    inner: S,
) -> Body
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    if !schedule {
        return Body::empty();
    }
    Body::from_stream(stream! {
        let (_ticket, grant) = scheduler.enqueue(info);
        let slow_drain = scheduler.slow_drain();
        let mut normal_grant = Box::pin(async move {
            match grant.await {
                Ok(GrantResult::Granted(permit)) => Some(permit),
                Ok(GrantResult::Evicted) | Err(_) => None,
            }
        });
        let mut normal_pending = true;

        let mut transfer_permit = {
            let mut slow_grant = Box::pin(slow_drain.acquire());
            let remaining = serve_timeout.saturating_sub(recv_time.elapsed());
            let mut deadline = Box::pin(sleep(remaining));

            loop {
                tokio::select! {
                    biased;
                    permit = &mut normal_grant, if normal_pending => {
                        normal_pending = false;
                        if let Some(permit) = permit {
                            break TransferPermit::Normal { _permit: permit };
                        }
                    }
                    permit = &mut slow_grant => break TransferPermit::SlowDrain(permit),
                    _ = &mut deadline => {
                        // No admission grant arrived before the serve timeout. The
                        // response head is already on the wire, so reset the stream.
                        timeout_metric.inc();
                        yield Err::<Bytes, _>(reset_error());
                        return;
                    }
                }
            }
        };

        let mut inner = std::pin::pin!(inner);
        loop {
            // Poll the normal grant while waiting for disk/download data so an
            // upgrade promptly returns the slow-drain slot.
            let item = loop {
                if transfer_permit.is_normal() || !normal_pending {
                    break inner.next().await;
                }
                tokio::select! {
                    biased;
                    permit = &mut normal_grant => {
                        normal_pending = false;
                        if let Some(permit) = permit {
                            transfer_permit = TransferPermit::Normal { _permit: permit };
                        }
                    }
                    item = inner.next() => break item,
                }
            };

            let Some(item) = item else {
                return;
            };
            match item {
                Ok(mut bytes) => while !bytes.is_empty() {
                    let Some(slow) = transfer_permit.slow_drain() else {
                        yield Ok(bytes);
                        break;
                    };
                    let chunk_len = slow.chunk_len(bytes.len());
                    let delay = slow.delay_for(chunk_len);
                    let chunk = bytes.split_to(chunk_len);

                    if normal_pending {
                        let mut pacing = Box::pin(sleep(delay));
                        tokio::select! {
                            biased;
                            permit = &mut normal_grant => {
                                normal_pending = false;
                                if let Some(permit) = permit {
                                    transfer_permit = TransferPermit::Normal { _permit: permit };
                                } else {
                                    pacing.await;
                                }
                            }
                            _ = &mut pacing => {}
                        }
                    } else {
                        sleep(delay).await;
                    }
                    yield Ok(chunk);
                },
                Err(err) => {
                    yield Err(err);
                    return;
                }
            }
        }
    })
}

/// Error used to abort a response body so the connection/stream is reset rather
/// than finishing normally.
fn reset_error() -> std::io::Error {
    std::io::Error::other("serve reset")
}

/// Build a response that resets the connection instead of sending a complete
/// reply. Used for pre-body rejections (serve timeout during the head-producing
/// phase, refill at capacity): the body errors on its first poll so hyper tears
/// the connection down before writing any head (HTTP/1.1), and the [`ResetConnection`]
/// extension tells the HTTP/3 server loop to reset the stream before sending a
/// response. No status code is delivered in either case.
fn reset_connection() -> Response {
    let mut response = Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(Body::from_stream(stream! {
            yield Err::<Bytes, _>(reset_error());
        }))
        .unwrap();
    response.extensions_mut().insert(ResetConnection);
    response
}

/// Helper function to add url to reqwest error
///
/// Some kind of reqwest error doesn't have url, so add it manually
fn err_with_url<T: IntoUrl>(error: reqwest::Error, url: T) -> reqwest::Error {
    if error.url().is_none()
        && let Ok(url) = url.into_url()
    {
        error.with_url(url)
    } else {
        error
    }
}
