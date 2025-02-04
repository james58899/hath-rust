use std::{cmp, convert::Infallible, sync::Arc};

use async_stream::stream;
use axum::{
    body::Body,
    extract::{Path, State},
    http::header::CONTENT_LENGTH,
    response::Response,
};
use bytes::{Bytes, BytesMut};
use rand::{prelude::SmallRng, RngCore, SeedableRng};

use crate::{route::forbidden, util::string_to_hash, AppState, MAX_KEY_TIME_DRIFT};

const BUFFER_SIZE: usize = 16384;

// example: /t/5242880/1645930666/bce541b2a97788319e53a754b47e1801204ae7bf/43432228
pub(super) async fn speedtest(
    Path((size, time, hash, _random)): Path<(u64, i64, String, String)>,
    data: State<Arc<AppState>>,
) -> Response<Body> {
    // Check time & hash
    let hash_string = format!("hentai@home-speedtest-{}-{}-{}-{}", size, time, data.rpc.id(), data.rpc.key());
    if !MAX_KEY_TIME_DRIFT.contains(&(data.rpc.get_timestemp() - time)) || string_to_hash(hash_string) != hash {
        return forbidden();
    }

    random_response(size)
}

pub(super) fn random_response(size: u64) -> Response<Body> {
    Response::builder()
        .header(CONTENT_LENGTH, size)
        .body(Body::from_stream(stream! {
            let mut buffer = BytesMut::zeroed(BUFFER_SIZE);
            SmallRng::from_entropy().fill_bytes(&mut buffer);
            let buffer = buffer.freeze();

            let mut filled = 0;
            while filled < size {
                let size = cmp::min(size - filled, BUFFER_SIZE as u64) as usize;
                filled += size as u64;
                if size == BUFFER_SIZE {
                    yield Ok(buffer.clone());
                    continue;
                }
                yield Ok::<Bytes, Infallible>(buffer.slice(0..size));
            }
        }))
        .unwrap()
}
