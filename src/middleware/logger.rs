use std::{
    cmp::max,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::{Body, HttpBody},
    extract::Request,
    http::{header::CONTENT_LENGTH, HeaderValue, Method},
    response::Response,
};
use futures::future::BoxFuture;
use http_body::{Frame, SizeHint};
use log::info;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::server::ClientAddr;

#[derive(Clone)]
pub(super) struct Logger {
    counter: Arc<AtomicU64>,
}

impl Default for Logger {
    fn default() -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl<S> Layer<S> for Logger {
    type Service = LoggerMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LoggerMiddleware {
            counter: self.counter.clone(),
            service: inner,
        }
    }
}

#[derive(Clone)]
pub(super) struct LoggerMiddleware<S> {
    counter: Arc<AtomicU64>,
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
        let count = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let ip = req
            .extensions()
            .get::<ClientAddr>()
            .map(|i| i.ip().to_string())
            .unwrap_or_else(|| "-".into());
        let is_head = req.method() == Method::HEAD;
        let uri = req.uri();
        let request = if uri.query().is_none() {
            format!("{} {} {:?}", req.method(), uri.path(), req.version())
        } else {
            format!("{} {}?{} {:?}", req.method(), uri.path(), uri.query().unwrap(), req.version())
        };

        let fut = self.service.call(req);

        Box::pin(async move {
            let res = fut.await?;
            let code = res.status().as_u16();
            let mut size = 0;

            if !is_head {
                if let Some(Ok(i)) = res.headers().get(CONTENT_LENGTH).map(HeaderValue::to_str) {
                    size = i.parse::<u64>().unwrap_or_default();
                }
            }

            info!("{{{}/{:16} Code={} Byte={:<8} {}", count, ip.clone() + "}", code, size, request);
            Ok(res.map(|body| LoggerFinalizer {
                body,
                count,
                ip,
                code,
                size,
                start,
                body_start: Instant::now(),
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
            )
        }
    }
}

impl HttpBody for LoggerFinalizer {
    type Data = <Body as HttpBody>::Data;
    type Error = <Body as HttpBody>::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        this.body.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
