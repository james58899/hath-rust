use std::{
    cmp::max,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::{Body, HttpBody},
    extract::Request,
    http::{HeaderValue, Method, Version, header::CONTENT_LENGTH},
    response::Response,
};
use futures::future::BoxFuture;
use http_body::{Frame, SizeHint};
use log::info;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::{
    metrics::{MethodLabel, Metrics, RequestLabel, StatusLabel},
    server::ClientAddr,
};

#[derive(Clone)]
pub(super) struct Logger {
    metrics: Arc<Metrics>,
    seq: Arc<AtomicU64>,
}

impl Logger {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Logger {
            metrics,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl<S> Layer<S> for Logger {
    type Service = LoggerMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LoggerMiddleware {
            metrics: self.metrics.clone(),
            seq: self.seq.clone(),
            service: inner,
        }
    }
}

#[derive(Clone)]
pub(super) struct LoggerMiddleware<S> {
    metrics: Arc<Metrics>,
    seq: Arc<AtomicU64>,
    service: S,
}

impl<S> Service<Request> for LoggerMiddleware<S>
where
    S: Service<Request, Response = Response> + Send + 'static,
    S::Future: Send + 'static,
{
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    type Response = Response<LoggerFinalizer>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let start = Instant::now();
        let method_label = if req.method() == Method::GET {
            MethodLabel::Get
        } else if req.method() == Method::HEAD {
            MethodLabel::Head
        } else {
            MethodLabel::Other
        };
        self.metrics.cache_sent.get_or_create(&RequestLabel { method: method_label }).inc();
        let count = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ip = req
            .extensions()
            .get::<ClientAddr>()
            .map(|i| i.ip().to_string())
            .unwrap_or_else(|| "-".into());
        let is_head = req.method() == Method::HEAD;
        let uri = req.uri();
        let version = req.version();
        let request = if uri.query().is_none() {
            format!("{} {} {:?}", req.method(), uri.path(), version)
        } else {
            format!("{} {}?{} {:?}", req.method(), uri.path(), uri.query().unwrap(), version)
        };

        let fut = self.service.call(req);

        let metrics = self.metrics.clone();
        Box::pin(async move {
            let res = fut.await?;
            let code = res.status().as_u16();
            metrics.cache_sent_status.get_or_create(&StatusLabel { code, method: method_label }).inc();
            let mut size = 0;

            if !is_head && let Some(Ok(i)) = res.headers().get(CONTENT_LENGTH).map(HeaderValue::to_str) {
                size = i.parse::<u64>().unwrap_or_default();
            }

            info!("{{{}/{:16} Code={} Byte={:<8} {}", count, ip.clone() + "}", code, size, request);
            Ok(res.map(|body| LoggerFinalizer {
                body,
                count,
                ip,
                code,
                size: 0,
                start,
                body_start: Instant::now(),
                version,
                is_get: method_label == MethodLabel::Get,
                metrics,
            }))
        })
    }
}

pin_project! {
    pub(super) struct LoggerFinalizer {
        #[pin]
        body: Body,
        count: u64,
        ip: String,
        code: u16,
        size: u64,
        start: std::time::Instant,
        body_start: std::time::Instant,
        version: Version,
        is_get: bool,
        metrics: Arc<Metrics>,
    }

    impl PinnedDrop for LoggerFinalizer {
        fn drop(this: Pin<&mut Self>) {
            info!("{{{}/{:16} Code={} Byte={:<8} Finished processing request in {}ms ({:.2} KB/s)",
                this.count,
                this.ip.clone() + "}",
                this.code,
                this.size,
                this.start.elapsed().as_millis(),
                this.size as f64 / max(this.body_start.elapsed().as_millis(), 1) as f64
            );
            // TODO replace label with enum
            this.metrics.cache_sent_size.get_or_create(&vec![("version".to_owned(), format!("{:?}", this.version))]).inc_by(this.size);
            this.metrics.cache_sent_duration.get_or_create(&vec![("version".to_owned(), format!("{:?}", this.version))]).observe(this.start.elapsed().as_secs_f64());
            if this.is_get {
                this.metrics.cache_response_size.observe(this.size as f64);
            }
        }
    }
}

impl HttpBody for LoggerFinalizer {
    type Data = <Body as HttpBody>::Data;
    type Error = <Body as HttpBody>::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let frame = this.body.poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &frame {
            frame.data_ref().inspect(|b| *this.size += b.len() as u64);
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
