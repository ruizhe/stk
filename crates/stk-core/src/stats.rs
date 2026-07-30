use hyper::body::{Body, Bytes, Frame, SizeHint};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    pin::Pin,
    sync::{
        Arc, LazyLock, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::watch,
};

const TRAFFIC_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const TRAFFIC_RATE_WINDOW_MS: u64 = 1_000;
const TRAFFIC_HISTORY_BUCKET_MS: u64 = 60_000;
const TRAFFIC_HISTORY_BUCKETS: usize = 24 * 60;
const RATE_PUSH_MIN_DELTA_BPS: u64 = 1024;
const RATE_PUSH_ABSOLUTE_DELTA_BPS: u64 = 64 * 1024;
const RATE_PUSH_RELATIVE_PERCENT: u64 = 25;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LatencySnapshot {
    pub samples: u64,
    pub average_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostRuntimeStatus {
    Idle,
    Connecting,
    Healthy,
    Degraded,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SshSessionRuntimeStatus {
    Connecting,
    Healthy,
    Suspect,
    Draining,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TunnelKind {
    LocalProxy,
    LocalForward,
    RemoteProxy,
    RemoteForward,
}

impl TunnelKind {
    fn key(self) -> &'static str {
        match self {
            Self::LocalProxy => "local-proxy",
            Self::LocalForward => "local-forward",
            Self::RemoteProxy => "remote-proxy",
            Self::RemoteForward => "remote-forward",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TunnelRuntimeStatus {
    Starting,
    Listening,
    Error,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionRuntimeStatus {
    Connecting,
    Active,
    Closed,
    Error,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConnectionCaptureSnapshot {
    pub recording: bool,
    pub auto_clear_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConnectionRuntimeSnapshot {
    pub id: u64,
    pub status: ConnectionRuntimeStatus,
    pub tunnel_id: String,
    pub peer_address: String,
    pub target: Option<String>,
    pub protocol: Option<String>,
    pub session_id: Option<u64>,
    pub created_at_unix_ms: u64,
    pub established_at_unix_ms: Option<u64>,
    pub ended_at_unix_ms: Option<u64>,
    pub uptime_ms: u64,
    pub upload_bps: u64,
    pub download_bps: u64,
    pub uploaded_bytes_total: u64,
    pub downloaded_bytes_total: u64,
    pub errors_total: u64,
    pub last_activity_unix_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SshSessionRuntimeSnapshot {
    pub id: u64,
    pub status: SshSessionRuntimeStatus,
    pub ssh_alias: String,
    pub address: String,
    pub created_at_unix_ms: u64,
    pub established_at_unix_ms: Option<u64>,
    pub ended_at_unix_ms: Option<u64>,
    pub uptime_ms: Option<u64>,
    pub startup_ms: Option<f64>,
    pub rtt_ms: Option<u64>,
    pub active_channels: u64,
    pub channels_total: u64,
    pub channel_open_errors_total: u64,
    #[serde(default)]
    pub upload_bps: u64,
    #[serde(default)]
    pub download_bps: u64,
    pub uploaded_bytes_total: u64,
    pub downloaded_bytes_total: u64,
    pub last_activity_unix_ms: u64,
    pub last_probe_unix_ms: Option<u64>,
    pub last_error: Option<String>,
    pub retiring: bool,
    pub remote_forward_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TunnelRuntimeSnapshot {
    pub id: String,
    pub name: String,
    pub kind: TunnelKind,
    pub status: TunnelRuntimeStatus,
    pub listen: String,
    pub target: Option<String>,
    pub protocol: Option<String>,
    pub owner_session_id: Option<u64>,
    pub started_at_unix_ms: u64,
    pub last_activity_unix_ms: u64,
    pub connections_active: u64,
    pub connections_total: u64,
    #[serde(default)]
    pub upload_bps: u64,
    #[serde(default)]
    pub download_bps: u64,
    pub uploaded_bytes_total: u64,
    pub downloaded_bytes_total: u64,
    pub errors_total: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct HostRuntimeSnapshot {
    pub name: String,
    pub status: HostRuntimeStatus,
    pub ssh_alias: String,
    pub address: String,
    pub min_sessions: u64,
    pub max_sessions: u64,
    pub rtt_ms: Option<u64>,
    pub restart_count: u64,
    pub connections_active: u64,
    pub connections_total: u64,
    #[serde(default)]
    pub upload_bps: u64,
    #[serde(default)]
    pub download_bps: u64,
    pub uploaded_bytes_total: u64,
    pub downloaded_bytes_total: u64,
    pub errors_total: u64,
    pub last_activity_unix_ms: u64,
    pub last_error: Option<String>,
    pub sessions: Vec<SshSessionRuntimeSnapshot>,
    pub tunnels: Vec<TunnelRuntimeSnapshot>,
    #[serde(default)]
    pub connections: Vec<ConnectionRuntimeSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RuntimeSnapshot {
    pub running: bool,
    pub uptime_ms: Option<u64>,
    pub configured_hosts: u64,
    pub configured_local_listeners: u64,
    pub configured_remote_listeners: u64,
    pub config_generation: u64,
    pub config_reloads_total: u64,
    pub config_reload_errors_total: u64,
    pub local_connections_total: u64,
    pub local_connections_active: u64,
    pub ssh_sessions_total: u64,
    pub ssh_sessions_active: u64,
    pub ssh_channel_open: LatencySnapshot,
    #[serde(default)]
    pub rate_sampled_at_unix_ms: u64,
    #[serde(default)]
    pub rate_window_ms: u64,
    #[serde(default)]
    pub upload_bps: u64,
    #[serde(default)]
    pub download_bps: u64,
    pub uploaded_bytes_total: u64,
    pub downloaded_bytes_total: u64,
    pub transferred_bytes_total: u64,
    pub errors_total: u64,
    #[serde(default)]
    pub connection_capture: ConnectionCaptureSnapshot,
    #[serde(default)]
    pub hosts: Vec<HostRuntimeSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TrafficHistoryPoint {
    pub started_at_unix_ms: u64,
    pub duration_ms: u64,
    pub upload_bps: u64,
    pub download_bps: u64,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TrafficHistorySnapshot {
    pub retention_hours: u64,
    pub bucket_seconds: u64,
    pub sampled_at_unix_ms: u64,
    pub points: Vec<TrafficHistoryPoint>,
}

#[derive(Debug, Clone)]
pub(crate) struct HostRegistration {
    pub name: String,
    pub ssh_alias: String,
    pub address: String,
    pub min_sessions: usize,
    pub max_sessions: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct HostStateUpdate {
    pub status: HostRuntimeStatus,
    pub rtt_ms: Option<u64>,
    pub restart_count: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SshSessionRegistration {
    pub id: u64,
    pub host_name: String,
    pub ssh_alias: String,
    pub address: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SshSessionStateUpdate {
    pub status: SshSessionRuntimeStatus,
    pub startup_ms: Option<f64>,
    pub rtt_ms: Option<u64>,
    pub retiring: bool,
    pub remote_forward_owner: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TunnelRegistration {
    pub id: String,
    pub host_name: String,
    pub name: String,
    pub kind: TunnelKind,
    pub listen: String,
    pub target: Option<String>,
    pub protocol: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ConnectionRegistration {
    pub id: u64,
    pub host_name: String,
    pub tunnel_id: String,
    pub peer_address: String,
    pub target: Option<String>,
    pub protocol: Option<String>,
    pub session_id: Option<u64>,
}

struct RuntimeStats {
    running: AtomicBool,
    started_at: StdMutex<Option<Instant>>,
    configured_hosts: AtomicU64,
    configured_local_listeners: AtomicU64,
    configured_remote_listeners: AtomicU64,
    config_generation: AtomicU64,
    config_reloads_total: AtomicU64,
    config_reload_errors_total: AtomicU64,
    local_connections_total: AtomicU64,
    local_connections_active: AtomicU64,
    ssh_sessions_total: AtomicU64,
    ssh_sessions_active: AtomicU64,
    ssh_channel_open_samples: AtomicU64,
    ssh_channel_open_total_micros: AtomicU64,
    ssh_channel_open_max_micros: AtomicU64,
    rate_sampled_at_unix_ms: AtomicU64,
    rate_window_ms: AtomicU64,
    upload_bps: AtomicU64,
    download_bps: AtomicU64,
    rate_samples: StdMutex<RollingRateWindow>,
    sampled_uploaded_bytes: AtomicU64,
    sampled_downloaded_bytes: AtomicU64,
    uploaded_bytes_total: AtomicU64,
    downloaded_bytes_total: AtomicU64,
    transferred_bytes_total: AtomicU64,
    errors_total: AtomicU64,
    connection_capture_enabled: AtomicBool,
    connection_capture_auto_clear_closed: AtomicBool,
}

#[derive(Debug, Clone, Copy)]
struct RateSample {
    duration_ms: u64,
    uploaded_bytes: u64,
    downloaded_bytes: u64,
}

#[derive(Default)]
struct RollingRateWindow {
    samples: VecDeque<RateSample>,
    duration_ms: u64,
    uploaded_bytes: u64,
    downloaded_bytes: u64,
}

impl RollingRateWindow {
    fn reset(&mut self) {
        self.samples.clear();
        self.duration_ms = 0;
        self.uploaded_bytes = 0;
        self.downloaded_bytes = 0;
    }

    fn record(
        &mut self,
        elapsed_ms: u64,
        uploaded_bytes: u64,
        downloaded_bytes: u64,
    ) -> (u64, u64) {
        let elapsed_ms = elapsed_ms.max(1);
        self.samples.push_back(RateSample {
            duration_ms: elapsed_ms,
            uploaded_bytes,
            downloaded_bytes,
        });
        self.duration_ms = self.duration_ms.saturating_add(elapsed_ms);
        self.uploaded_bytes = self.uploaded_bytes.saturating_add(uploaded_bytes);
        self.downloaded_bytes = self.downloaded_bytes.saturating_add(downloaded_bytes);
        self.trim_to_window();
        (
            bytes_per_second(self.uploaded_bytes, TRAFFIC_RATE_WINDOW_MS as f64 / 1_000.0),
            bytes_per_second(
                self.downloaded_bytes,
                TRAFFIC_RATE_WINDOW_MS as f64 / 1_000.0,
            ),
        )
    }

    fn trim_to_window(&mut self) {
        while self.duration_ms > TRAFFIC_RATE_WINDOW_MS {
            let excess = self.duration_ms - TRAFFIC_RATE_WINDOW_MS;
            let Some(front) = self.samples.front_mut() else {
                self.reset();
                return;
            };
            if front.duration_ms <= excess {
                let removed = self.samples.pop_front().expect("front sample must exist");
                self.duration_ms = self.duration_ms.saturating_sub(removed.duration_ms);
                self.uploaded_bytes = self.uploaded_bytes.saturating_sub(removed.uploaded_bytes);
                self.downloaded_bytes = self
                    .downloaded_bytes
                    .saturating_sub(removed.downloaded_bytes);
                continue;
            }

            let removed_uploaded =
                proportional_bytes(front.uploaded_bytes, excess, front.duration_ms);
            let removed_downloaded =
                proportional_bytes(front.downloaded_bytes, excess, front.duration_ms);
            front.duration_ms -= excess;
            front.uploaded_bytes = front.uploaded_bytes.saturating_sub(removed_uploaded);
            front.downloaded_bytes = front.downloaded_bytes.saturating_sub(removed_downloaded);
            self.duration_ms -= excess;
            self.uploaded_bytes = self.uploaded_bytes.saturating_sub(removed_uploaded);
            self.downloaded_bytes = self.downloaded_bytes.saturating_sub(removed_downloaded);
        }
    }
}

fn proportional_bytes(bytes: u64, duration_ms: u64, sample_duration_ms: u64) -> u64 {
    ((bytes as u128 * duration_ms as u128) / sample_duration_ms.max(1) as u128)
        .min(u64::MAX as u128) as u64
}

#[derive(Default)]
struct TrafficTotals {
    uploaded_bytes_total: AtomicU64,
    downloaded_bytes_total: AtomicU64,
    upload_bps: AtomicU64,
    download_bps: AtomicU64,
    rate_samples: StdMutex<RollingRateWindow>,
    sampled_uploaded_bytes: AtomicU64,
    sampled_downloaded_bytes: AtomicU64,
}

impl TrafficTotals {
    fn add(&self, uploaded_bytes: u64, downloaded_bytes: u64) {
        self.uploaded_bytes_total
            .fetch_add(uploaded_bytes, Ordering::Relaxed);
        self.downloaded_bytes_total
            .fetch_add(downloaded_bytes, Ordering::Relaxed);
    }

    fn uploaded(&self) -> u64 {
        self.uploaded_bytes_total.load(Ordering::Relaxed)
    }

    fn downloaded(&self) -> u64 {
        self.downloaded_bytes_total.load(Ordering::Relaxed)
    }

    fn upload_bps(&self) -> u64 {
        self.upload_bps.load(Ordering::Relaxed)
    }

    fn download_bps(&self) -> u64 {
        self.download_bps.load(Ordering::Relaxed)
    }

    fn reset_sample(&self) {
        self.sampled_uploaded_bytes
            .store(self.uploaded(), Ordering::Relaxed);
        self.sampled_downloaded_bytes
            .store(self.downloaded(), Ordering::Relaxed);
        self.upload_bps.store(0, Ordering::Relaxed);
        self.download_bps.store(0, Ordering::Relaxed);
        self.rate_samples
            .lock()
            .expect("traffic rate window lock poisoned")
            .reset();
    }

    fn sample(&self, elapsed_ms: u64) -> (u64, u64) {
        let uploaded = self.uploaded();
        let downloaded = self.downloaded();
        let previous_uploaded = self
            .sampled_uploaded_bytes
            .swap(uploaded, Ordering::Relaxed);
        let previous_downloaded = self
            .sampled_downloaded_bytes
            .swap(downloaded, Ordering::Relaxed);
        let upload_delta = uploaded.saturating_sub(previous_uploaded);
        let download_delta = downloaded.saturating_sub(previous_downloaded);
        let (upload_bps, download_bps) = self
            .rate_samples
            .lock()
            .expect("traffic rate window lock poisoned")
            .record(elapsed_ms, upload_delta, download_delta);
        self.upload_bps.store(upload_bps, Ordering::Relaxed);
        self.download_bps.store(download_bps, Ordering::Relaxed);
        (upload_delta, download_delta)
    }

    fn clear_rate(&self) {
        self.upload_bps.store(0, Ordering::Relaxed);
        self.download_bps.store(0, Ordering::Relaxed);
        self.rate_samples
            .lock()
            .expect("traffic rate window lock poisoned")
            .reset();
    }
}

struct HostDetail {
    registration: HostRegistration,
    state: StdMutex<HostDetailState>,
    connections_total: AtomicU64,
    connections_active: AtomicU64,
    traffic: TrafficTotals,
    errors_total: AtomicU64,
    last_activity_unix_ms: AtomicU64,
}

struct HostDetailState {
    status: HostRuntimeStatus,
    rtt_ms: Option<u64>,
    restart_count: u64,
    last_error: Option<String>,
}

struct SshSessionDetail {
    registration: SshSessionRegistration,
    state: StdMutex<SshSessionDetailState>,
    created_at_unix_ms: u64,
    active_channels: AtomicU64,
    channels_total: AtomicU64,
    channel_open_errors_total: AtomicU64,
    traffic: TrafficTotals,
    last_activity_unix_ms: AtomicU64,
    last_probe_unix_ms: AtomicU64,
}

struct SshSessionDetailState {
    status: SshSessionRuntimeStatus,
    established_at_unix_ms: Option<u64>,
    ended_at_unix_ms: Option<u64>,
    startup_ms: Option<f64>,
    rtt_ms: Option<u64>,
    last_error: Option<String>,
    retiring: bool,
    remote_forward_owner: bool,
}

struct TunnelDetail {
    registration: TunnelRegistration,
    state: StdMutex<TunnelDetailState>,
    connections_total: AtomicU64,
    connections_active: AtomicU64,
    traffic: TrafficTotals,
    errors_total: AtomicU64,
    started_at_unix_ms: u64,
    last_activity_unix_ms: AtomicU64,
}

struct TunnelDetailState {
    status: TunnelRuntimeStatus,
    owner_session_id: Option<u64>,
    last_error: Option<String>,
}

struct ConnectionDetail {
    registration: ConnectionRegistration,
    state: StdMutex<ConnectionDetailState>,
    created_at_unix_ms: u64,
    traffic: TrafficTotals,
    errors_total: AtomicU64,
    last_activity_unix_ms: AtomicU64,
}

struct ConnectionDetailState {
    status: ConnectionRuntimeStatus,
    target: Option<String>,
    protocol: Option<String>,
    session_id: Option<u64>,
    established_at_unix_ms: Option<u64>,
    ended_at_unix_ms: Option<u64>,
    last_error: Option<String>,
}

#[derive(Default)]
struct RuntimeDetails {
    hosts: BTreeMap<String, Arc<HostDetail>>,
    sessions: BTreeMap<u64, Arc<SshSessionDetail>>,
    tunnels: BTreeMap<String, Arc<TunnelDetail>>,
    connections: BTreeMap<u64, Arc<ConnectionDetail>>,
}

#[derive(Default)]
struct TrafficHistory {
    buckets: VecDeque<TrafficHistoryBucket>,
}

struct TrafficHistoryBucket {
    started_at_unix_ms: u64,
    duration_ms: u64,
    uploaded_bytes: u64,
    downloaded_bytes: u64,
}

impl RuntimeDetails {
    fn snapshot(&self) -> Vec<HostRuntimeSnapshot> {
        let now = unix_time_ms();
        self.hosts
            .values()
            .map(|host| {
                let state = host.state.lock().expect("host detail lock poisoned");
                let sessions = self
                    .sessions
                    .values()
                    .filter(|session| session.registration.host_name == host.registration.name)
                    .map(|session| session.snapshot(now))
                    .collect();
                let tunnels = self
                    .tunnels
                    .values()
                    .filter(|tunnel| tunnel.registration.host_name == host.registration.name)
                    .map(|tunnel| tunnel.snapshot())
                    .collect();
                let connections = self
                    .connections
                    .values()
                    .filter(|connection| {
                        connection.registration.host_name == host.registration.name
                    })
                    .map(|connection| connection.snapshot(now))
                    .collect();
                HostRuntimeSnapshot {
                    name: host.registration.name.clone(),
                    status: state.status,
                    ssh_alias: host.registration.ssh_alias.clone(),
                    address: host.registration.address.clone(),
                    min_sessions: u64::try_from(host.registration.min_sessions).unwrap_or(u64::MAX),
                    max_sessions: u64::try_from(host.registration.max_sessions).unwrap_or(u64::MAX),
                    rtt_ms: state.rtt_ms,
                    restart_count: state.restart_count,
                    connections_active: host.connections_active.load(Ordering::Relaxed),
                    connections_total: host.connections_total.load(Ordering::Relaxed),
                    upload_bps: host.traffic.upload_bps(),
                    download_bps: host.traffic.download_bps(),
                    uploaded_bytes_total: host.traffic.uploaded(),
                    downloaded_bytes_total: host.traffic.downloaded(),
                    errors_total: host.errors_total.load(Ordering::Relaxed),
                    last_activity_unix_ms: host.last_activity_unix_ms.load(Ordering::Relaxed),
                    last_error: state.last_error.clone(),
                    sessions,
                    tunnels,
                    connections,
                }
            })
            .collect()
    }
}

impl HostDetail {
    fn new(registration: HostRegistration) -> Self {
        let now = unix_time_ms();
        Self {
            registration,
            state: StdMutex::new(HostDetailState {
                status: HostRuntimeStatus::Connecting,
                rtt_ms: None,
                restart_count: 0,
                last_error: None,
            }),
            connections_total: AtomicU64::new(0),
            connections_active: AtomicU64::new(0),
            traffic: TrafficTotals::default(),
            errors_total: AtomicU64::new(0),
            last_activity_unix_ms: AtomicU64::new(now),
        }
    }
}

impl SshSessionDetail {
    fn new(registration: SshSessionRegistration) -> Self {
        let now = unix_time_ms();
        Self {
            registration,
            state: StdMutex::new(SshSessionDetailState {
                status: SshSessionRuntimeStatus::Connecting,
                established_at_unix_ms: None,
                ended_at_unix_ms: None,
                startup_ms: None,
                rtt_ms: None,
                last_error: None,
                retiring: false,
                remote_forward_owner: false,
            }),
            created_at_unix_ms: now,
            active_channels: AtomicU64::new(0),
            channels_total: AtomicU64::new(0),
            channel_open_errors_total: AtomicU64::new(0),
            traffic: TrafficTotals::default(),
            last_activity_unix_ms: AtomicU64::new(now),
            last_probe_unix_ms: AtomicU64::new(0),
        }
    }

    fn snapshot(&self, now: u64) -> SshSessionRuntimeSnapshot {
        let state = self.state.lock().expect("SSH session detail lock poisoned");
        let uptime_ms = state.established_at_unix_ms.map(|established| {
            state
                .ended_at_unix_ms
                .unwrap_or(now)
                .saturating_sub(established)
        });
        SshSessionRuntimeSnapshot {
            id: self.registration.id,
            status: state.status,
            ssh_alias: self.registration.ssh_alias.clone(),
            address: self.registration.address.clone(),
            created_at_unix_ms: self.created_at_unix_ms,
            established_at_unix_ms: state.established_at_unix_ms,
            ended_at_unix_ms: state.ended_at_unix_ms,
            uptime_ms,
            startup_ms: state.startup_ms,
            rtt_ms: state.rtt_ms,
            active_channels: self.active_channels.load(Ordering::Relaxed),
            channels_total: self.channels_total.load(Ordering::Relaxed),
            channel_open_errors_total: self.channel_open_errors_total.load(Ordering::Relaxed),
            upload_bps: self.traffic.upload_bps(),
            download_bps: self.traffic.download_bps(),
            uploaded_bytes_total: self.traffic.uploaded(),
            downloaded_bytes_total: self.traffic.downloaded(),
            last_activity_unix_ms: self.last_activity_unix_ms.load(Ordering::Relaxed),
            last_probe_unix_ms: non_zero(self.last_probe_unix_ms.load(Ordering::Relaxed)),
            last_error: state.last_error.clone(),
            retiring: state.retiring,
            remote_forward_owner: state.remote_forward_owner,
        }
    }
}

impl TunnelDetail {
    fn new(registration: TunnelRegistration) -> Self {
        let now = unix_time_ms();
        Self {
            registration,
            state: StdMutex::new(TunnelDetailState {
                status: TunnelRuntimeStatus::Starting,
                owner_session_id: None,
                last_error: None,
            }),
            connections_total: AtomicU64::new(0),
            connections_active: AtomicU64::new(0),
            traffic: TrafficTotals::default(),
            errors_total: AtomicU64::new(0),
            started_at_unix_ms: now,
            last_activity_unix_ms: AtomicU64::new(now),
        }
    }

    fn snapshot(&self) -> TunnelRuntimeSnapshot {
        let state = self.state.lock().expect("tunnel detail lock poisoned");
        TunnelRuntimeSnapshot {
            id: self.registration.id.clone(),
            name: self.registration.name.clone(),
            kind: self.registration.kind,
            status: state.status,
            listen: self.registration.listen.clone(),
            target: self.registration.target.clone(),
            protocol: self.registration.protocol.clone(),
            owner_session_id: state.owner_session_id,
            started_at_unix_ms: self.started_at_unix_ms,
            last_activity_unix_ms: self.last_activity_unix_ms.load(Ordering::Relaxed),
            connections_active: self.connections_active.load(Ordering::Relaxed),
            connections_total: self.connections_total.load(Ordering::Relaxed),
            upload_bps: self.traffic.upload_bps(),
            download_bps: self.traffic.download_bps(),
            uploaded_bytes_total: self.traffic.uploaded(),
            downloaded_bytes_total: self.traffic.downloaded(),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            last_error: state.last_error.clone(),
        }
    }
}

impl ConnectionDetail {
    fn new(registration: ConnectionRegistration) -> Self {
        let now = unix_time_ms();
        Self {
            state: StdMutex::new(ConnectionDetailState {
                status: ConnectionRuntimeStatus::Connecting,
                target: registration.target.clone(),
                protocol: registration.protocol.clone(),
                session_id: registration.session_id,
                established_at_unix_ms: None,
                ended_at_unix_ms: None,
                last_error: None,
            }),
            registration,
            created_at_unix_ms: now,
            traffic: TrafficTotals::default(),
            errors_total: AtomicU64::new(0),
            last_activity_unix_ms: AtomicU64::new(now),
        }
    }

    fn snapshot(&self, now: u64) -> ConnectionRuntimeSnapshot {
        let state = self.state.lock().expect("connection detail lock poisoned");
        let ended_or_now = state.ended_at_unix_ms.unwrap_or(now);
        ConnectionRuntimeSnapshot {
            id: self.registration.id,
            status: state.status,
            tunnel_id: self.registration.tunnel_id.clone(),
            peer_address: self.registration.peer_address.clone(),
            target: state.target.clone(),
            protocol: state.protocol.clone(),
            session_id: state.session_id,
            created_at_unix_ms: self.created_at_unix_ms,
            established_at_unix_ms: state.established_at_unix_ms,
            ended_at_unix_ms: state.ended_at_unix_ms,
            uptime_ms: ended_or_now.saturating_sub(self.created_at_unix_ms),
            upload_bps: self.traffic.upload_bps(),
            download_bps: self.traffic.download_bps(),
            uploaded_bytes_total: self.traffic.uploaded(),
            downloaded_bytes_total: self.traffic.downloaded(),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            last_activity_unix_ms: self.last_activity_unix_ms.load(Ordering::Relaxed),
            last_error: state.last_error.clone(),
        }
    }
}

impl RuntimeStats {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            started_at: StdMutex::new(None),
            configured_hosts: AtomicU64::new(0),
            configured_local_listeners: AtomicU64::new(0),
            configured_remote_listeners: AtomicU64::new(0),
            config_generation: AtomicU64::new(0),
            config_reloads_total: AtomicU64::new(0),
            config_reload_errors_total: AtomicU64::new(0),
            local_connections_total: AtomicU64::new(0),
            local_connections_active: AtomicU64::new(0),
            ssh_sessions_total: AtomicU64::new(0),
            ssh_sessions_active: AtomicU64::new(0),
            ssh_channel_open_samples: AtomicU64::new(0),
            ssh_channel_open_total_micros: AtomicU64::new(0),
            ssh_channel_open_max_micros: AtomicU64::new(0),
            rate_sampled_at_unix_ms: AtomicU64::new(0),
            rate_window_ms: AtomicU64::new(0),
            upload_bps: AtomicU64::new(0),
            download_bps: AtomicU64::new(0),
            rate_samples: StdMutex::new(RollingRateWindow::default()),
            sampled_uploaded_bytes: AtomicU64::new(0),
            sampled_downloaded_bytes: AtomicU64::new(0),
            uploaded_bytes_total: AtomicU64::new(0),
            downloaded_bytes_total: AtomicU64::new(0),
            transferred_bytes_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            connection_capture_enabled: AtomicBool::new(false),
            connection_capture_auto_clear_closed: AtomicBool::new(false),
        }
    }

    fn snapshot(&self) -> RuntimeSnapshot {
        let samples = self.ssh_channel_open_samples.load(Ordering::Relaxed);
        let total_micros = self.ssh_channel_open_total_micros.load(Ordering::Relaxed);
        let max_micros = self.ssh_channel_open_max_micros.load(Ordering::Relaxed);
        let started_at = *self.started_at.lock().expect("runtime stats lock poisoned");
        RuntimeSnapshot {
            running: self.running.load(Ordering::Relaxed),
            uptime_ms: started_at
                .map(|started| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
            configured_hosts: self.configured_hosts.load(Ordering::Relaxed),
            configured_local_listeners: self.configured_local_listeners.load(Ordering::Relaxed),
            configured_remote_listeners: self.configured_remote_listeners.load(Ordering::Relaxed),
            config_generation: self.config_generation.load(Ordering::Relaxed),
            config_reloads_total: self.config_reloads_total.load(Ordering::Relaxed),
            config_reload_errors_total: self.config_reload_errors_total.load(Ordering::Relaxed),
            local_connections_total: self.local_connections_total.load(Ordering::Relaxed),
            local_connections_active: self.local_connections_active.load(Ordering::Relaxed),
            ssh_sessions_total: self.ssh_sessions_total.load(Ordering::Relaxed),
            ssh_sessions_active: self.ssh_sessions_active.load(Ordering::Relaxed),
            ssh_channel_open: LatencySnapshot {
                samples,
                average_ms: (samples > 0)
                    .then(|| micros_to_ms(total_micros as f64 / samples as f64)),
                max_ms: (samples > 0).then(|| micros_to_ms(max_micros as f64)),
            },
            rate_sampled_at_unix_ms: self.rate_sampled_at_unix_ms.load(Ordering::Relaxed),
            rate_window_ms: self.rate_window_ms.load(Ordering::Relaxed),
            upload_bps: self.upload_bps.load(Ordering::Relaxed),
            download_bps: self.download_bps.load(Ordering::Relaxed),
            uploaded_bytes_total: self.uploaded_bytes_total.load(Ordering::Relaxed),
            downloaded_bytes_total: self.downloaded_bytes_total.load(Ordering::Relaxed),
            transferred_bytes_total: self.transferred_bytes_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            connection_capture: ConnectionCaptureSnapshot {
                recording: self.connection_capture_enabled.load(Ordering::Relaxed),
                auto_clear_closed: self
                    .connection_capture_auto_clear_closed
                    .load(Ordering::Relaxed),
            },
            hosts: RUNTIME_DETAILS
                .lock()
                .expect("runtime details lock poisoned")
                .snapshot(),
        }
    }

    fn add_transferred_bytes(&self, uploaded_bytes: u64, downloaded_bytes: u64) {
        self.uploaded_bytes_total
            .fetch_add(uploaded_bytes, Ordering::Relaxed);
        self.downloaded_bytes_total
            .fetch_add(downloaded_bytes, Ordering::Relaxed);
        self.transferred_bytes_total.fetch_add(
            uploaded_bytes.saturating_add(downloaded_bytes),
            Ordering::Relaxed,
        );
    }

    fn reset_rate_sample(&self) {
        self.sampled_uploaded_bytes.store(
            self.uploaded_bytes_total.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.sampled_downloaded_bytes.store(
            self.downloaded_bytes_total.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.upload_bps.store(0, Ordering::Relaxed);
        self.download_bps.store(0, Ordering::Relaxed);
        self.rate_samples
            .lock()
            .expect("runtime rate window lock poisoned")
            .reset();
        self.rate_window_ms.store(0, Ordering::Relaxed);
        self.rate_sampled_at_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
    }

    fn sample_rate(&self, elapsed_ms: u64) -> (u64, u64) {
        let uploaded = self.uploaded_bytes_total.load(Ordering::Relaxed);
        let downloaded = self.downloaded_bytes_total.load(Ordering::Relaxed);
        let previous_uploaded = self
            .sampled_uploaded_bytes
            .swap(uploaded, Ordering::Relaxed);
        let previous_downloaded = self
            .sampled_downloaded_bytes
            .swap(downloaded, Ordering::Relaxed);
        let upload_delta = uploaded.saturating_sub(previous_uploaded);
        let download_delta = downloaded.saturating_sub(previous_downloaded);
        let (upload_bps, download_bps) = self
            .rate_samples
            .lock()
            .expect("runtime rate window lock poisoned")
            .record(elapsed_ms, upload_delta, download_delta);
        self.upload_bps.store(upload_bps, Ordering::Relaxed);
        self.download_bps.store(download_bps, Ordering::Relaxed);
        self.rate_window_ms
            .store(TRAFFIC_RATE_WINDOW_MS, Ordering::Relaxed);
        self.rate_sampled_at_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
        (upload_delta, download_delta)
    }
}

static RUNTIME_STATS: LazyLock<RuntimeStats> = LazyLock::new(RuntimeStats::new);
static RUNTIME_DETAILS: LazyLock<StdMutex<RuntimeDetails>> =
    LazyLock::new(|| StdMutex::new(RuntimeDetails::default()));
static ACTIVE_RUNTIME_GUARDS: LazyLock<StdMutex<usize>> = LazyLock::new(|| StdMutex::new(0));
static TRAFFIC_HISTORY: LazyLock<StdMutex<TrafficHistory>> =
    LazyLock::new(|| StdMutex::new(TrafficHistory::default()));
static RUNTIME_STATUS_CHANGES: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SSH_SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn subscribe_runtime_status_changes() -> watch::Receiver<u64> {
    RUNTIME_STATUS_CHANGES.subscribe()
}

fn notify_runtime_status_change() {
    RUNTIME_STATUS_CHANGES.send_modify(|version| *version = version.wrapping_add(1));
}

impl TrafficHistory {
    fn record(
        &mut self,
        sampled_at_unix_ms: u64,
        elapsed_ms: u64,
        uploaded_bytes: u64,
        downloaded_bytes: u64,
    ) {
        let bucket_start =
            sampled_at_unix_ms / TRAFFIC_HISTORY_BUCKET_MS * TRAFFIC_HISTORY_BUCKET_MS;
        if self
            .buckets
            .back()
            .is_some_and(|bucket| bucket.started_at_unix_ms != bucket_start)
        {
            let previous_start = self
                .buckets
                .back()
                .map(|bucket| bucket.started_at_unix_ms)
                .unwrap_or(bucket_start);
            let gap = bucket_start
                .saturating_sub(previous_start)
                .checked_div(TRAFFIC_HISTORY_BUCKET_MS)
                .unwrap_or_default();
            let missing = if gap > TRAFFIC_HISTORY_BUCKETS as u64 {
                self.buckets.clear();
                0
            } else {
                gap.saturating_sub(1)
                    .min(TRAFFIC_HISTORY_BUCKETS.saturating_sub(1) as u64)
            };
            for offset in 1..=missing {
                self.buckets.push_back(TrafficHistoryBucket {
                    started_at_unix_ms: previous_start
                        .saturating_add(offset.saturating_mul(TRAFFIC_HISTORY_BUCKET_MS)),
                    duration_ms: TRAFFIC_HISTORY_BUCKET_MS,
                    uploaded_bytes: 0,
                    downloaded_bytes: 0,
                });
            }
        }
        if self
            .buckets
            .back()
            .is_none_or(|bucket| bucket.started_at_unix_ms != bucket_start)
        {
            self.buckets.push_back(TrafficHistoryBucket {
                started_at_unix_ms: bucket_start,
                duration_ms: 0,
                uploaded_bytes: 0,
                downloaded_bytes: 0,
            });
        }
        if let Some(bucket) = self.buckets.back_mut() {
            bucket.duration_ms = bucket
                .duration_ms
                .saturating_add(elapsed_ms)
                .min(TRAFFIC_HISTORY_BUCKET_MS);
            bucket.uploaded_bytes = bucket.uploaded_bytes.saturating_add(uploaded_bytes);
            bucket.downloaded_bytes = bucket.downloaded_bytes.saturating_add(downloaded_bytes);
        }
        while self.buckets.len() > TRAFFIC_HISTORY_BUCKETS {
            self.buckets.pop_front();
        }
    }

    fn snapshot(&self) -> TrafficHistorySnapshot {
        TrafficHistorySnapshot {
            retention_hours: 24,
            bucket_seconds: TRAFFIC_HISTORY_BUCKET_MS / 1_000,
            sampled_at_unix_ms: RUNTIME_STATS
                .rate_sampled_at_unix_ms
                .load(Ordering::Relaxed),
            points: self
                .buckets
                .iter()
                .map(|bucket| {
                    let elapsed_seconds = bucket.duration_ms.max(1) as f64 / 1_000.0;
                    TrafficHistoryPoint {
                        started_at_unix_ms: bucket.started_at_unix_ms,
                        duration_ms: bucket.duration_ms,
                        upload_bps: bytes_per_second(bucket.uploaded_bytes, elapsed_seconds),
                        download_bps: bytes_per_second(bucket.downloaded_bytes, elapsed_seconds),
                        uploaded_bytes: bucket.uploaded_bytes,
                        downloaded_bytes: bucket.downloaded_bytes,
                    }
                })
                .collect(),
        }
    }
}

pub(crate) async fn run_traffic_sampler() -> anyhow::Result<()> {
    reset_rate_samples();
    let mut interval = tokio::time::interval(TRAFFIC_SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut previous = Instant::now();
    loop {
        interval.tick().await;
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(previous);
        previous = now;
        sample_traffic_rates(elapsed);
    }
}

fn reset_rate_samples() {
    RUNTIME_STATS.reset_rate_sample();
    let details = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned");
    for host in details.hosts.values() {
        host.traffic.reset_sample();
    }
    for session in details.sessions.values() {
        session.traffic.reset_sample();
    }
    for tunnel in details.tunnels.values() {
        tunnel.traffic.reset_sample();
    }
    for connection in details.connections.values() {
        connection.traffic.reset_sample();
    }
}

fn sample_traffic_rates(elapsed: Duration) {
    let elapsed_ms = u64::try_from(elapsed.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let sampled_at = unix_time_ms();
    let previous_upload_bps = RUNTIME_STATS.upload_bps.load(Ordering::Relaxed);
    let previous_download_bps = RUNTIME_STATS.download_bps.load(Ordering::Relaxed);
    let (uploaded_bytes, downloaded_bytes) = RUNTIME_STATS.sample_rate(elapsed_ms);
    let mut significant_change = rate_pair_changed_significantly(
        previous_upload_bps,
        RUNTIME_STATS.upload_bps.load(Ordering::Relaxed),
        previous_download_bps,
        RUNTIME_STATS.download_bps.load(Ordering::Relaxed),
    );
    TRAFFIC_HISTORY
        .lock()
        .expect("traffic history lock poisoned")
        .record(sampled_at, elapsed_ms, uploaded_bytes, downloaded_bytes);

    let details = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned");
    for host in details.hosts.values() {
        let active = host.state.lock().expect("host detail lock poisoned").status
            != HostRuntimeStatus::Offline;
        significant_change |= sample_or_reset(&host.traffic, active, elapsed_ms);
    }
    for session in details.sessions.values() {
        let active = session
            .state
            .lock()
            .expect("SSH session detail lock poisoned")
            .status
            != SshSessionRuntimeStatus::Offline;
        significant_change |= sample_or_reset(&session.traffic, active, elapsed_ms);
    }
    for tunnel in details.tunnels.values() {
        let active = tunnel
            .state
            .lock()
            .expect("tunnel detail lock poisoned")
            .status
            == TunnelRuntimeStatus::Listening;
        significant_change |= sample_or_reset(&tunnel.traffic, active, elapsed_ms);
    }
    for connection in details.connections.values() {
        let active = matches!(
            connection
                .state
                .lock()
                .expect("connection detail lock poisoned")
                .status,
            ConnectionRuntimeStatus::Connecting | ConnectionRuntimeStatus::Active
        );
        significant_change |= sample_or_reset(&connection.traffic, active, elapsed_ms);
    }
    drop(details);
    if significant_change {
        notify_runtime_status_change();
    }
}

fn sample_or_reset(traffic: &TrafficTotals, active: bool, elapsed_ms: u64) -> bool {
    let previous_upload_bps = traffic.upload_bps();
    let previous_download_bps = traffic.download_bps();
    if active {
        traffic.sample(elapsed_ms);
    } else {
        traffic.reset_sample();
    }
    rate_pair_changed_significantly(
        previous_upload_bps,
        traffic.upload_bps(),
        previous_download_bps,
        traffic.download_bps(),
    )
}

fn rate_pair_changed_significantly(
    previous_upload_bps: u64,
    upload_bps: u64,
    previous_download_bps: u64,
    download_bps: u64,
) -> bool {
    rate_changed_significantly(previous_upload_bps, upload_bps)
        || rate_changed_significantly(previous_download_bps, download_bps)
}

fn rate_changed_significantly(previous_bps: u64, current_bps: u64) -> bool {
    if previous_bps == current_bps {
        return false;
    }
    if previous_bps == 0 || current_bps == 0 {
        return true;
    }
    let delta = previous_bps.abs_diff(current_bps);
    delta >= RATE_PUSH_ABSOLUTE_DELTA_BPS
        || (delta >= RATE_PUSH_MIN_DELTA_BPS
            && delta.saturating_mul(100)
                >= previous_bps
                    .max(RATE_PUSH_MIN_DELTA_BPS)
                    .saturating_mul(RATE_PUSH_RELATIVE_PERCENT))
}

fn bytes_per_second(bytes: u64, elapsed_seconds: f64) -> u64 {
    (bytes as f64 / elapsed_seconds.max(0.001))
        .round()
        .clamp(0.0, u64::MAX as f64) as u64
}

pub(crate) fn next_connection_id() -> u64 {
    NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn next_ssh_session_id() -> u64 {
    NEXT_SSH_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) struct RuntimeGuard;

impl RuntimeGuard {
    pub(crate) fn start(
        configured_hosts: usize,
        configured_local_listeners: usize,
        configured_remote_listeners: usize,
    ) -> Self {
        let mut active_guards = ACTIVE_RUNTIME_GUARDS
            .lock()
            .expect("runtime guard lock poisoned");
        let first_guard = *active_guards == 0;
        *active_guards = (*active_guards)
            .checked_add(1)
            .expect("runtime guard count overflow");
        let _ = RUNTIME_STATS.config_generation.compare_exchange(
            0,
            1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        if first_guard {
            RUNTIME_STATS.running.store(true, Ordering::Relaxed);
            *RUNTIME_STATS
                .started_at
                .lock()
                .expect("runtime stats lock poisoned") = Some(Instant::now());
        }
        RUNTIME_STATS.configured_hosts.store(
            u64::try_from(configured_hosts).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        RUNTIME_STATS.configured_local_listeners.store(
            u64::try_from(configured_local_listeners).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        RUNTIME_STATS.configured_remote_listeners.store(
            u64::try_from(configured_remote_listeners).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        *RUNTIME_DETAILS
            .lock()
            .expect("runtime details lock poisoned") = RuntimeDetails::default();
        drop(active_guards);
        Self
    }

    pub(crate) fn update_configured_counts(
        &self,
        configured_hosts: usize,
        configured_local_listeners: usize,
        configured_remote_listeners: usize,
    ) {
        RUNTIME_STATS.configured_hosts.store(
            u64::try_from(configured_hosts).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        RUNTIME_STATS.configured_local_listeners.store(
            u64::try_from(configured_local_listeners).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        RUNTIME_STATS.configured_remote_listeners.store(
            u64::try_from(configured_remote_listeners).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        let mut active_guards = ACTIVE_RUNTIME_GUARDS
            .lock()
            .expect("runtime guard lock poisoned");
        *active_guards = (*active_guards)
            .checked_sub(1)
            .expect("runtime guard count underflow");
        if *active_guards != 0 {
            return;
        }
        RUNTIME_STATS.running.store(false, Ordering::Relaxed);
        *RUNTIME_STATS
            .started_at
            .lock()
            .expect("runtime stats lock poisoned") = None;
        let details = RUNTIME_DETAILS
            .lock()
            .expect("runtime details lock poisoned");
        for host in details.hosts.values() {
            host.state.lock().expect("host detail lock poisoned").status =
                HostRuntimeStatus::Offline;
        }
        for session in details.sessions.values() {
            let mut state = session
                .state
                .lock()
                .expect("SSH session detail lock poisoned");
            state.status = SshSessionRuntimeStatus::Offline;
            state.ended_at_unix_ms.get_or_insert_with(unix_time_ms);
        }
        for tunnel in details.tunnels.values() {
            tunnel
                .state
                .lock()
                .expect("tunnel detail lock poisoned")
                .status = TunnelRuntimeStatus::Stopped;
        }
        for connection in details.connections.values() {
            let mut state = connection
                .state
                .lock()
                .expect("connection detail lock poisoned");
            if !matches!(
                state.status,
                ConnectionRuntimeStatus::Closed | ConnectionRuntimeStatus::Error
            ) {
                state.status = ConnectionRuntimeStatus::Closed;
                state.ended_at_unix_ms.get_or_insert_with(unix_time_ms);
            }
            connection.traffic.clear_rate();
        }
    }
}

pub(crate) struct LocalConnectionGuard {
    connection: Option<Arc<ConnectionDetail>>,
    host: Option<Arc<HostDetail>>,
    tunnel: Option<Arc<TunnelDetail>>,
}

impl LocalConnectionGuard {
    pub(crate) fn start(registration: ConnectionRegistration) -> Self {
        RUNTIME_STATS
            .local_connections_total
            .fetch_add(1, Ordering::Relaxed);
        RUNTIME_STATS
            .local_connections_active
            .fetch_add(1, Ordering::Relaxed);
        let mut details = RUNTIME_DETAILS
            .lock()
            .expect("runtime details lock poisoned");
        let connection = RUNTIME_STATS
            .connection_capture_enabled
            .load(Ordering::Relaxed)
            .then(|| {
                let connection = Arc::new(ConnectionDetail::new(registration.clone()));
                details
                    .connections
                    .insert(registration.id, Arc::clone(&connection));
                connection
            });
        let host = details.hosts.get(&registration.host_name).cloned();
        let tunnel = details.tunnels.get(&registration.tunnel_id).cloned();
        let now = unix_time_ms();
        if let Some(host) = &host {
            host.connections_total.fetch_add(1, Ordering::Relaxed);
            host.connections_active.fetch_add(1, Ordering::Relaxed);
            host.last_activity_unix_ms.store(now, Ordering::Relaxed);
        }
        if let Some(tunnel) = &tunnel {
            tunnel.connections_total.fetch_add(1, Ordering::Relaxed);
            tunnel.connections_active.fetch_add(1, Ordering::Relaxed);
            tunnel.last_activity_unix_ms.store(now, Ordering::Relaxed);
        }
        Self {
            connection,
            host,
            tunnel,
        }
    }
}

impl Drop for LocalConnectionGuard {
    fn drop(&mut self) {
        RUNTIME_STATS
            .local_connections_active
            .fetch_sub(1, Ordering::Relaxed);
        if let Some(host) = &self.host {
            host.connections_active.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(tunnel) = &self.tunnel {
            tunnel.connections_active.fetch_sub(1, Ordering::Relaxed);
        }
        let Some(connection) = &self.connection else {
            return;
        };
        let now = unix_time_ms();
        let mut state = connection
            .state
            .lock()
            .expect("connection detail lock poisoned");
        if state.status != ConnectionRuntimeStatus::Error {
            state.status = ConnectionRuntimeStatus::Closed;
        }
        state.ended_at_unix_ms.get_or_insert(now);
        drop(state);
        connection.traffic.clear_rate();
        connection
            .last_activity_unix_ms
            .store(now, Ordering::Relaxed);
        if RUNTIME_STATS
            .connection_capture_auto_clear_closed
            .load(Ordering::Relaxed)
        {
            RUNTIME_DETAILS
                .lock()
                .expect("runtime details lock poisoned")
                .connections
                .remove(&connection.registration.id);
        } else {
            prune_captured_connections();
        }
    }
}

pub(crate) struct SshSessionGuard {
    session_id: u64,
}

impl SshSessionGuard {
    pub(crate) fn start(session_id: u64) -> Self {
        RUNTIME_STATS
            .ssh_sessions_total
            .fetch_add(1, Ordering::Relaxed);
        RUNTIME_STATS
            .ssh_sessions_active
            .fetch_add(1, Ordering::Relaxed);
        Self { session_id }
    }
}

impl Drop for SshSessionGuard {
    fn drop(&mut self) {
        RUNTIME_STATS
            .ssh_sessions_active
            .fetch_sub(1, Ordering::Relaxed);
        mark_ssh_session_offline_if_active(self.session_id);
    }
}

fn observe_ssh_channel_open_ms(milliseconds: f64) {
    let micros = (milliseconds.max(0.0) * 1000.0).round() as u64;
    RUNTIME_STATS
        .ssh_channel_open_samples
        .fetch_add(1, Ordering::Relaxed);
    RUNTIME_STATS
        .ssh_channel_open_total_micros
        .fetch_add(micros, Ordering::Relaxed);
    RUNTIME_STATS
        .ssh_channel_open_max_micros
        .fetch_max(micros, Ordering::Relaxed);
}

#[derive(Clone)]
pub(crate) struct TransferRecorder {
    record_global: bool,
    host: Option<Arc<HostDetail>>,
    tunnel: Option<Arc<TunnelDetail>>,
    session: Option<Arc<SshSessionDetail>>,
    connection: Option<Arc<ConnectionDetail>>,
}

impl TransferRecorder {
    pub(crate) fn record(&self, uploaded_bytes: u64, downloaded_bytes: u64) {
        if uploaded_bytes == 0 && downloaded_bytes == 0 {
            return;
        }
        if self.record_global {
            RUNTIME_STATS.add_transferred_bytes(uploaded_bytes, downloaded_bytes);
        }
        let now = unix_time_ms();
        if let Some(host) = &self.host {
            host.traffic.add(uploaded_bytes, downloaded_bytes);
            host.last_activity_unix_ms.store(now, Ordering::Relaxed);
        }
        if let Some(tunnel) = &self.tunnel {
            tunnel.traffic.add(uploaded_bytes, downloaded_bytes);
            tunnel.last_activity_unix_ms.store(now, Ordering::Relaxed);
        }
        if let Some(session) = &self.session {
            session.traffic.add(uploaded_bytes, downloaded_bytes);
            session.last_activity_unix_ms.store(now, Ordering::Relaxed);
        }
        if let Some(connection) = &self.connection {
            connection.traffic.add(uploaded_bytes, downloaded_bytes);
            connection
                .last_activity_unix_ms
                .store(now, Ordering::Relaxed);
        }
    }
}

pub(crate) fn tunnel_transfer_recorder(
    host_name: &str,
    tunnel_id: &str,
    connection_id: u64,
) -> TransferRecorder {
    let details = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned");
    TransferRecorder {
        record_global: true,
        host: details.hosts.get(host_name).cloned(),
        tunnel: details.tunnels.get(tunnel_id).cloned(),
        session: None,
        connection: details.connections.get(&connection_id).cloned(),
    }
}

pub(crate) fn session_transfer_recorder(session_id: u64) -> TransferRecorder {
    TransferRecorder {
        record_global: false,
        host: None,
        tunnel: None,
        session: RUNTIME_DETAILS
            .lock()
            .expect("runtime details lock poisoned")
            .sessions
            .get(&session_id)
            .cloned(),
        connection: None,
    }
}

pub(crate) fn tunnel_and_session_transfer_recorder(
    host_name: &str,
    tunnel_id: &str,
    session_id: u64,
    connection_id: u64,
) -> TransferRecorder {
    let details = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned");
    TransferRecorder {
        record_global: true,
        host: details.hosts.get(host_name).cloned(),
        tunnel: details.tunnels.get(tunnel_id).cloned(),
        session: details.sessions.get(&session_id).cloned(),
        connection: details.connections.get(&connection_id).cloned(),
    }
}

pub(crate) fn tunnel_id(host_name: &str, kind: TunnelKind, name: &str) -> String {
    format!("{host_name}/{}/{name}", kind.key())
}

pub(crate) fn register_host(registration: HostRegistration) {
    let detail = Arc::new(HostDetail::new(registration.clone()));
    RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .hosts
        .insert(registration.name, detail);
    notify_runtime_status_change();
}

pub(crate) fn update_host_state(host_name: &str, update: HostStateUpdate) {
    let host = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .hosts
        .get(host_name)
        .cloned();
    let Some(host) = host else {
        return;
    };
    let mut state = host.state.lock().expect("host detail lock poisoned");
    if state.status != update.status
        || state.rtt_ms != update.rtt_ms
        || state.restart_count != update.restart_count
        || state.last_error != update.last_error
    {
        host.last_activity_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
    }
    state.status = update.status;
    state.rtt_ms = update.rtt_ms;
    state.restart_count = update.restart_count;
    state.last_error = update.last_error;
}

pub(crate) fn register_ssh_session(registration: SshSessionRegistration) {
    let detail = Arc::new(SshSessionDetail::new(registration.clone()));
    RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .sessions
        .insert(registration.id, detail);
    notify_runtime_status_change();
}

pub(crate) fn update_ssh_session_state(session_id: u64, update: SshSessionStateUpdate) {
    let session = session_detail(session_id);
    let Some(session) = session else {
        return;
    };
    let now = unix_time_ms();
    let became_offline;
    let changed;
    {
        let mut state = session
            .state
            .lock()
            .expect("SSH session detail lock poisoned");
        changed = state.status != update.status
            || state.startup_ms != update.startup_ms
            || state.rtt_ms != update.rtt_ms
            || state.retiring != update.retiring
            || state.remote_forward_owner != update.remote_forward_owner
            || state.last_error != update.last_error;
        if changed {
            session.last_activity_unix_ms.store(now, Ordering::Relaxed);
        }
        if update.status == SshSessionRuntimeStatus::Healthy
            && state.established_at_unix_ms.is_none()
        {
            state.established_at_unix_ms = Some(now);
        }
        became_offline = update.status == SshSessionRuntimeStatus::Offline;
        if became_offline && state.ended_at_unix_ms.is_none() {
            state.ended_at_unix_ms = Some(now);
        }
        state.status = update.status;
        state.startup_ms = update.startup_ms;
        state.rtt_ms = update.rtt_ms;
        state.retiring = update.retiring;
        state.remote_forward_owner = update.remote_forward_owner;
        state.last_error = update.last_error;
    }
    if became_offline {
        prune_offline_sessions(&session.registration.host_name);
    }
    if changed {
        notify_runtime_status_change();
    }
}

pub(crate) fn remove_ssh_session(session_id: u64) {
    let removed = remove_ssh_session_detail(
        &mut RUNTIME_DETAILS
            .lock()
            .expect("runtime details lock poisoned"),
        session_id,
    );
    if removed {
        notify_runtime_status_change();
    }
}

fn remove_ssh_session_detail(details: &mut RuntimeDetails, session_id: u64) -> bool {
    details.sessions.remove(&session_id).is_some()
}

pub(crate) fn record_ssh_session_probe(session_id: u64) {
    if let Some(session) = session_detail(session_id) {
        let now = unix_time_ms();
        session.last_probe_unix_ms.store(now, Ordering::Relaxed);
        session.last_activity_unix_ms.store(now, Ordering::Relaxed);
    }
}

pub(crate) fn ssh_session_channel_reserved(session_id: u64) {
    if let Some(session) = session_detail(session_id) {
        session.active_channels.fetch_add(1, Ordering::Relaxed);
        session
            .last_activity_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
    }
}

pub(crate) fn ssh_session_channel_accepted(session_id: u64) {
    ssh_session_channel_reserved(session_id);
    if let Some(session) = session_detail(session_id) {
        session.channels_total.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn ssh_session_channel_closed(session_id: u64) {
    if let Some(session) = session_detail(session_id) {
        session.active_channels.fetch_sub(1, Ordering::Relaxed);
        session
            .last_activity_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
    }
}

pub(crate) fn record_ssh_session_channel_open(session_id: u64, milliseconds: f64) {
    observe_ssh_channel_open_ms(milliseconds);
    if let Some(session) = session_detail(session_id) {
        session.channels_total.fetch_add(1, Ordering::Relaxed);
        session
            .last_activity_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
    }
}

pub(crate) fn record_ssh_session_channel_error(session_id: u64, error: &str) {
    record_error();
    if let Some(session) = session_detail(session_id) {
        session
            .channel_open_errors_total
            .fetch_add(1, Ordering::Relaxed);
        session
            .last_activity_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
        session
            .state
            .lock()
            .expect("SSH session detail lock poisoned")
            .last_error = Some(error.to_string());
        record_host_detail_error(&session.registration.host_name, error);
    }
}

pub(crate) fn register_tunnel(registration: TunnelRegistration) {
    let detail = Arc::new(TunnelDetail::new(registration.clone()));
    RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .tunnels
        .insert(registration.id, detail);
    notify_runtime_status_change();
}

pub(crate) fn update_tunnel_status(
    tunnel_id: &str,
    status: TunnelRuntimeStatus,
    owner_session_id: Option<u64>,
    error: Option<String>,
) {
    let tunnel = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .tunnels
        .get(tunnel_id)
        .cloned();
    let Some(tunnel) = tunnel else {
        return;
    };
    let mut state = tunnel.state.lock().expect("tunnel detail lock poisoned");
    let changed = state.status != status
        || state.owner_session_id != owner_session_id
        || state.last_error != error;
    state.status = status;
    state.owner_session_id = owner_session_id;
    state.last_error = error;
    tunnel
        .last_activity_unix_ms
        .store(unix_time_ms(), Ordering::Relaxed);
    drop(state);
    if changed {
        notify_runtime_status_change();
    }
}

pub(crate) fn update_connection_route(
    connection_id: u64,
    target: Option<String>,
    protocol: Option<String>,
) {
    let Some(connection) = connection_detail(connection_id) else {
        return;
    };
    let mut state = connection
        .state
        .lock()
        .expect("connection detail lock poisoned");
    if target.is_some() {
        state.target = target;
    }
    if protocol.is_some() {
        state.protocol = protocol;
    }
    connection
        .last_activity_unix_ms
        .store(unix_time_ms(), Ordering::Relaxed);
}

pub(crate) fn associate_connection_session(connection_id: u64, session_id: u64) {
    let Some(connection) = connection_detail(connection_id) else {
        return;
    };
    connection
        .state
        .lock()
        .expect("connection detail lock poisoned")
        .session_id = Some(session_id);
    connection
        .last_activity_unix_ms
        .store(unix_time_ms(), Ordering::Relaxed);
}

pub(crate) fn mark_connection_active(connection_id: u64) {
    let Some(connection) = connection_detail(connection_id) else {
        return;
    };
    let now = unix_time_ms();
    let mut state = connection
        .state
        .lock()
        .expect("connection detail lock poisoned");
    state.status = ConnectionRuntimeStatus::Active;
    state.established_at_unix_ms.get_or_insert(now);
    connection
        .last_activity_unix_ms
        .store(now, Ordering::Relaxed);
}

pub(crate) fn record_connection_error(connection_id: u64, error: &str, terminal: bool) {
    let Some(connection) = connection_detail(connection_id) else {
        return;
    };
    let now = unix_time_ms();
    connection.errors_total.fetch_add(1, Ordering::Relaxed);
    connection
        .last_activity_unix_ms
        .store(now, Ordering::Relaxed);
    let mut state = connection
        .state
        .lock()
        .expect("connection detail lock poisoned");
    state.last_error = Some(error.to_string());
    if terminal {
        state.status = ConnectionRuntimeStatus::Error;
        state.ended_at_unix_ms.get_or_insert(now);
        connection.traffic.clear_rate();
    }
}

pub(crate) fn record_tunnel_error(host_name: &str, tunnel_id: &str, error: &str) {
    record_error();
    record_host_detail_error(host_name, error);
    let tunnel = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .tunnels
        .get(tunnel_id)
        .cloned();
    if let Some(tunnel) = tunnel {
        tunnel.errors_total.fetch_add(1, Ordering::Relaxed);
        tunnel
            .last_activity_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
        let mut state = tunnel.state.lock().expect("tunnel detail lock poisoned");
        state.last_error = Some(error.to_string());
    }
}

pub(crate) fn record_host_error(host_name: &str, error: &str) {
    record_error();
    record_host_detail_error(host_name, error);
}

fn session_detail(session_id: u64) -> Option<Arc<SshSessionDetail>> {
    RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .sessions
        .get(&session_id)
        .cloned()
}

fn connection_detail(connection_id: u64) -> Option<Arc<ConnectionDetail>> {
    RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .connections
        .get(&connection_id)
        .cloned()
}

fn mark_ssh_session_offline_if_active(session_id: u64) {
    let Some(session) = session_detail(session_id) else {
        return;
    };
    let now = unix_time_ms();
    let mut state = session
        .state
        .lock()
        .expect("SSH session detail lock poisoned");
    if state.status != SshSessionRuntimeStatus::Offline {
        state.status = SshSessionRuntimeStatus::Offline;
        state.ended_at_unix_ms = Some(now);
        session.last_activity_unix_ms.store(now, Ordering::Relaxed);
    }
}

fn prune_offline_sessions(host_name: &str) {
    const RETAINED_OFFLINE_SESSIONS: usize = 8;
    let mut details = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned");
    let mut offline = details
        .sessions
        .iter()
        .filter_map(|(id, session)| {
            if session.registration.host_name != host_name {
                return None;
            }
            let status = session
                .state
                .lock()
                .expect("SSH session detail lock poisoned")
                .status;
            (status == SshSessionRuntimeStatus::Offline).then_some(*id)
        })
        .collect::<Vec<_>>();
    offline.sort_unstable();
    let remove_count = offline.len().saturating_sub(RETAINED_OFFLINE_SESSIONS);
    for id in offline.into_iter().take(remove_count) {
        details.sessions.remove(&id);
    }
}

fn prune_captured_connections() {
    const MAX_CAPTURED_CONNECTIONS: usize = 10_000;
    let mut details = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned");
    let remove_count = details
        .connections
        .len()
        .saturating_sub(MAX_CAPTURED_CONNECTIONS);
    if remove_count == 0 {
        return;
    }
    let mut closed = details
        .connections
        .iter()
        .filter_map(|(id, connection)| {
            let status = connection
                .state
                .lock()
                .expect("connection detail lock poisoned")
                .status;
            matches!(
                status,
                ConnectionRuntimeStatus::Closed | ConnectionRuntimeStatus::Error
            )
            .then_some(*id)
        })
        .collect::<Vec<_>>();
    closed.sort_unstable();
    for id in closed.into_iter().take(remove_count) {
        details.connections.remove(&id);
    }
}

fn record_host_detail_error(host_name: &str, error: &str) {
    let host = RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .hosts
        .get(host_name)
        .cloned();
    if let Some(host) = host {
        host.errors_total.fetch_add(1, Ordering::Relaxed);
        host.last_activity_unix_ms
            .store(unix_time_ms(), Ordering::Relaxed);
        host.state
            .lock()
            .expect("host detail lock poisoned")
            .last_error = Some(error.to_string());
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn non_zero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

pub(crate) fn record_error() {
    RUNTIME_STATS.errors_total.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_config_reload_success() {
    RUNTIME_STATS
        .config_reloads_total
        .fetch_add(1, Ordering::Relaxed);
    RUNTIME_STATS
        .config_generation
        .fetch_add(1, Ordering::Relaxed);
    notify_runtime_status_change();
}

pub(crate) fn record_config_reload_error() {
    RUNTIME_STATS
        .config_reload_errors_total
        .fetch_add(1, Ordering::Relaxed);
    record_error();
    notify_runtime_status_change();
}

pub fn runtime_snapshot() -> RuntimeSnapshot {
    RUNTIME_STATS.snapshot()
}

pub fn traffic_history_snapshot() -> TrafficHistorySnapshot {
    TRAFFIC_HISTORY
        .lock()
        .expect("traffic history lock poisoned")
        .snapshot()
}

pub fn set_connection_capture_recording(recording: bool) {
    RUNTIME_STATS
        .connection_capture_enabled
        .store(recording, Ordering::Relaxed);
    notify_runtime_status_change();
}

pub fn clear_captured_connections() {
    RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .connections
        .clear();
    notify_runtime_status_change();
}

pub fn set_connection_capture_auto_clear_closed(enabled: bool) {
    RUNTIME_STATS
        .connection_capture_auto_clear_closed
        .store(enabled, Ordering::Relaxed);
    if !enabled {
        notify_runtime_status_change();
        return;
    }
    RUNTIME_DETAILS
        .lock()
        .expect("runtime details lock poisoned")
        .connections
        .retain(|_, connection| {
            matches!(
                connection
                    .state
                    .lock()
                    .expect("connection detail lock poisoned")
                    .status,
                ConnectionRuntimeStatus::Connecting | ConnectionRuntimeStatus::Active
            )
        });
    notify_runtime_status_change();
}

fn micros_to_ms(micros: f64) -> f64 {
    ((micros / 1000.0) * 1000.0).round() / 1000.0
}

pub(crate) fn elapsed_ms(started: Instant) -> f64 {
    duration_ms(started.elapsed())
}

pub(crate) fn duration_ms(duration: Duration) -> f64 {
    let millis = duration.as_secs_f64() * 1000.0;
    (millis * 1000.0).round() / 1000.0
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IoTiming {
    pub first_read_ms: Option<f64>,
    pub first_write_ms: Option<f64>,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub total_ms: f64,
}

pub(crate) struct TimedIo<S> {
    inner: S,
    started: Instant,
    first_read: Option<Duration>,
    first_write: Option<Duration>,
    bytes_read: u64,
    bytes_written: u64,
    transfer_recorder: Option<TransferRecorder>,
}

impl<S> TimedIo<S> {
    #[cfg(test)]
    pub fn new(inner: S, started: Instant) -> Self {
        Self {
            inner,
            started,
            first_read: None,
            first_write: None,
            bytes_read: 0,
            bytes_written: 0,
            transfer_recorder: None,
        }
    }

    pub fn with_transfer_recorder(
        inner: S,
        started: Instant,
        transfer_recorder: TransferRecorder,
    ) -> Self {
        Self {
            inner,
            started,
            first_read: None,
            first_write: None,
            bytes_read: 0,
            bytes_written: 0,
            transfer_recorder: Some(transfer_recorder),
        }
    }

    pub fn timing(&self) -> IoTiming {
        IoTiming {
            first_read_ms: self.first_read.map(duration_ms),
            first_write_ms: self.first_write.map(duration_ms),
            bytes_read: self.bytes_read,
            bytes_written: self.bytes_written,
            total_ms: elapsed_ms(self.started),
        }
    }
}

impl<S> AsyncRead for TimedIo<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = &result {
            let read = buffer.filled().len().saturating_sub(before);
            if read > 0 {
                self.bytes_read = self.bytes_read.saturating_add(read as u64);
                if let Some(recorder) = &self.transfer_recorder {
                    recorder.record(0, read as u64);
                }
                let elapsed = self.started.elapsed();
                self.first_read.get_or_insert(elapsed);
            }
        }
        result
    }
}

impl<S> AsyncWrite for TimedIo<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(context, buffer);
        if let Poll::Ready(Ok(written)) = result {
            if written > 0 {
                self.bytes_written = self.bytes_written.saturating_add(written as u64);
                if let Some(recorder) = &self.transfer_recorder {
                    recorder.record(written as u64, 0);
                }
                let elapsed = self.started.elapsed();
                self.first_write.get_or_insert(elapsed);
            }
            Poll::Ready(Ok(written))
        } else {
            result
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BodyTiming {
    pub first_data_ms: Option<f64>,
    pub bytes: u64,
    pub total_ms: f64,
    pub outcome: &'static str,
}

pub(crate) struct TimedBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    inner: B,
    started: Instant,
    first_data: Option<Duration>,
    bytes: u64,
    on_complete: Option<Box<dyn FnOnce(BodyTiming) + Send + 'static>>,
}

impl<B> TimedBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    pub fn new(
        inner: B,
        started: Instant,
        on_complete: impl FnOnce(BodyTiming) + Send + 'static,
    ) -> Self {
        Self {
            inner,
            started,
            first_data: None,
            bytes: 0,
            on_complete: Some(Box::new(on_complete)),
        }
    }

    fn finish(&mut self, outcome: &'static str) {
        let Some(on_complete) = self.on_complete.take() else {
            return;
        };
        on_complete(BodyTiming {
            first_data_ms: self.first_data.map(duration_ms),
            bytes: self.bytes,
            total_ms: elapsed_ms(self.started),
            outcome,
        });
    }
}

impl<B> Body for TimedBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(context);
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref()
                    && !data.is_empty()
                {
                    self.bytes = self.bytes.saturating_add(data.len() as u64);
                    let elapsed = self.started.elapsed();
                    self.first_data.get_or_insert(elapsed);
                }
            }
            Poll::Ready(Some(Err(_))) => self.finish("error"),
            Poll::Ready(None) => self.finish("completed"),
            Poll::Pending => {}
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for TimedBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    fn drop(&mut self) {
        let outcome = if self.inner.is_end_stream() {
            "completed"
        } else {
            "dropped"
        };
        self.finish(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[test]
    fn transferred_bytes_are_recorded_by_direction() {
        let stats = RuntimeStats::new();

        stats.add_transferred_bytes(3, 5);
        stats.add_transferred_bytes(7, 11);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.uploaded_bytes_total, 10);
        assert_eq!(snapshot.downloaded_bytes_total, 16);
        assert_eq!(snapshot.transferred_bytes_total, 26);
    }

    #[test]
    fn traffic_totals_sample_bytes_per_second_and_reset_the_rate() {
        let traffic = TrafficTotals::default();
        traffic.reset_sample();
        traffic.add(2_000, 6_000);

        assert_eq!(traffic.sample(2_000), (2_000, 6_000));
        assert_eq!(traffic.upload_bps(), 1_000);
        assert_eq!(traffic.download_bps(), 3_000);

        traffic.reset_sample();
        assert_eq!(traffic.sample(1_000), (0, 0));
        assert_eq!(traffic.upload_bps(), 0);
        assert_eq!(traffic.download_bps(), 0);
    }

    #[test]
    fn traffic_rate_uses_a_one_second_window_updated_every_quarter_second() {
        let traffic = TrafficTotals::default();
        traffic.reset_sample();
        for expected in [250, 500, 750, 1_000] {
            traffic.add(250, 500);
            traffic.sample(250);
            assert_eq!(traffic.upload_bps(), expected);
            assert_eq!(traffic.download_bps(), expected * 2);
        }

        traffic.sample(250);
        assert_eq!(traffic.upload_bps(), 750);
        assert_eq!(traffic.download_bps(), 1_500);
    }

    #[test]
    fn runtime_snapshot_reports_the_one_second_rate_window() {
        let stats = RuntimeStats::new();
        stats.reset_rate_sample();
        stats.add_transferred_bytes(250, 500);
        stats.sample_rate(250);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.rate_window_ms, 1_000);
        assert_eq!(snapshot.upload_bps, 250);
        assert_eq!(snapshot.download_bps, 500);
    }

    #[test]
    fn status_push_rate_threshold_detects_material_changes() {
        assert!(!rate_changed_significantly(10_000, 10_000));
        assert!(rate_changed_significantly(0, 1));
        assert!(rate_changed_significantly(1, 0));
        assert!(rate_changed_significantly(10_000, 12_500));
        assert!(!rate_changed_significantly(10_000, 11_000));
        assert!(rate_changed_significantly(1_000_000, 1_070_000));
    }

    #[test]
    fn traffic_history_averages_samples_within_a_minute() {
        let mut history = TrafficHistory::default();
        let bucket_start = 1_800_000;
        history.record(bucket_start + 1_000, 1_000, 1_000, 2_000);
        history.record(bucket_start + 2_000, 2_000, 6_000, 3_000);

        let snapshot = history.snapshot();
        assert_eq!(snapshot.retention_hours, 24);
        assert_eq!(snapshot.bucket_seconds, 60);
        assert_eq!(snapshot.points.len(), 1);
        assert_eq!(snapshot.points[0].started_at_unix_ms, bucket_start);
        assert_eq!(snapshot.points[0].duration_ms, 3_000);
        assert_eq!(snapshot.points[0].uploaded_bytes, 7_000);
        assert_eq!(snapshot.points[0].downloaded_bytes, 5_000);
        assert_eq!(snapshot.points[0].upload_bps, 2_333);
        assert_eq!(snapshot.points[0].download_bps, 1_667);
    }

    #[test]
    fn traffic_history_retains_exactly_twenty_four_hours() {
        let mut history = TrafficHistory::default();
        let first_bucket = 3_600_000;
        for index in 0..=TRAFFIC_HISTORY_BUCKETS {
            history.record(
                first_bucket + index as u64 * TRAFFIC_HISTORY_BUCKET_MS,
                TRAFFIC_HISTORY_BUCKET_MS,
                60,
                120,
            );
        }

        let snapshot = history.snapshot();
        assert_eq!(snapshot.points.len(), TRAFFIC_HISTORY_BUCKETS);
        assert_eq!(
            snapshot.points[0].started_at_unix_ms,
            first_bucket + TRAFFIC_HISTORY_BUCKET_MS
        );
        assert_eq!(snapshot.points.last().unwrap().upload_bps, 1);
        assert_eq!(snapshot.points.last().unwrap().download_bps, 2);
    }

    #[test]
    fn traffic_history_fills_idle_minute_gaps() {
        let mut history = TrafficHistory::default();
        let first_bucket = 7_200_000;
        history.record(first_bucket, 1_000, 1_000, 2_000);
        history.record(
            first_bucket + 3 * TRAFFIC_HISTORY_BUCKET_MS,
            1_000,
            3_000,
            4_000,
        );

        let snapshot = history.snapshot();
        assert_eq!(snapshot.points.len(), 4);
        for point in &snapshot.points[1..3] {
            assert_eq!(point.duration_ms, TRAFFIC_HISTORY_BUCKET_MS);
            assert_eq!(point.upload_bps, 0);
            assert_eq!(point.download_bps, 0);
            assert_eq!(point.uploaded_bytes, 0);
            assert_eq!(point.downloaded_bytes, 0);
        }
    }

    #[test]
    fn connection_traffic_is_recorded_once_at_every_runtime_level() {
        let host = Arc::new(HostDetail::new(HostRegistration {
            name: "stats-host".to_string(),
            ssh_alias: "stats-alias".to_string(),
            address: "127.0.0.1:22".to_string(),
            min_sessions: 1,
            max_sessions: 2,
        }));
        let session = Arc::new(SshSessionDetail::new(SshSessionRegistration {
            id: 7,
            host_name: "stats-host".to_string(),
            ssh_alias: "stats-alias".to_string(),
            address: "127.0.0.1:22".to_string(),
        }));
        let tunnel = Arc::new(TunnelDetail::new(TunnelRegistration {
            id: "stats-host/local-forward/test".to_string(),
            host_name: "stats-host".to_string(),
            name: "test".to_string(),
            kind: TunnelKind::LocalForward,
            listen: "127.0.0.1:10080".to_string(),
            target: Some("example.com:443".to_string()),
            protocol: Some("TCP".to_string()),
        }));
        let connection = Arc::new(ConnectionDetail::new(ConnectionRegistration {
            id: 99,
            host_name: "stats-host".to_string(),
            tunnel_id: "stats-host/local-forward/test".to_string(),
            peer_address: "127.0.0.1:50000".to_string(),
            target: Some("example.com:443".to_string()),
            protocol: Some("TCP".to_string()),
            session_id: Some(7),
        }));
        {
            let mut state = connection.state.lock().unwrap();
            state.status = ConnectionRuntimeStatus::Active;
            state.established_at_unix_ms = Some(connection.created_at_unix_ms);
        }
        let recorder = TransferRecorder {
            record_global: false,
            host: Some(Arc::clone(&host)),
            tunnel: Some(Arc::clone(&tunnel)),
            session: Some(Arc::clone(&session)),
            connection: Some(Arc::clone(&connection)),
        };

        recorder.record(900, 300);
        host.traffic.sample(1_000);
        tunnel.traffic.sample(1_000);
        session.traffic.sample(1_000);
        connection.traffic.sample(1_000);

        assert_eq!(host.traffic.uploaded(), 900);
        assert_eq!(tunnel.traffic.uploaded(), 900);
        assert_eq!(session.traffic.uploaded(), 900);
        let active = connection.snapshot(connection.created_at_unix_ms + 1_000);
        assert_eq!(active.status, ConnectionRuntimeStatus::Active);
        assert_eq!(active.session_id, Some(7));
        assert_eq!(active.upload_bps, 900);
        assert_eq!(active.download_bps, 300);
        assert_eq!(active.uploaded_bytes_total, 900);
        assert_eq!(active.downloaded_bytes_total, 300);

        {
            let mut state = connection.state.lock().unwrap();
            state.status = ConnectionRuntimeStatus::Closed;
            state.ended_at_unix_ms = Some(connection.created_at_unix_ms + 2_000);
        }
        connection.traffic.clear_rate();
        let closed = connection.snapshot(connection.created_at_unix_ms + 3_000);
        assert_eq!(closed.status, ConnectionRuntimeStatus::Closed);
        assert_eq!(closed.uptime_ms, 2_000);
        assert_eq!(closed.upload_bps, 0);
        assert_eq!(closed.download_bps, 0);
    }

    #[test]
    fn removed_ssh_session_is_deleted_from_runtime_details() {
        let mut details = RuntimeDetails::default();
        details.sessions.insert(
            42,
            Arc::new(SshSessionDetail::new(SshSessionRegistration {
                id: 42,
                host_name: "removed-session-host".to_string(),
                ssh_alias: "removed-session-alias".to_string(),
                address: "127.0.0.1:22".to_string(),
            })),
        );

        assert!(remove_ssh_session_detail(&mut details, 42));
        assert!(!details.sessions.contains_key(&42));
        assert!(!remove_ssh_session_detail(&mut details, 42));
    }

    #[test]
    fn detailed_snapshot_groups_sessions_and_tunnels_under_their_host() {
        let host = Arc::new(HostDetail::new(HostRegistration {
            name: "test-host".to_string(),
            ssh_alias: "test-alias".to_string(),
            address: "127.0.0.1:22".to_string(),
            min_sessions: 2,
            max_sessions: 4,
        }));
        host.state.lock().unwrap().status = HostRuntimeStatus::Healthy;
        host.traffic.add(30, 70);

        let session = Arc::new(SshSessionDetail::new(SshSessionRegistration {
            id: 42,
            host_name: "test-host".to_string(),
            ssh_alias: "test-alias".to_string(),
            address: "127.0.0.1:22".to_string(),
        }));
        {
            let mut state = session.state.lock().unwrap();
            state.status = SshSessionRuntimeStatus::Healthy;
            state.established_at_unix_ms = Some(session.created_at_unix_ms + 10);
            state.startup_ms = Some(12.5);
            state.rtt_ms = Some(8);
        }
        session.channels_total.store(3, Ordering::Relaxed);
        session.active_channels.store(1, Ordering::Relaxed);
        session.traffic.add(20, 40);

        let tunnel = Arc::new(TunnelDetail::new(TunnelRegistration {
            id: "test-host/local-proxy/proxy".to_string(),
            host_name: "test-host".to_string(),
            name: "proxy".to_string(),
            kind: TunnelKind::LocalProxy,
            listen: "127.0.0.1:1080".to_string(),
            target: None,
            protocol: Some("Mixed".to_string()),
        }));
        tunnel.state.lock().unwrap().status = TunnelRuntimeStatus::Listening;
        tunnel.connections_total.store(5, Ordering::Relaxed);
        tunnel.traffic.add(30, 70);

        let mut details = RuntimeDetails::default();
        details.hosts.insert("test-host".to_string(), host);
        details.sessions.insert(42, session);
        details
            .tunnels
            .insert("test-host/local-proxy/proxy".to_string(), tunnel);

        let snapshot = details.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].uploaded_bytes_total, 30);
        assert_eq!(snapshot[0].sessions.len(), 1);
        assert_eq!(snapshot[0].sessions[0].id, 42);
        assert_eq!(snapshot[0].sessions[0].active_channels, 1);
        assert_eq!(snapshot[0].tunnels.len(), 1);
        assert_eq!(snapshot[0].tunnels[0].connections_total, 5);
    }

    #[tokio::test]
    async fn timed_io_records_first_read_write_and_bytes() {
        let (client, mut peer) = duplex(64);
        let started = Instant::now();
        let mut timed = TimedIo::new(client, started);

        timed.write_all(b"up").await.unwrap();
        let mut uploaded = [0_u8; 2];
        peer.read_exact(&mut uploaded).await.unwrap();
        peer.write_all(b"down").await.unwrap();
        let mut downloaded = [0_u8; 4];
        timed.read_exact(&mut downloaded).await.unwrap();

        let timing = timed.timing();
        assert_eq!(timing.bytes_written, 2);
        assert_eq!(timing.bytes_read, 4);
        assert!(timing.first_write_ms.is_some());
        assert!(timing.first_read_ms.is_some());
    }

    #[tokio::test]
    async fn timed_body_records_completion_and_bytes() {
        let observed = Arc::new(Mutex::new(None));
        let observed_for_callback = Arc::clone(&observed);
        let body = TimedBody::new(
            Full::new(Bytes::from_static(b"body")),
            Instant::now(),
            move |timing| {
                *observed_for_callback.lock().unwrap() = Some(timing);
            },
        );

        let collected = body.collect().await.unwrap().to_bytes();
        assert_eq!(collected, b"body"[..]);
        let timing = observed.lock().unwrap().unwrap();
        assert_eq!(timing.bytes, 4);
        assert_eq!(timing.outcome, "completed");
        assert!(timing.first_data_ms.is_some());
    }

    #[test]
    fn timed_empty_body_is_completed_when_dropped_without_polling() {
        let observed = Arc::new(Mutex::new(None));
        let observed_for_callback = Arc::clone(&observed);
        let body = TimedBody::new(Full::new(Bytes::new()), Instant::now(), move |timing| {
            *observed_for_callback.lock().unwrap() = Some(timing);
        });

        drop(body);
        let timing = observed.lock().unwrap().unwrap();
        assert_eq!(timing.bytes, 0);
        assert_eq!(timing.outcome, "completed");
    }

    #[tokio::test]
    async fn timed_exact_body_is_completed_when_dropped_after_last_frame() {
        let observed = Arc::new(Mutex::new(None));
        let observed_for_callback = Arc::clone(&observed);
        let mut body = TimedBody::new(
            Full::new(Bytes::from_static(b"done")),
            Instant::now(),
            move |timing| {
                *observed_for_callback.lock().unwrap() = Some(timing);
            },
        );

        let frame = body.frame().await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), b"done"[..]);
        drop(body);

        let timing = observed.lock().unwrap().unwrap();
        assert_eq!(timing.bytes, 4);
        assert_eq!(timing.outcome, "completed");
    }
}
