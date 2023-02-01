use std::time::Duration;

use futures::future::try_join_all;
use openssl::sha::Sha1;
use tokio::fs::create_dir_all;

use crate::CLIENT_VERSION;

pub fn string_to_hash(str: String) -> String {
    let mut hasher = Sha1::new();
    hasher.update(str.as_bytes());
    hex::encode(hasher.finish())
}

pub fn create_http_client(timeout: Duration) -> reqwest::Client {
    reqwest::ClientBuilder::new()
        .user_agent(format!("Hentai@Home {CLIENT_VERSION}"))
        .tcp_keepalive(Duration::from_secs(75)) // Linux default keepalive inverval
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .pool_idle_timeout(Duration::from_secs(3600))
        .pool_max_idle_per_host(8)
        .http1_title_case_headers()
        .http1_only()
        .build()
        .unwrap()
}

pub async fn create_dirs(dirs: Vec<&str>) -> Result<Vec<()>, std::io::Error> {
    try_join_all(dirs.iter().map(create_dir_all)).await
}
