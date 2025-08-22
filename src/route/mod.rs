use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{
        Response, StatusCode,
        header::{CONTENT_TYPE, LOCATION},
    },
    response::{Html, IntoResponse},
    routing::get,
};
use prometheus_client::encoding::text::encode;

use crate::{
    AppState,
    route::{cache::hath, server_command::servercmd, speed_test::speedtest},
};

mod cache;
mod server_command;
mod speed_test;

pub fn register_route(router: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    router
        .route("/favicon.ico", get(favicon).head(favicon))
        .route("/robots.txt", get(robots).head(robots))
        .route("/servercmd/{command}/{additional}/{time}/{key}", get(servercmd).head(servercmd))
        .route("/t/{size}/{time}/{hash}/{random}", get(speedtest).head(speedtest))
        .route("/h/{fileid}/{additional}/{*filename}", get(hath).head(hath))
        .fallback(get(default).head(default))
}

pub async fn default() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Html("An error has occurred. (404)"))
}

pub async fn metrics(data: State<Arc<AppState>>) -> impl IntoResponse {
    let mut buffer = String::new();
    encode(&mut buffer, &data.metrics.registry).unwrap();

    Response::builder()
        .header(CONTENT_TYPE, "application/openmetrics-text; version=1.0.0; charset=utf-8")
        .body(Body::from(buffer))
        .unwrap()
}

async fn favicon() -> impl IntoResponse {
    (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "https://e-hentai.org/favicon.ico")])
}

async fn robots() -> impl IntoResponse {
    "User-agent: *\nDisallow: /"
}

fn forbidden() -> Response<Body> {
    (StatusCode::FORBIDDEN, Html("An error has occurred. (403)")).into_response()
}

fn not_found() -> Response<Body> {
    (StatusCode::NOT_FOUND, "An error has occurred. (404)").into_response()
}

fn parse_additional(additional: &str) -> HashMap<&str, &str> {
    let mut map = HashMap::new();
    for kv in additional.split(';') {
        let mut pair = kv.split('=');
        let k = pair.next();
        let v = pair.next();
        if k.is_none() || v.is_none() {
            continue;
        }

        map.insert(k.unwrap(), v.unwrap());
    }

    map
}
