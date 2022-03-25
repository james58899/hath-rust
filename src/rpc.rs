use std::{
    collections::HashMap,
    net::IpAddr,
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU64, Ordering},
    time::{Instant, Duration},
};

use chrono::{TimeZone, Utc};
use futures::{executor::block_on, TryFutureExt};
use log::{debug, error, info, warn};
use openssl::pkcs12::{ParsedPkcs12, Pkcs12};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use rand::{
    prelude::{SliceRandom, SmallRng},
    SeedableRng,
};
use reqwest::{IntoUrl, Url};

use crate::{
    error::Error,
    util::{create_http_client, string_to_hash},
};

const API_VERSION: i32 = 154; // For server check capabilities.
const DEFAULT_SERVER: &str = "rpc.hentaiathome.net";

pub struct RPCClient {
    api_base: RwLock<Url>,
    clock_offset: AtomicI64,
    id: i32,
    key: String,
    reqwest: reqwest::Client,
    rpc_servers: RwLock<Vec<String>>,
    running: AtomicBool,
    settings: Settings,
}

pub struct Settings {
    client_port: AtomicU16,
    client_host: RwLock<String>,
    size_limit: AtomicU64,
    static_range: RwLock<Vec<String>>,
    verify_cache: AtomicBool,
}

impl Settings {
    /// Get a reference to the settings's client port.
    pub fn client_port(&self) -> u16 {
        self.client_port.load(Ordering::Relaxed)
    }

    /// Get a reference to the settings's size limit.
    pub fn size_limit(&self) -> u64 {
        self.size_limit.load(Ordering::Relaxed)
    }

    /// Get a reference to the settings's static range.
    pub fn static_range(&self) -> Vec<String> {
        self.static_range.read().clone()
    }

    /// Get a reference to the settings's verify cache.
    pub fn verify_cache(&self) -> bool {
        self.verify_cache.load(Ordering::Relaxed)
    }
}

impl RPCClient {
    pub fn new(id: i32, key: &str) -> Self {
        Self {
            api_base: RwLock::new(Url::parse(format!("http://{}/15/rpc?clientbuild={}", DEFAULT_SERVER, API_VERSION).as_str()).unwrap()),
            clock_offset: AtomicI64::new(0),
            id,
            key: key.to_string(),
            reqwest: create_http_client(),
            rpc_servers: RwLock::new(vec![]),
            running: AtomicBool::new(false),
            settings: Settings {
                client_port: AtomicU16::new(0),
                client_host: RwLock::new(String::new()),
                size_limit: AtomicU64::new(u64::MAX),
                static_range: RwLock::new(vec![]),
                verify_cache: AtomicBool::new(false),
            },
        }
    }

    pub async fn login(&self) -> Result<(), Error> {
        // Version & time check
        if let Some((min, new)) = self.check_stat().await? {
            if min > API_VERSION {
                return Err(Error::VersionTooOld);
            } else if new > API_VERSION {
                info!("A new client version is available.");
            }
        }

        // Login
        let res = self.send_action("client_login", None).await;
        if let Err(err) = res {
            error!("Login failed: {}", err);
            return Err(Error::connection_error("Failed to get a login response from server."));
        }

        let res = res.unwrap();
        if res.is_ok() {
            self.update_settings(res.to_map());
            Ok(())
        } else {
            Err(Error::ApiResponseFail {
                fail_code: res.status,
                message: res.data.join("\n"),
            })
        }
    }

    pub async fn get_cert(&self) -> Option<ParsedPkcs12> {
        self.reqwest
            .get(self.build_url("get_cert", ""))
            .send()
            .and_then(|res| res.bytes())
            .await
            .ok()
            .and_then(|data| Pkcs12::from_der(&data[..]).ok())
            .and_then(|cert| cert.parse(self.key.as_str()).ok())
    }

    pub async fn get_purgelist(&self, delta_time: u64) -> Option<Vec<String>> {
        if let Ok(res) = self.send_action("get_blacklist", Some(&delta_time.to_string())).await {
            if res.is_ok() {
                return Some(res.data);
            }
        }
        None
    }

    /// Get a reference to the rpcclient's settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub async fn connect_check(&self) -> Option<()> {
        if let Ok(res) = self.send_action("client_start", None).await {
            if res.is_ok() {
                self.running.store(true, Ordering::Relaxed);
                return Some(());
            }

            error!("Startup Failure: {}", &res.status.as_str());
            match res.status.as_str() {
                "FAIL_CONNECT_TEST" => error!(
                    r#"

************************************************************************************************************************************
The client has failed the external connection test.
The server failed to verify that this client is online and available from the Internet.
If you are behind a firewall, please check that port {} is forwarded to this computer.
You might also want to check that {} is your actual public IP address.
If you need assistance with forwarding a port to this client, locate a guide for your particular router at http://portforward.com/
The client will remain running so you can run port connection tests.
Use Program -> Exit in windowed mode or hit Ctrl+C in console mode to exit the program.
************************************************************************************************************************************

"#,
                    self.settings.client_port.load(Ordering::Relaxed),
                    self.settings.client_host.read()
                ),
                "FAIL_OTHER_CLIENT_CONNECTED" => error!(
                    r#"

************************************************************************************************************************************
The server detected that another client was already connected from this computer or local network.
You can only have one client running per public IP address.
The program will now terminate.
************************************************************************************************************************************

"#
                ),
                "FAIL_CID_IN_USE" => error!(
                    r#"

************************************************************************************************************************************
The server detected that another client is already using this client ident.
If you want to run more than one client, you have to apply for additional idents.
The program will now terminate.
************************************************************************************************************************************

"#
                ),
                _ => (),
            }
        }
        None
    }

    pub async fn refresh_settings(&self) {
        info!("Refreshing Hentai@Home client settings from server...");

        if let Ok(res) = self.send_action("client_settings", None).await {
            if res.is_ok() {
                self.update_settings(res.to_map());
            }
        }
    }

    pub fn is_vaild_rpc_server(&self, ip: &str) -> bool {
        self.rpc_servers.read().iter().any(|s| s == ip)
    }

    pub async fn sr_fetch(&self, file_index: &str, xres: &str, file_id: &str) -> Option<Vec<String>> {
        let add = format!("{};{};{}", file_index, xres, file_id);
        if let Ok(res) = self.send_action("srfetch", Some(&add)).await {
            if res.is_ok() {
                return Some(res.data);
            }
        }

        None
    }

    pub async fn shutdown(&self) {
        if self.running.swap(false, Ordering::Relaxed) {
            let _ = self.send_action("client_stop", None).await;
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub async fn still_alive(&self, resume: bool) {
        if let Ok(res) = self.send_action("still_alive", Some(if resume { "resume" } else { "" })).await {
            if res.is_ok() {
                debug!("Successfully performed a stillAlive test for the server.");
            } else {
                warn!("Failed stillAlive test: ({}) - will retry later", res.status);
            }
        } else {
            warn!("Failed to connect to the server for the stillAlive test. This is probably a temporary connection problem.");
        }
    }

    async fn check_stat(&self) -> Result<Option<(i32, i32)>, Error> {
        let mut url = self.api_base.read().clone();
        url.query_pairs_mut().append_pair("act", "server_stat");
        let start_time = Instant::now();
        let body = self
            .send_request(&url.to_string())
            .await
            .map_err(|_| Error::connection_error("Failed to get initial stat from server."))?;
        let request_time = chrono::Duration::from_std(start_time.elapsed()).unwrap_or_else(|_| chrono::Duration::zero());
        debug!("{}", body);
        let res = parse_response(body.as_str());

        if res.is_ok() {
            let data = res.to_map();

            let server_time = data
                .get("server_time")
                .and_then(|time| time.parse::<i64>().ok())
                .and_then(|time| Utc.timestamp_opt(time, 0).single())
                .and_then(|time| time.checked_add_signed(request_time / 4)); // connecting 1 RTT + request 1 RTT
            if let Some(time) = server_time {
                let offset = Utc::now().signed_duration_since(time);
                self.clock_offset.store(offset.num_seconds(), Ordering::Relaxed);
                debug!(
                    "Server clock offset: {}ms±{}ms",
                    offset.num_milliseconds(),
                    request_time.num_milliseconds()
                );
            }

            let min_version = data.get("min_client_build").and_then(|s| s.parse().ok());
            let new_version = data.get("cur_client_build").and_then(|s| s.parse().ok());

            if min_version.is_none() || new_version.is_none() {
                Ok(None)
            } else {
                Ok(Some((min_version.unwrap(), new_version.unwrap())))
            }
        } else {
            Err(Error::ApiResponseFail {
                fail_code: res.status,
                message: res.data.join("\n"),
            })
        }
    }

    async fn send_action(&self, action: &str, additional: Option<&str>) -> Result<ApiResponse, reqwest::Error> {
        let additional = additional.unwrap_or("");
        let mut result = self.send_request(self.build_url(action, additional)).await.map(|body| {
            debug!("Received response: {}", body);
            parse_response(&body)
        });

        if let Ok(response) = &result {
            if response.is_key_expired() {
                warn!("Server reported expired key; attempting to refresh time from server and retrying");
                let _ = self.check_stat().await; // Sync clock
                result = self.send_request(self.build_url(action, additional)).await.map(|body| {
                    debug!("Received response: {}", body);
                    parse_response(&body)
                })
            }
        }

        result
    }

    async fn send_request<U: IntoUrl>(&self, url: U) -> Result<String, reqwest::Error> {
        match self.reqwest.get(url).timeout(Duration::from_secs(600)).send().await {
            Ok(res) => Ok(res.text().await?),
            Err(err) => {
                if err.is_connect() || err.is_timeout() || err.is_status() {
                    self.change_server();
                }
                Err(err)
            }
        }
    }

    fn build_url(&self, action: &str, additional: &str) -> Url {
        let mut url = self.api_base.read().clone();
        let timestamp = &Utc::now()
            .checked_add_signed(chrono::Duration::seconds(self.clock_offset.load(Ordering::Relaxed)))
            .unwrap_or_else(Utc::now)
            .timestamp()
            .to_string();
        let hash = string_to_hash(format!(
            "hentai@home-{}-{}-{}-{}-{}",
            action, additional, self.id, timestamp, self.key
        ));

        url.query_pairs_mut()
            .append_pair("act", action)
            .append_pair("add", additional)
            .append_pair("cid", &self.id.to_string())
            .append_pair("acttime", timestamp)
            .append_pair("actkey", &hash);

        debug!("{}", url.to_string());
        url
    }

    fn update_settings(&self, settings: HashMap<String, String>) {
        // Update RPC server IP
        if let Some(ips) = settings.get("rpc_server_ip") {
            {
                *self.rpc_servers.write() = ips
                    .split(';')
                    .filter_map(|s| IpAddr::from_str(s).ok())
                    .map(|ip| match ip {
                        IpAddr::V4(ip) => ip.to_string(),
                        IpAddr::V6(ip) => ip.to_ipv4().map_or_else(|| ip.to_string(), |ip| ip.to_string()),
                    })
                    .collect();
            }
            debug!("Setting altered: rpc_server_ip={}", self.rpc_servers.read().join(";"));
            self.change_server();
        }
        // Update Host & Port
        if let Some(host) = settings.get("host") {
            *self.settings.client_host.write() = host.to_owned();
        }
        if let Some(port) = settings.get("port").and_then(|s| s.parse().ok()) {
            self.settings.client_port.store(port, Ordering::Relaxed);
        }
        if let Some(size) = settings.get("disklimit_bytes").and_then(|s| s.parse().ok()) {
            self.settings.client_port.store(size, Ordering::Relaxed);
        }
        if let Some(static_range) = settings.get("static_ranges").map(|s| s.split(';').map(|s| s.to_string()).collect()) {
            *self.settings.static_range.write() = static_range;
        }

        // TODO update other settings
    }

    fn change_server(&self) {
        let rpc_servers = self.rpc_servers.read();

        // Skip if no other server
        if rpc_servers.len() <= 1 {
            return;
        }

        let mut servers = rpc_servers.clone();
        drop(rpc_servers); // Release lock
        let api_base = self.api_base.upgradable_read();
        // Remove failed server
        if let Some(pos) = servers.iter().position(|x| *x == api_base.host_str().unwrap()) {
            servers.swap_remove(pos);
        }

        // Shuffle servers
        if servers.len() > 1 {
            let mut rng = SmallRng::from_entropy();
            servers.shuffle(&mut rng);
        }

        // Update server
        RwLockUpgradableReadGuard::upgrade(api_base)
            .set_host(servers.pop().as_deref())
            .unwrap();
    }
}

impl Drop for RPCClient {
    fn drop(&mut self) {
        if self.running.load(Ordering::Relaxed) {
            block_on(self.send_action("client_stop", None)).unwrap();
        }
    }
}

#[derive(Debug)]
struct ApiResponse {
    status: String,
    data: Vec<String>,
}

impl ApiResponse {
    fn is_ok(&self) -> bool {
        self.status == "OK"
    }

    fn is_key_expired(&self) -> bool {
        self.status == "KEY_EXPIRED"
    }

    /// Parse data to HashMap
    fn to_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for kv in &self.data {
            let pair: Vec<&str> = kv.split('=').collect();
            if pair.len() != 2 {
                continue;
            }

            map.insert(pair[0].to_string(), pair[1].to_string());
        }

        map
    }
}

fn parse_response(res: &str) -> ApiResponse {
    let mut lines = res.lines();
    let status = lines.next();
    match status {
        Some(s) => ApiResponse {
            status: s.to_string(),
            data: lines.map(|s| s.to_string()).collect(),
        },
        None => ApiResponse {
            status: "NO_RESPONSE".to_string(),
            data: vec![],
        },
    }
}
