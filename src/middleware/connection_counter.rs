use std::{
    pin::Pin,
    sync::{Arc, atomic::AtomicU64},
    task::{Context, Poll},
};

use axum::{
    body::{Body, HttpBody},
    extract::Request,
    response::Response,
};
use futures::future::BoxFuture;
use http_body::{Frame, SizeHint};
use pin_project_lite::pin_project;
use prometheus_client::metrics::gauge::Gauge;
use scopeguard::{ScopeGuard, guard};
use tokio::sync::mpsc::Sender;
use tower::{Layer, Service};

use crate::{Command, rpc::Settings};

#[derive(Clone)]
pub(super) struct ConnectionCounter {
    data: ConnectionCounterState,
}

#[derive(Clone)]
struct ConnectionCounterState {
    counter: Gauge<u64, AtomicU64>,
    settings: Arc<Settings>,
    command_channel: Sender<Command>,
}

impl ConnectionCounter {
    pub fn new(counter: Gauge<u64, AtomicU64>, settings: Arc<Settings>, command_channel: Sender<Command>) -> Self {
        Self {
            data: ConnectionCounterState {
                counter,
                settings,
                command_channel,
            },
        }
    }
}

impl<S> Layer<S> for ConnectionCounter {
    type Service = ConnectionCounterMiddleware<S>;

    fn layer(&self, service: S) -> Self::Service {
        ConnectionCounterMiddleware {
            data: self.data.clone(),
            service,
        }
    }
}

#[derive(Clone)]
pub(super) struct ConnectionCounterMiddleware<S> {
    data: ConnectionCounterState,
    service: S,
}

impl<S> Service<Request> for ConnectionCounterMiddleware<S>
where
    S: Service<Request, Response = Response> + Send + 'static,
    S::Future: Send + 'static,
{
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    type Response = Response<ConnectionCounterFinalizer>;

    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let counter = self.data.counter.clone();
        if counter.inc() > (self.data.settings.max_connection() as f64 * 0.8).ceil() as u64 {
            let _ = self.data.command_channel.try_send(Command::Overload);
        };
        let guard = guard(counter, move |counter| {
            counter.dec();
        });

        let fut = self.service.call(req);

        Box::pin(async move {
            let res = fut.await;
            let counter = ScopeGuard::into_inner(guard); // Cancel counter guard
            match res {
                Ok(res) => Ok(res.map(|body| ConnectionCounterFinalizer { body, counter })),
                Err(err) => {
                    counter.dec();
                    Err(err)
                }
            }
        })
    }
}

pin_project! {
    pub(super) struct ConnectionCounterFinalizer {
        #[pin]
        body: Body,
        counter: Gauge<u64, AtomicU64>,
    }

    impl PinnedDrop for ConnectionCounterFinalizer {
        fn drop(this: Pin<&mut Self>) {
            this.counter.dec();
        }
    }
}

impl HttpBody for ConnectionCounterFinalizer {
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
