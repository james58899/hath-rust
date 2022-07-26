use std::{
    cmp::max,
    future::{ready, Ready},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Instant,
};

use actix_web::{
    body::{
        BodySize::{self, Sized},
        MessageBody,
    },
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    http::Method,
    web::Bytes,
    Error,
};
use futures::future::LocalBoxFuture;
use log::info;
use pin_project_lite::pin_project;

#[derive(Clone)]
pub struct Logger {
    counter: Arc<AtomicU64>,
}

impl Default for Logger {
    fn default() -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl<S, B> Transform<S, ServiceRequest> for Logger
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: MessageBody,
{
    type Response = ServiceResponse<LoggerFinalizer<B>>;
    type Error = Error;
    type Transform = LoggerMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(LoggerMiddleware {
            counter: self.counter.clone(),
            service,
        }))
    }
}

pub struct LoggerMiddleware<S> {
    counter: Arc<AtomicU64>,
    service: S,
}

impl<S, B> Service<ServiceRequest> for LoggerMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: MessageBody,
{
    type Response = ServiceResponse<LoggerFinalizer<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let start = Instant::now();
        let count = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let ip = req.connection_info().peer_addr().unwrap_or("-").to_string();
        let is_head = req.method() == Method::HEAD;
        let request = if req.query_string().is_empty() {
            format!("{} {} {:?}", req.method(), req.path(), req.version())
        } else {
            format!("{} {}?{} {:?}", req.method(), req.path(), req.query_string(), req.version())
        };
        let fut = self.service.call(req);

        Box::pin(async move {
            let res = fut.await?;
            let code = res.response().status().as_u16();
            let mut size = 0;

            if !is_head {
                if let Sized(i) = res.response().body().size() {
                    size = i;
                }
            }

            info!("{{{}/{:16} Code={} Byte={:<8} {}", count, ip.clone() + "}", code, size, request);
            Ok(res.map_body(|_, body| LoggerFinalizer {
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

    forward_ready!(service);
}

pin_project! {
    pub struct LoggerFinalizer<B> {
        #[pin]
        body: B,
        count: u64,
        ip: String,
        code: u16,
        size: u64,
        start: std::time::Instant,
        body_start: std::time::Instant,
    }

    impl<B> PinnedDrop for LoggerFinalizer<B> {
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

impl<B: MessageBody> MessageBody for LoggerFinalizer<B> {
    type Error = B::Error;

    #[inline]
    fn size(&self) -> BodySize {
        self.body.size()
    }

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Self::Error>>> {
        let this = self.project();
        this.body.poll_next(cx)
    }
}
