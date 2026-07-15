mod connection_counter;
mod logger;

use std::sync::Arc;

use axum::{
    Router,
    http::{HeaderValue, header::SERVER},
    middleware,
    response::Response,
};
use const_format::concatcp;

use crate::{
    AppState, CLIENT_VERSION,
    middleware::{connection_counter::ConnectionCounter, logger::Logger},
};

static SERVER_HEADER: HeaderValue = HeaderValue::from_static(concatcp!("Genetic Lifeform and Distributed Open Server ", CLIENT_VERSION));

pub fn register_layer(router: Router<Arc<AppState>>, data: &AppState, server_header: bool) -> Router<Arc<AppState>> {
    let mut router = router
        .layer(Logger::new(data.metrics.clone()))
        .layer(ConnectionCounter::new(data.metrics.connections.clone()));

    if server_header {
        router = router.layer(middleware::map_response(default_headers));
    }

    router
}

async fn default_headers<B>(mut response: Response<B>) -> Response<B> {
    let headers = response.headers_mut();
    headers.insert(SERVER, SERVER_HEADER.clone());
    response
}
