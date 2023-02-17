use std::{cmp, convert::Infallible};

use actix_web::{
    body::SizedStream,
    route,
    web::{Bytes, Data},
    HttpResponse, Responder,
};
use actix_web_lab::extract::Path;
use async_stream::stream;
use rand::{prelude::SmallRng, RngCore, SeedableRng};

use crate::{route::forbidden, util::string_to_hash, AppState, MAX_KEY_TIME_DRIFT};

// example: /t/5242880/1645930666/bce541b2a97788319e53a754b47e1801204ae7bf/43432228
#[route("/t/{size}/{time}/{hash}/{random}", method = "GET", method = "HEAD")]
async fn speedtest(Path((size, time, hash)): Path<(u64, i64, String)>, data: Data<AppState>) -> impl Responder {
    // Check time & hash
    let hash_string = format!("hentai@home-speedtest-{}-{}-{}-{}", size, time, data.rpc.id(), data.rpc.key());
    if !MAX_KEY_TIME_DRIFT.contains(&(data.rpc.get_timestemp() - time)) || string_to_hash(hash_string) != hash {
        return forbidden();
    }

    random_response(size)
}

pub(super) fn random_response(size: u64) -> HttpResponse {
    HttpResponse::Ok().body(SizedStream::new(
        size,
        stream! {
            let mut buffer = [0; 8192];
            SmallRng::from_entropy().fill_bytes(&mut buffer);
            let buffer = Bytes::copy_from_slice(&buffer);

            let mut filled = 0;
            while(filled < size) {
                let size = cmp::min(size - filled, 8192) as usize;
                yield Result::Ok::<Bytes, Infallible>(buffer.slice(0..size));
                filled += size as u64;
            }
        },
    ))
}
