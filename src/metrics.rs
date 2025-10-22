use std::{fmt::Error, sync::atomic::AtomicU64, time::Instant};

use prometheus_client::{
    collector::Collector,
    encoding::{DescriptorEncoder, EncodeLabelSet, EncodeLabelValue, EncodeMetric},
    metrics::{
        counter::{ConstCounter, Counter},
        family::Family,
        gauge::Gauge,
        histogram::Histogram,
    },
    registry::{
        Registry,
        Unit::{Bytes, Seconds},
    },
};

pub const LABEL_CACHE_FETCH_URL: CacheFetchLabels = CacheFetchLabels {
    phase: CacheFetchPhase::FetchUrl,
};
pub const LABEL_CACHE_DOWNLOAD: CacheFetchLabels = CacheFetchLabels {
    phase: CacheFetchPhase::Download,
};

pub struct Metrics {
    pub registry: Registry,
    pub cache_sent: Counter,
    pub cache_sent_size: Family<Vec<(String, String)>, Counter>,
    pub cache_sent_duration: Family<Vec<(String, String)>, Histogram>,
    pub cache_received: Counter,
    pub cache_received_size: Counter,
    pub cache_received_duration: Family<CacheFetchLabels, Histogram>,
    pub connections: Gauge<u64, AtomicU64>,
    pub static_range: Gauge,
    pub cache_capacity: Gauge,
    pub cache_count: Gauge<u64, AtomicU64>,
    pub cache_size: Gauge<u64, AtomicU64>,
    pub download_count: Counter,
    pub download_file_count: Counter,
    pub download_size: Counter,
    pub download_duration: Histogram,
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("hath");

        // Request metrics
        let cache_sent = Counter::default();
        let cache_sent_size = Family::<Vec<(String, String)>, Counter>::default();
        let cache_sent_duration = Family::<Vec<(String, String)>, Histogram>::new_with_constructor(|| {
            Histogram::new([0.01, 0.025, 0.05, 0.1, 0.2, 0.5, 1.0, 1.5, 2.0, 3.0, 5.0, 10.0, 30.0])
        });
        let cache_received = Counter::default();
        let cache_received_size = Counter::default();
        let cache_received_duration = Family::<CacheFetchLabels, Histogram>::new_with_constructor(|| {
            Histogram::new([0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0])
        });
        let connections = Gauge::<u64, AtomicU64>::default();
        registry.register("cache_sent", "Number of files sent", cache_sent.clone());
        registry.register_with_unit("cache_sent_size", "Number of bytes sent", Bytes, cache_sent_size.clone());
        registry.register_with_unit("cache_sent_duration", "Histogram of cache sent", Seconds, cache_sent_duration.clone());
        registry.register("cache_received", "Number of files received", cache_received.clone());
        registry.register_with_unit("cache_received_size", "Number of bytes received", Bytes, cache_received_size.clone());
        registry.register_with_unit("cache_received_duration", "Histogram of cache received", Seconds, cache_received_duration.clone());
        registry.register("connections", "Number of connections", connections.clone());

        // Cache metrics
        let static_range = Gauge::default();
        let cache_capacity = Gauge::default();
        let cache_count = Gauge::<u64, AtomicU64>::default();
        let cache_size = Gauge::<u64, AtomicU64>::default();
        registry.register("cache_count", "Number of files in cache", cache_count.clone());
        registry.register_with_unit("cache_size", "Number of bytes in cache", Bytes, cache_size.clone());
        registry.register_with_unit("cache_capacity", "Cache capacity", Bytes, cache_capacity.clone());
        registry.register("static_range", "Static range", static_range.clone());

        // H@H downloader metrics
        let download_count = Counter::default();
        let download_file_count = Counter::default();
        let download_size = Counter::default();
        let download_duration = Histogram::new([0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]);
        registry.register("download_count", "Number of tasks received by downloader", download_count.clone());
        registry.register("download_file_count", "Number of files downloaded", download_file_count.clone());
        registry.register_with_unit("download_size", "Number of bytes downloaded", Bytes, download_size.clone());
        registry.register_with_unit("download_duration", "Histogram of download", Seconds, download_duration.clone());

        // Uptime
        registry.register_collector(Box::new(Uptime::new()));

        Self {
            registry,
            cache_sent,
            cache_sent_size,
            cache_sent_duration,
            cache_received,
            cache_received_size,
            cache_received_duration,
            connections,
            static_range,
            cache_capacity,
            cache_count,
            cache_size,
            download_count,
            download_file_count,
            download_size,
            download_duration,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, EncodeLabelSet)]
pub struct CacheFetchLabels {
    phase: CacheFetchPhase,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, EncodeLabelValue)]
enum CacheFetchPhase {
    FetchUrl,
    Download,
}

#[derive(Debug)]
struct Uptime {
    start_time: Instant,
}

impl Uptime {
    fn new() -> Self {
        Self {
            start_time: Instant::now(),
        }
    }
}

impl Collector for Uptime {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), Error> {
        let counter = ConstCounter::new(self.start_time.elapsed().as_secs());
        let metrics_encoder = encoder.encode_descriptor("uptime", "Uptime in seconds", Some(&Seconds), counter.metric_type())?;
        counter.encode(metrics_encoder)?;
        Ok(())
    }
}
