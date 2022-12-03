use std::{
    future::{ready, Ready},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use actix_web::{
    body::{BodySize, MessageBody},
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    web::Bytes,
    Error,
};
use futures::future::LocalBoxFuture;
use pin_project_lite::pin_project;
use tokio::sync::mpsc::Sender;

use crate::{rpc::Settings, Command};

#[derive(Clone)]
pub struct ConnectionCounter {
    data: ConnectionCounterState,
}

#[derive(Clone)]
struct ConnectionCounterState {
    counter: Arc<AtomicU64>,
    settings: Arc<Settings>,
    command_channel: Sender<Command>,
}

impl ConnectionCounter {
    pub fn new(settings: Arc<Settings>, command_channel: Sender<Command>) -> Self {
        Self {
            data: ConnectionCounterState {
                counter: Arc::new(AtomicU64::new(0)),
                settings,
                command_channel,
            },
        }
    }
}

impl<S, B> Transform<S, ServiceRequest> for ConnectionCounter
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: MessageBody,
{
    type Response = ServiceResponse<ConnectionCounterFinalizer<B>>;
    type Error = Error;
    type InitError = ();
    type Transform = ConnectionCounterMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(ConnectionCounterMiddleware {
            data: self.data.clone(),
            service,
        }))
    }
}

pub struct ConnectionCounterMiddleware<S> {
    data: ConnectionCounterState,
    service: S,
}

impl<S, B> Service<ServiceRequest> for ConnectionCounterMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: MessageBody,
{
    type Response = ServiceResponse<ConnectionCounterFinalizer<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let counter = self.data.counter.clone();
        if counter.fetch_add(1, Ordering::Relaxed) > (self.data.settings.max_connection() as f64 * 0.8).ceil() as u64 {
            let _ = self.data.command_channel.try_send(Command::Overload);
        };

        let fut = self.service.call(req);

        Box::pin(async move {
            match fut.await {
                Ok(res) => Ok(res.map_body(|_, body| ConnectionCounterFinalizer { body, counter })),
                Err(err) => {
                    counter.fetch_sub(1, Ordering::Relaxed);
                    Err(err)
                }
            }
        })
    }
}

pin_project! {
    pub struct ConnectionCounterFinalizer<B> {
        #[pin]
        body: B,
        counter: Arc<AtomicU64>,
    }

    impl<B> PinnedDrop for ConnectionCounterFinalizer<B> {
        fn drop(this: Pin<&mut Self>) {
            this.counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl<B: MessageBody> MessageBody for ConnectionCounterFinalizer<B> {
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
