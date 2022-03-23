use std::collections::HashMap;

use actix_web::{http::header::ContentType, route, web::ServiceConfig, HttpResponse, Responder};
use reqwest::header::LOCATION;

use self::{cache::hath, server_command::servercmd, speed_test::speedtest};

mod cache;
mod server_command;
mod speed_test;

pub fn configure(cfg: &mut ServiceConfig) {
    cfg.service(favicon)
        .service(robots)
        .service(servercmd)
        .service(hath)
        .service(speedtest);
}

pub async fn default() -> impl Responder {
    HttpResponse::NotFound()
        .content_type(ContentType::html())
        .body("An error has occurred. (404)")
}

#[route("/favicon.ico", method = "GET", method = "HEAD")]
async fn favicon() -> impl Responder {
    HttpResponse::MovedPermanently()
        .insert_header((LOCATION, "https://e-hentai.org/favicon.ico"))
        .finish()
}

#[route("/robots.txt", method = "GET", method = "HEAD")]
async fn robots() -> impl Responder {
    HttpResponse::Ok()
        .content_type(ContentType::plaintext())
        .body("User-agent: *\nDisallow: /")
}

fn forbidden() -> HttpResponse {
    HttpResponse::Forbidden().body("An error has occurred. (403)")
}

fn parse_additional(additional: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for kv in additional.split(';') {
        let pair: Vec<&str> = kv.split('=').collect();
        if pair.len() != 2 {
            continue;
        }

        map.insert(pair[0].to_string(), pair[1].to_string());
    }

    map
}
