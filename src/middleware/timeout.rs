use std::time::Duration;

use actix_web::{
    body::MessageBody,
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    error::ErrorServiceUnavailable,
    Error,
};
use futures::future::{ready, LocalBoxFuture, Ready};
use tokio::time::timeout;

pub struct Timeout {
    timeout: Duration,
}

impl Timeout {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl<S, B> Transform<S, ServiceRequest> for Timeout
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: MessageBody,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = TimeoutMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(TimeoutMiddleware {
            service,
            timeout: self.timeout,
        }))
    }
}

pub struct TimeoutMiddleware<S> {
    service: S,
    timeout: Duration,
}

impl<S, B> Service<ServiceRequest> for TimeoutMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: MessageBody,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let duration = self.timeout;
        let fut = self.service.call(req);
        Box::pin(async move { timeout(duration, fut).await.map_err(ErrorServiceUnavailable)? })
    }
}
