mod connection_counter;
mod logger;

use std::{sync::Arc, time::Duration};

use axum::{
    error_handling::HandleErrorLayer,
    http::{
        header::{CONNECTION, SERVER},
        HeaderValue, StatusCode,
    },
    middleware,
    response::Response,
    Router,
};
use once_cell::sync::Lazy;
use tower::{timeout::TimeoutLayer, ServiceBuilder};

use crate::{AppState, CLIENT_VERSION};

use self::connection_counter::ConnectionCounter;
use self::logger::Logger;

static SERVER_HEADER: Lazy<HeaderValue> =
    Lazy::new(|| HeaderValue::from_maybe_shared(format!("Genetic Lifeform and Distributed Open Server {CLIENT_VERSION}")).unwrap());

pub fn register_layer(router: Router<Arc<AppState>>, data: &AppState) -> Router<Arc<AppState>> {
    router
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_| async { StatusCode::SERVICE_UNAVAILABLE }))
                .layer(TimeoutLayer::new(Duration::from_secs(181))),
        )
        .layer(Logger::default())
        .layer(ConnectionCounter::new(data.rpc.settings(), data.command_channel.clone()))
        .layer(middleware::map_response(default_headers))
}

async fn default_headers<B>(mut response: Response<B>) -> Response<B> {
    let headers = response.headers_mut();
    headers.insert(SERVER, SERVER_HEADER.clone());
    headers.insert(CONNECTION, "close".try_into().unwrap());
    response
}
