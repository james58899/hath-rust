use std::{
    cmp::min,
    collections::HashMap,
    error::Error,
    net::{IpAddr, SocketAddr},
    ops::Deref,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::{Extension, extract::FromRequestParts, http::request::Parts};
use bytes::{Buf, Bytes};
use http_body::SizeHint;
use log::warn;
use pin_project_lite::pin_project;

#[derive(Clone)]
pub struct ClientAddr(pub SocketAddr);

impl Deref for ClientAddr {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> FromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = <Extension<Self> as FromRequestParts<S>>::Rejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Extension::<Self>::from_request_parts(parts, state).await {
            Ok(Extension(connect_info)) => Ok(connect_info),
            Err(err) => Err(err),
        }
    }
}

pub(super) struct FloodControl {
    last_clean: Instant,
    hit_map: HashMap<IpAddr, (u8, Instant)>,
    block_list: HashMap<IpAddr, Instant>,
}

impl FloodControl {
    pub fn new() -> Self {
        Self {
            last_clean: Instant::now(),
            hit_map: HashMap::new(),
            block_list: HashMap::new(),
        }
    }

    pub fn hit(&mut self, addr: SocketAddr) -> bool {
        let now = Instant::now();
        let ip = addr.ip();

        // Clean old entry
        if self.last_clean.elapsed() > Duration::from_secs(10) {
            self.block_list.retain(|_, time| *time > now);
            self.hit_map.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(10));
            self.block_list.shrink_to_fit();
            self.hit_map.shrink_to_fit();
            self.last_clean = now;
        }

        // Check block
        if self.block_list.get(&ip).is_some_and(|time| time > &now) {
            return true;
        }

        // Update hit map
        let (hit, time) = self.hit_map.entry(ip).or_insert_with(|| (0, now));
        let time_diff = min(10, (now - *time).as_secs()) as u8;
        *hit = hit.saturating_sub(time_diff as u8) + 1;
        *time = now;

        // Check hit
        if *hit >= 10 {
            warn!("Flood control activated for {ip} (blocking for 60 seconds)");
            self.block_list.insert(ip, now + Duration::from_secs(60));
            true
        } else {
            false
        }
    }
}

pin_project! {
    pub struct TruncateBody<B> {
        limit: u64,
        #[pin]
        inner: B,
    }
}

impl<B> TruncateBody<B> {
    pub fn new(inner: B, limit: u64) -> Self {
        Self { limit, inner }
    }
}

impl<B> http_body::Body for TruncateBody<B>
where
    B: http_body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    type Data = Bytes;
    type Error = Box<dyn Error + Send + Sync>;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let res = match this.inner.poll_frame(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => None,
            Poll::Ready(Some(Ok(mut frame))) => {
                if *this.limit == 0 {
                    return Poll::Ready(None);
                }
                if let Some(data) = frame.data_mut() {
                    let len = data.remaining() as u64;
                    if len > *this.limit {
                        data.truncate(*this.limit as usize);
                        *this.limit = 0;
                    } else {
                        *this.limit -= len;
                    }
                }

                Some(Ok(frame))
            }
            Poll::Ready(Some(Err(err))) => Some(Err(err.into())),
        };

        Poll::Ready(res)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = self.inner.size_hint();
        if hint.lower() >= self.limit {
            hint.set_exact(self.limit)
        } else if let Some(max) = hint.upper() {
            hint.set_upper(self.limit.min(max))
        } else {
            hint.set_upper(self.limit)
        }
        hint
    }
}
