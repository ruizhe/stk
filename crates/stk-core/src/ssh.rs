use crate::{
    config::{
        ProbeConfig, ProxyProtocol, SshAuthConfig, SshHostConfig, SshPoolConfig,
        SshRemoteForwardConfig,
    },
    engine::{ProxySessionContext, handle_proxy_session},
    health::{HealthStatus, LoadBalancer, SshHostState},
    network::{ConnectivityHandle, ConnectivitySnapshot, NetworkAvailability},
    outbound::{BoxedProxyStream, DialContext, LocalTcpDialer, OutboundDialer, TargetAddr},
    ssh_config::{
        ResolvedHostKeyPolicy, ResolvedSshEndpoint, ResolvedSshPlan, expand_proxy_command,
        resolve_ssh_plan,
    },
    stats::{self, elapsed_ms, next_connection_id},
};
use anyhow::{Context, bail};
use async_trait::async_trait;
use russh::{
    Channel, ChannelOpenFailure, Disconnect,
    client::{self, DisconnectReason},
    keys::{
        self,
        agent::{AgentIdentity, client::AgentClient, client::AgentStream},
        key::PrivateKeyWithHashAlg,
    },
};
use std::{
    collections::{HashMap, HashSet},
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll, Waker},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf, copy_bidirectional},
    net::TcpStream,
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Notify, RwLock, mpsc, watch},
    task::{AbortHandle, JoinSet},
    time::{MissedTickBehavior, interval, sleep, timeout},
};
use tracing::{Instrument, debug, info, info_span, warn};

const MAX_CONCURRENT_SESSION_STARTS_PER_HOST: usize = 2;
const SESSION_SCALE_UP_UTILIZATION_NUMERATOR: usize = 3;
const SESSION_SCALE_UTILIZATION_DENOMINATOR: usize = 4;
const SESSION_SCALE_DOWN_UTILIZATION_NUMERATOR: usize = 1;

#[cfg(not(test))]
const SESSION_MANAGER_TICK_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const SESSION_MANAGER_TICK_INTERVAL: Duration = Duration::from_millis(20);

#[cfg(not(test))]
const OFFLINE_REMOTE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(test)]
const OFFLINE_REMOTE_REFRESH_INTERVAL: Duration = Duration::from_millis(40);

#[cfg(not(test))]
const DORMANT_SESSION_MANAGER_INTERVAL: Duration = Duration::from_secs(60 * 60);
#[cfg(test)]
const DORMANT_SESSION_MANAGER_INTERVAL: Duration = Duration::from_secs(5);

#[cfg(not(test))]
const RESUME_SETTLE_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(test)]
const RESUME_SETTLE_INTERVAL: Duration = Duration::from_millis(40);

const SUSPECTED_SUSPEND_GAP: Duration = Duration::from_secs(5);

#[cfg(not(test))]
const SESSION_SCALE_UP_SUSTAINED_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(test)]
const SESSION_SCALE_UP_SUSTAINED_INTERVAL: Duration = Duration::from_millis(40);

#[cfg(not(test))]
const SESSION_SCALE_DOWN_IDLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
#[cfg(test)]
const SESSION_SCALE_DOWN_IDLE_INTERVAL: Duration = Duration::from_millis(200);

#[cfg(not(test))]
const SESSION_SCALE_DOWN_STEP_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(test)]
const SESSION_SCALE_DOWN_STEP_INTERVAL: Duration = Duration::from_millis(40);

fn initial_session_start_count(minimum: usize) -> usize {
    minimum.min(MAX_CONCURRENT_SESSION_STARTS_PER_HOST)
}

fn session_recovery_poll_interval(plan: &ResolvedSshPlan) -> Duration {
    plan.jumps
        .iter()
        .chain(std::iter::once(&plan.target))
        .fold(Duration::ZERO, |total, endpoint| {
            total.saturating_add(endpoint.connect_timeout.saturating_mul(2))
        })
        .max(Duration::from_secs(1))
}

fn can_start_another_session(connecting: usize) -> bool {
    connecting < MAX_CONCURRENT_SESSION_STARTS_PER_HOST
}

fn session_spawn_authorized(
    connectivity: ConnectivitySnapshot,
    requires_remote_availability: bool,
    demand_pending: bool,
    startup_spawn_budget: usize,
) -> bool {
    match connectivity.availability {
        NetworkAvailability::Offline => false,
        NetworkAvailability::Online => {
            connectivity.events_available
                || requires_remote_availability
                || demand_pending
                || startup_spawn_budget > 0
        }
        NetworkAvailability::Unknown => {
            requires_remote_availability || demand_pending || startup_spawn_budget > 0
        }
    }
}

fn session_manager_tick_interval(
    connectivity: ConnectivitySnapshot,
    requires_remote_availability: bool,
    background_work_enabled: bool,
) -> Duration {
    if connectivity.is_offline() {
        if requires_remote_availability {
            OFFLINE_REMOTE_REFRESH_INTERVAL
        } else {
            DORMANT_SESSION_MANAGER_INTERVAL
        }
    } else if background_work_enabled {
        SESSION_MANAGER_TICK_INTERVAL
    } else {
        DORMANT_SESSION_MANAGER_INTERVAL
    }
}

fn new_session_manager_ticker(period: Duration) -> tokio::time::Interval {
    let mut ticker = interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker
}

fn new_delayed_session_manager_ticker(period: Duration) -> tokio::time::Interval {
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker
}

fn new_session_manager_ticker_not_before(
    period: Duration,
    not_before: Option<Instant>,
) -> tokio::time::Interval {
    let delay = session_manager_ticker_delay(not_before, Instant::now());
    if delay.is_zero() {
        new_session_manager_ticker(period)
    } else {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + delay, period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker
    }
}

fn session_manager_ticker_delay(not_before: Option<Instant>, now: Instant) -> Duration {
    not_before
        .map(|deadline| deadline.saturating_duration_since(now))
        .unwrap_or_default()
}

fn session_pool_is_under_pressure(
    in_flight: usize,
    healthy_sessions: usize,
    max_channels_per_session: usize,
) -> bool {
    healthy_sessions > 0
        && (in_flight as u128).saturating_mul(SESSION_SCALE_UTILIZATION_DENOMINATOR as u128)
            >= (healthy_sessions as u128)
                .saturating_mul(max_channels_per_session as u128)
                .saturating_mul(SESSION_SCALE_UP_UTILIZATION_NUMERATOR as u128)
}

fn session_pool_can_scale_down(
    in_flight: usize,
    healthy_sessions: usize,
    minimum_sessions: usize,
    max_channels_per_session: usize,
) -> bool {
    if healthy_sessions <= minimum_sessions {
        return false;
    }
    let capacity_after_scale_down = (healthy_sessions - 1).saturating_mul(max_channels_per_session);
    (in_flight as u128).saturating_mul(SESSION_SCALE_UTILIZATION_DENOMINATOR as u128)
        <= (capacity_after_scale_down as u128)
            .saturating_mul(SESSION_SCALE_DOWN_UTILIZATION_NUMERATOR as u128)
}

fn session_pool_requires_forced_turnover(
    active_sessions: usize,
    retiring_sessions: usize,
    maximum_sessions: usize,
    replacement_capacity_blocked: bool,
    forced_turnover_in_progress: bool,
) -> bool {
    replacement_capacity_blocked
        && active_sessions >= maximum_sessions
        && active_sessions > 0
        && retiring_sessions == active_sessions
        && !forced_turnover_in_progress
}

fn probe_failure_requires_disconnect(
    consecutive_failures: u32,
    failure_threshold: u32,
    active_channels: usize,
) -> bool {
    consecutive_failures >= failure_threshold && active_channels == 0
}

#[cfg(test)]
use crate::inbound::SOCKS5_REPLY_SUCCEEDED;
#[cfg(test)]
use tokio::net::TcpListener;

type NativeSshHandle = client::Handle<NativeSshHandler>;
type DynamicAgent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

trait SshTransport: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> SshTransport for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

type BoxedSshTransport = Box<dyn SshTransport>;

// russh starts its session task before connect_stream returns a Handle. If the
// caller cancels a stalled key exchange, dropping that future alone cannot stop
// the already-spawned task. Keep the transport externally abortable until both
// key exchange and authentication have completed.
const SSH_HANDSHAKE_ABORT_ARMED: u8 = 0;
const SSH_HANDSHAKE_ABORTED: u8 = 1;
const SSH_HANDSHAKE_ABORT_DISARMED: u8 = 2;

struct SshHandshakeAbortState {
    status: AtomicU8,
    read_waker: Mutex<Option<Waker>>,
    write_waker: Mutex<Option<Waker>>,
}

impl SshHandshakeAbortState {
    fn new() -> Self {
        Self {
            status: AtomicU8::new(SSH_HANDSHAKE_ABORT_ARMED),
            read_waker: Mutex::new(None),
            write_waker: Mutex::new(None),
        }
    }

    fn status(&self) -> u8 {
        self.status.load(Ordering::Acquire)
    }

    fn register(&self, slot: &Mutex<Option<Waker>>, waker: &Waker) -> u8 {
        let status = self.status();
        if status != SSH_HANDSHAKE_ABORT_ARMED {
            return status;
        }
        let mut slot = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = self.status();
        if status == SSH_HANDSHAKE_ABORT_ARMED
            && slot
                .as_ref()
                .is_none_or(|registered| !registered.will_wake(waker))
        {
            *slot = Some(waker.clone());
        }
        status
    }

    fn register_read(&self, waker: &Waker) -> u8 {
        self.register(&self.read_waker, waker)
    }

    fn register_write(&self, waker: &Waker) -> u8 {
        self.register(&self.write_waker, waker)
    }

    fn clear(slot: &Mutex<Option<Waker>>) {
        slot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    fn clear_read(&self) {
        Self::clear(&self.read_waker);
    }

    fn clear_write(&self) {
        Self::clear(&self.write_waker);
    }

    fn clear_all(&self) {
        self.clear_read();
        self.clear_write();
    }

    fn abort(&self) {
        if self
            .status
            .compare_exchange(
                SSH_HANDSHAKE_ABORT_ARMED,
                SSH_HANDSHAKE_ABORTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        let read_waker = self
            .read_waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let write_waker = self
            .write_waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(waker) = read_waker {
            waker.wake();
        }
        if let Some(waker) = write_waker {
            waker.wake();
        }
    }

    fn disarm(&self) {
        if self
            .status
            .compare_exchange(
                SSH_HANDSHAKE_ABORT_ARMED,
                SSH_HANDSHAKE_ABORT_DISARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.clear_all();
        }
    }
}

struct SshHandshakeAbortGuard {
    state: Arc<SshHandshakeAbortState>,
}

impl SshHandshakeAbortGuard {
    fn disarm(self) {
        self.state.disarm();
    }
}

impl Drop for SshHandshakeAbortGuard {
    fn drop(&mut self) {
        self.state.abort();
    }
}

struct SshHandshakeTransport {
    inner: BoxedSshTransport,
    abort: Arc<SshHandshakeAbortState>,
}

impl SshHandshakeTransport {
    fn aborted_error() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "SSH handshake was cancelled",
        )
    }
}

impl AsyncRead for SshHandshakeTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.abort.status() {
            SSH_HANDSHAKE_ABORTED => return Poll::Ready(Err(Self::aborted_error())),
            SSH_HANDSHAKE_ABORT_DISARMED => {
                return Pin::new(&mut self.inner).poll_read(context, buffer);
            }
            SSH_HANDSHAKE_ABORT_ARMED => {}
            _ => unreachable!("invalid SSH handshake abort state"),
        }
        match Pin::new(&mut self.inner).poll_read(context, buffer) {
            Poll::Pending => match self.abort.register_read(context.waker()) {
                SSH_HANDSHAKE_ABORTED => Poll::Ready(Err(Self::aborted_error())),
                SSH_HANDSHAKE_ABORT_ARMED | SSH_HANDSHAKE_ABORT_DISARMED => Poll::Pending,
                _ => unreachable!("invalid SSH handshake abort state"),
            },
            ready => {
                self.abort.clear_read();
                ready
            }
        }
    }
}

impl AsyncWrite for SshHandshakeTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.poll_write_operation(context, |inner, context| {
            Pin::new(inner).poll_write(context, buffer)
        })
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.poll_write_operation(context, |inner, context| {
            Pin::new(inner).poll_flush(context)
        })
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.poll_write_operation(context, |inner, context| {
            Pin::new(inner).poll_shutdown(context)
        })
    }
}

impl SshHandshakeTransport {
    fn poll_write_operation<T>(
        &mut self,
        context: &mut TaskContext<'_>,
        operation: impl FnOnce(&mut BoxedSshTransport, &mut TaskContext<'_>) -> Poll<std::io::Result<T>>,
    ) -> Poll<std::io::Result<T>> {
        match self.abort.status() {
            SSH_HANDSHAKE_ABORTED => return Poll::Ready(Err(Self::aborted_error())),
            SSH_HANDSHAKE_ABORT_DISARMED => return operation(&mut self.inner, context),
            SSH_HANDSHAKE_ABORT_ARMED => {}
            _ => unreachable!("invalid SSH handshake abort state"),
        }
        match operation(&mut self.inner, context) {
            Poll::Pending => match self.abort.register_write(context.waker()) {
                SSH_HANDSHAKE_ABORTED => Poll::Ready(Err(Self::aborted_error())),
                SSH_HANDSHAKE_ABORT_ARMED | SSH_HANDSHAKE_ABORT_DISARMED => Poll::Pending,
                _ => unreachable!("invalid SSH handshake abort state"),
            },
            ready => {
                self.abort.clear_write();
                ready
            }
        }
    }
}

fn guard_ssh_handshake_transport(
    transport: BoxedSshTransport,
) -> (BoxedSshTransport, SshHandshakeAbortGuard) {
    let state = Arc::new(SshHandshakeAbortState::new());
    (
        Box::new(SshHandshakeTransport {
            inner: transport,
            abort: Arc::clone(&state),
        }),
        SshHandshakeAbortGuard { state },
    )
}

#[derive(Debug, Clone)]
struct SshNodeState {
    status: HealthStatus,
    rtt_millis: Option<u64>,
    restart_count: u64,
    last_error: Option<String>,
}

impl Default for SshNodeState {
    fn default() -> Self {
        Self {
            status: HealthStatus::Unknown,
            rtt_millis: None,
            restart_count: 0,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshSessionStatus {
    Connecting,
    Healthy,
    Suspect,
    Draining,
    Offline,
}

#[derive(Debug, Clone)]
struct SshSessionState {
    status: SshSessionStatus,
    rtt_millis: Option<u64>,
    startup_ms: Option<f64>,
    last_error: Option<String>,
    retirement_started: Option<Instant>,
    drain_started: Option<Instant>,
    drain_idle_since: Option<Instant>,
    drain_payload_generation: u64,
    forced_turnover_requested: bool,
}

impl Default for SshSessionState {
    fn default() -> Self {
        Self {
            status: SshSessionStatus::Connecting,
            rtt_millis: None,
            startup_ms: None,
            last_error: None,
            retirement_started: None,
            drain_started: None,
            drain_idle_since: None,
            drain_payload_generation: 0,
            forced_turnover_requested: false,
        }
    }
}

struct ManagedSshSession {
    id: u64,
    state: Arc<RwLock<SshSessionState>>,
    handle: Arc<RwLock<Option<Arc<NativeSshHandle>>>>,
    in_flight: Arc<AtomicUsize>,
    payload_generation: Arc<AtomicU64>,
    retire_requested: AtomicBool,
}

impl ManagedSshSession {
    async fn current_handle(&self) -> Option<Arc<NativeSshHandle>> {
        self.handle.read().await.clone()
    }

    fn has_capacity(&self, maximum: usize) -> bool {
        self.in_flight.load(Ordering::Relaxed) < maximum
    }
}

fn notify_session_event(events: &watch::Sender<u64>) {
    events.send_modify(|generation| *generation = generation.wrapping_add(1));
}

struct NativeSshNode {
    name: String,
    state: Arc<RwLock<SshNodeState>>,
    sessions: Arc<RwLock<Vec<Arc<ManagedSshSession>>>>,
    remote_owner: Arc<RwLock<Option<u64>>>,
    connect_demand: Arc<Notify>,
    session_events: watch::Sender<u64>,
    channel_open_timeout: Duration,
    max_channels_per_session: usize,
}

impl NativeSshNode {
    async fn open_channel(
        &self,
        target: &TargetAddr,
        originator_address: String,
        originator_port: u32,
    ) -> anyhow::Result<(u64, CountedStream<russh::ChannelStream<client::Msg>>)> {
        let (host, port) = target_host_port(target);
        let sessions = self.sessions.read().await.clone();
        let mut preferred = Vec::new();
        for session in sessions {
            if !session.has_capacity(self.max_channels_per_session) {
                continue;
            }
            let state = session.state.read().await;
            let score = state
                .rtt_millis
                .unwrap_or(u64::MAX / 4)
                .saturating_mul(session.in_flight.load(Ordering::Relaxed) as u64 + 1);
            match state.status {
                SshSessionStatus::Healthy if !session.retire_requested.load(Ordering::Relaxed) => {
                    preferred.push((score, Arc::clone(&session)));
                }
                SshSessionStatus::Connecting
                | SshSessionStatus::Healthy
                | SshSessionStatus::Suspect
                | SshSessionStatus::Draining
                | SshSessionStatus::Offline => {}
            }
        }
        preferred.sort_by_key(|(score, _)| *score);

        let mut last_error = None;
        for (_, session) in preferred {
            let Some(reservation) = reserve_in_flight(
                &session.in_flight,
                &session.payload_generation,
                self.max_channels_per_session,
                session.id,
            ) else {
                continue;
            };
            let Some(handle) = session.current_handle().await else {
                continue;
            };
            let started = Instant::now();
            let result = timeout(
                self.channel_open_timeout,
                handle.channel_open_direct_tcpip(
                    host.clone(),
                    port,
                    originator_address.clone(),
                    originator_port,
                ),
            )
            .await;
            match result {
                Ok(Ok(channel)) => {
                    let channel_open_ms = elapsed_ms(started);
                    stats::record_ssh_session_channel_open(session.id, channel_open_ms);
                    let sample = channel_open_ms.round() as u64;
                    let mut state = session.state.write().await;
                    state.rtt_millis = Some(ewma_rtt(state.rtt_millis, sample));
                    state.last_error = None;
                    debug!(
                        ssh_host = %self.name,
                        ssh_session_id = session.id,
                        target = %target,
                        ssh_channel_open_ms = channel_open_ms,
                        "native SSH channel established"
                    );
                    return Ok((session.id, reservation.into_stream(channel.into_stream())));
                }
                Ok(Err(error)) => {
                    let error = format!("failed to open SSH channel: {error}");
                    session.state.write().await.last_error = Some(error.clone());
                    stats::record_ssh_session_channel_error(session.id, &error);
                    warn!(
                        ssh_host = %self.name,
                        ssh_session_id = session.id,
                        target = %target,
                        %error,
                        "SSH channel open failed"
                    );
                    last_error = Some(error);
                }
                Err(_) => {
                    let error = format!(
                        "timed out opening SSH channel after {} ms",
                        self.channel_open_timeout.as_millis()
                    );
                    session.state.write().await.last_error = Some(error.clone());
                    stats::record_ssh_session_channel_error(session.id, &error);
                    warn!(
                        ssh_host = %self.name,
                        ssh_session_id = session.id,
                        target = %target,
                        timeout_ms = self.channel_open_timeout.as_millis(),
                        "SSH channel open timed out"
                    );
                    last_error = Some(error);
                }
            }
        }

        bail!(
            "SSH host {} has no available session{}",
            self.name,
            last_error
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        )
    }

    async fn has_available_session(&self) -> bool {
        let sessions = self.sessions.read().await.clone();
        for session in sessions {
            if !session.has_capacity(self.max_channels_per_session) {
                continue;
            }
            let state = session.state.read().await;
            if state.status == SshSessionStatus::Healthy
                && !session.retire_requested.load(Ordering::Relaxed)
            {
                return true;
            }
        }
        false
    }

    async fn in_flight(&self) -> usize {
        self.sessions
            .read()
            .await
            .iter()
            .map(|session| session.in_flight.load(Ordering::Relaxed))
            .sum()
    }
}

pub(crate) struct SshPoolDialer {
    name: String,
    nodes: Vec<Arc<NativeSshNode>>,
    balancer: LoadBalancer,
    connectivity: ConnectivityHandle,
    session_events: watch::Sender<u64>,
    session_recovery_poll_interval: Duration,
    tasks: Vec<AbortHandle>,
}

impl SshPoolDialer {
    #[cfg(test)]
    pub(crate) fn start(
        name: impl Into<String>,
        pool: SshPoolConfig,
        probe: ProbeConfig,
    ) -> anyhow::Result<Self> {
        Self::start_with_connectivity(name, pool, probe, ConnectivityHandle::assume_online())
    }

    pub(crate) fn start_with_connectivity(
        name: impl Into<String>,
        pool: SshPoolConfig,
        probe: ProbeConfig,
        connectivity: ConnectivityHandle,
    ) -> anyhow::Result<Self> {
        let name = name.into();
        let plans = pool
            .hosts
            .iter()
            .map(|upstream| resolve_ssh_plan(upstream, &pool).map(Arc::new))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let session_recovery_poll_interval = plans
            .iter()
            .map(|plan| session_recovery_poll_interval(plan))
            .max()
            .unwrap_or_else(|| Duration::from_secs(1));
        let (session_events, _) = watch::channel(0_u64);
        let mut nodes = Vec::with_capacity(pool.hosts.len());
        let mut tasks = Vec::new();

        for (upstream, plan) in pool.hosts.iter().zip(plans) {
            register_resolved_host(upstream, &plan, &pool);
            let state = Arc::new(RwLock::new(SshNodeState::default()));
            let node = Arc::new(NativeSshNode {
                name: upstream.name.clone(),
                state: Arc::clone(&state),
                sessions: Arc::new(RwLock::new(Vec::new())),
                remote_owner: Arc::new(RwLock::new(None)),
                connect_demand: Arc::new(Notify::new()),
                session_events: session_events.clone(),
                channel_open_timeout: plan.target.connect_timeout,
                max_channels_per_session: pool.max_channels_per_session,
            });
            nodes.push(Arc::clone(&node));

            info!(
                host_name = %name,
                ssh_alias = %plan.target.alias,
                ssh_address = %plan.target.host,
                ssh_port = plan.target.port,
                proxy_jump_count = plan.jumps.len(),
                ssh_config_path = ?plan.config_path,
                "resolved SSH host configuration"
            );

            let manager_name = name.clone();
            let manager_upstream = upstream.clone();
            let manager_pool = pool.clone();
            let manager_plan = Arc::clone(&plan);
            let manager_node = Arc::clone(&node);
            let manager_connectivity = connectivity.clone();
            let span = info_span!(
                "ssh_session_pool",
                host_name = %manager_name
            );
            let task = tokio::spawn(
                manage_ssh_sessions(
                    manager_name,
                    manager_upstream,
                    manager_pool,
                    manager_plan,
                    probe,
                    manager_node,
                    manager_connectivity,
                )
                .instrument(span),
            );
            tasks.push(task.abort_handle());
        }

        Ok(Self {
            name,
            nodes,
            balancer: LoadBalancer::new(pool.policy),
            connectivity,
            session_events,
            session_recovery_poll_interval,
            tasks,
        })
    }

    async fn snapshots(&self, excluded: &HashSet<String>) -> Vec<SshHostState> {
        let mut snapshots = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let available = !excluded.contains(&node.name) && node.has_available_session().await;
            let state = node.state.read().await;
            let status = if available {
                HealthStatus::Healthy
            } else {
                match state.status {
                    HealthStatus::Unknown => HealthStatus::Offline,
                    status => status,
                }
            };
            snapshots.push(SshHostState {
                name: node.name.clone(),
                enabled: available,
                status,
                rtt_millis: state.rtt_millis,
                in_flight: node.in_flight().await,
            });
        }
        snapshots
    }

    fn node(&self, name: &str) -> Option<&Arc<NativeSshNode>> {
        self.nodes.iter().find(|node| node.name == name)
    }

    async fn has_available_session(&self) -> bool {
        for node in &self.nodes {
            if node.has_available_session().await {
                return true;
            }
        }
        false
    }

    fn request_session_capacity(&self) {
        for node in &self.nodes {
            node.connect_demand.notify_one();
        }
    }

    async fn dial_available(&self, context: &DialContext) -> anyhow::Result<BoxedProxyStream> {
        let mut excluded = HashSet::new();
        let mut last_error = None;

        while excluded.len() < self.nodes.len() {
            let snapshots = self.snapshots(&excluded).await;
            let Some(selected) = self.balancer.select(&snapshots) else {
                break;
            };
            let selected_name = selected.name.clone();
            let Some(node) = self.node(&selected_name) else {
                break;
            };
            let started = Instant::now();
            match node
                .open_channel(&context.target, "127.0.0.1".to_string(), 0)
                .await
            {
                Ok((session_id, stream)) => {
                    let dial_ms = elapsed_ms(started);
                    let mut state = node.state.write().await;
                    state.status = HealthStatus::Healthy;
                    state.rtt_millis = Some(dial_ms.round() as u64);
                    state.last_error = None;
                    debug!(
                        host_name = %self.name,
                        target = %context.target,
                        ssh_channel_open_ms = dial_ms,
                        "native SSH dynamic channel established"
                    );
                    if let Some(connection_id) = context.connection_id {
                        stats::associate_connection_session(connection_id, session_id);
                    }
                    return Ok(Box::new(stream));
                }
                Err(error) => {
                    let error_text = format!("{error:#}");
                    let mut state = node.state.write().await;
                    state.status = HealthStatus::Degraded;
                    state.last_error = Some(error_text.clone());
                    last_error = Some(error_text);
                    excluded.insert(selected_name);
                }
            }
        }

        bail!(
            "SSH host {} has no healthy session for dynamic forwarding{}",
            self.name,
            last_error
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        )
    }
}

pub(crate) fn register_idle_ssh_host(host_name: &str, pool: &SshPoolConfig) -> anyhow::Result<()> {
    let upstream = pool
        .hosts
        .first()
        .with_context(|| format!("SSH host {host_name} has no runtime endpoint"))?;
    let plan = resolve_ssh_plan(upstream, pool)?;
    register_resolved_host(upstream, &plan, pool);
    stats::update_host_state(
        host_name,
        stats::HostStateUpdate {
            status: stats::HostRuntimeStatus::Idle,
            rtt_ms: None,
            restart_count: 0,
            last_error: None,
        },
    );
    Ok(())
}

fn register_resolved_host(upstream: &SshHostConfig, plan: &ResolvedSshPlan, pool: &SshPoolConfig) {
    stats::register_host(stats::HostRegistration {
        name: upstream.name.clone(),
        ssh_alias: plan.target.alias.clone(),
        address: format_ssh_address(&plan.target.host, plan.target.port),
        min_sessions: pool.min_sessions_per_host,
        max_sessions: pool.max_sessions_per_host,
    });
}

impl Drop for SshPoolDialer {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[async_trait]
impl OutboundDialer for SshPoolDialer {
    async fn dial(&self, context: DialContext) -> anyhow::Result<BoxedProxyStream> {
        if self.has_available_session().await {
            return self.dial_available(&context).await;
        }

        let mut session_events = self.session_events.subscribe();
        let mut connectivity_events = self.connectivity.subscribe();
        let mut connectivity_events_open = true;
        let mut connectivity = self.connectivity.current();
        if connectivity.is_offline()
            || connectivity.availability == NetworkAvailability::Unknown
            || !connectivity.events_available
        {
            connectivity = self.connectivity.refresh().await;
        }

        loop {
            if self.has_available_session().await {
                return self.dial_available(&context).await;
            }
            if !connectivity.is_offline() {
                self.request_session_capacity();
            }
            tokio::select! {
                changed = session_events.changed() => {
                    if changed.is_err() {
                        bail!("SSH host {} session manager stopped", self.name);
                    }
                }
                changed = connectivity_events.changed(), if connectivity_events_open => {
                    if changed.is_err() {
                        connectivity_events_open = false;
                        continue;
                    }
                    connectivity = *connectivity_events.borrow_and_update();
                    if !connectivity.is_offline() {
                        self.request_session_capacity();
                    }
                }
                _ = sleep(self.session_recovery_poll_interval) => {
                    connectivity = self.connectivity.refresh().await;
                    if !connectivity.is_offline() {
                        self.request_session_capacity();
                    }
                    debug!(
                        host_name = %self.name,
                        target = %context.target,
                        network_availability = ?connectivity.availability,
                        poll_interval_ms = self.session_recovery_poll_interval.as_millis(),
                        "SSH dial is waiting for session recovery"
                    );
                }
            }
        }
    }
}

trait LogTaskResult: Sized {
    fn map_err_log(self, message: &'static str) -> impl std::future::Future<Output = ()> + Send;
}

impl<F> LogTaskResult for F
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send,
{
    async fn map_err_log(self, message: &'static str) {
        if let Err(error) = self.await {
            stats::record_error();
            warn!(%error, "{message}");
        }
    }
}

#[derive(Clone)]
enum RemoteForwardRoute {
    Tcp {
        name: String,
        tunnel_id: String,
        local_host: String,
        local_port: u16,
    },
    Dynamic {
        name: String,
        tunnel_id: String,
        protocol: ProxyProtocol,
    },
}

struct NativeSshHandler {
    upstream: String,
    session_id: u64,
    host: String,
    port: u16,
    host_key_policy: ResolvedHostKeyPolicy,
    host_key_name: String,
    known_hosts_paths: Vec<PathBuf>,
    remote_forwards: Arc<HashMap<u32, RemoteForwardRoute>>,
    in_flight: Arc<AtomicUsize>,
    payload_generation: Arc<AtomicU64>,
    disconnect_tx: mpsc::UnboundedSender<String>,
}

impl client::Handler for NativeSshHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let mut accepted = self.host_key_policy == ResolvedHostKeyPolicy::InsecureAcceptAny;
        if !accepted {
            for path in &self.known_hosts_paths {
                if keys::check_known_hosts_path(
                    &self.host_key_name,
                    self.port,
                    server_public_key,
                    path,
                )? {
                    accepted = true;
                    break;
                }
            }
        }
        if !accepted && self.host_key_policy == ResolvedHostKeyPolicy::AcceptNew {
            let path = self
                .known_hosts_paths
                .first()
                .context("SSH accept-new requires a known_hosts path")?;
            keys::known_hosts::learn_known_hosts_path(
                &self.host_key_name,
                self.port,
                server_public_key,
                path,
            )?;
            accepted = true;
            info!(
                ssh_host = %self.upstream,
                host = %self.host_key_name,
                port = self.port,
                known_hosts_path = %path.display(),
                "recorded new SSH server key"
            );
        };
        if !accepted {
            warn!(
                ssh_host = %self.upstream,
                host = %self.host,
                port = self.port,
                "SSH server key is not present in known_hosts"
            );
        }
        Ok(accepted)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let Some(route) = self.remote_forwards.get(&connected_port).cloned() else {
            warn!(
                ssh_host = %self.upstream,
                %connected_address,
                connected_port,
                "rejected unconfigured SSH remote forward channel"
            );
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };

        let connection_id = next_connection_id();
        let active_channel = ActiveChannelGuard::new(Arc::clone(&self.in_flight), self.session_id);
        let payload_generation = Arc::clone(&self.payload_generation);
        let upstream = self.upstream.clone();
        let session_id = self.session_id;
        let connected_address = connected_address.to_string();
        let originator_address = originator_address.to_string();
        let span = info_span!(
            "proxy_connection",
            connection_id,
            ssh_forward = "remote",
            ssh_host = %upstream,
            ssh_session_id = session_id,
            %connected_address,
            connected_port,
            %originator_address,
            originator_port
        );
        tokio::spawn(
            async move {
                let _active_channel = active_channel;
                match route {
                    RemoteForwardRoute::Tcp {
                        name,
                        tunnel_id,
                        local_host,
                        local_port,
                    } => {
                        handle_remote_tcp_forward(
                            RemoteTcpForwardContext {
                                connection_id,
                                upstream,
                                session_id,
                                peer_address: format!("{}:{}", originator_address, originator_port),
                                forward: name,
                                tunnel_id,
                                local_host,
                                local_port,
                                payload_generation,
                            },
                            channel,
                            reply,
                        )
                        .await
                    }
                    RemoteForwardRoute::Dynamic {
                        name,
                        tunnel_id,
                        protocol,
                    } => {
                        reply.accept().await;
                        let peer_addr = remote_peer_addr(&originator_address, originator_port);
                        serve_remote_proxy(
                            RemoteProxyContext {
                                upstream,
                                session_id,
                                connection_id,
                                forward: name,
                                tunnel_id,
                                protocol,
                                peer_addr,
                                payload_generation,
                            },
                            channel.into_stream(),
                        )
                        .await
                    }
                }
            }
            .map_err_log("SSH remote forward session failed")
            .instrument(span),
        );
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let _ = self.disconnect_tx.send(format!("{reason:?}"));
        Ok(())
    }
}

struct RemoteTcpForwardContext {
    connection_id: u64,
    upstream: String,
    session_id: u64,
    peer_address: String,
    forward: String,
    tunnel_id: String,
    local_host: String,
    local_port: u16,
    payload_generation: Arc<AtomicU64>,
}

async fn handle_remote_tcp_forward(
    context: RemoteTcpForwardContext,
    channel: Channel<client::Msg>,
    reply: client::ChannelOpenHandle,
) -> anyhow::Result<()> {
    let target = TargetAddr::from_host_port(context.local_host, context.local_port);
    let _connection = stats::LocalConnectionGuard::start(stats::ConnectionRegistration {
        id: context.connection_id,
        host_name: context.upstream.clone(),
        tunnel_id: context.tunnel_id.clone(),
        peer_address: context.peer_address.clone(),
        target: Some(target.to_string()),
        protocol: Some("TCP".to_string()),
        session_id: Some(context.session_id),
    });
    let started = Instant::now();
    let local_stream = LocalTcpDialer
        .dial(DialContext {
            host_name: format!("ssh-remote/{}/{}", context.upstream, context.forward),
            target: target.clone(),
            connection_id: Some(context.connection_id),
        })
        .await;
    let mut local_stream = match local_stream {
        Ok(stream) => {
            reply.accept().await;
            stream
        }
        Err(error) => {
            stats::record_connection_error(context.connection_id, &format!("{error:#}"), true);
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            stats::record_tunnel_error(
                &context.upstream,
                &context.tunnel_id,
                &format!("{error:#}"),
            );
            return Err(error).context("failed to connect SSH remote forward local target");
        }
    };
    stats::mark_connection_active(context.connection_id);
    let connect_ms = elapsed_ms(started);
    let mut ssh_stream = PayloadActivityStream::new(
        channel.into_stream(),
        Arc::clone(&context.payload_generation),
    );
    let relay_started = Instant::now();
    let recorder = stats::tunnel_and_session_transfer_recorder(
        &context.upstream,
        &context.tunnel_id,
        context.session_id,
        context.connection_id,
    );
    let mut timed_ssh_stream =
        stats::TimedIo::with_transfer_recorder(&mut ssh_stream, relay_started, recorder);
    let (remote_to_local_bytes, local_to_remote_bytes) =
        match copy_bidirectional(&mut timed_ssh_stream, &mut local_stream).await {
            Ok(bytes) => bytes,
            Err(error) => {
                stats::record_connection_error(context.connection_id, &error.to_string(), true);
                stats::record_tunnel_error(
                    &context.upstream,
                    &context.tunnel_id,
                    &error.to_string(),
                );
                return Err(error.into());
            }
        };
    debug!(
        forward = %context.forward,
        target = %target,
        connect_ms,
        relay_duration_ms = elapsed_ms(relay_started),
        remote_to_local_bytes,
        local_to_remote_bytes,
        session_total_ms = elapsed_ms(started),
        "SSH fixed remote forward session finished"
    );
    Ok(())
}

struct RemoteProxyContext {
    upstream: String,
    session_id: u64,
    connection_id: u64,
    forward: String,
    tunnel_id: String,
    protocol: ProxyProtocol,
    peer_addr: SocketAddr,
    payload_generation: Arc<AtomicU64>,
}

async fn serve_remote_proxy<S>(context: RemoteProxyContext, remote_stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let connection = Arc::new(stats::LocalConnectionGuard::start(
        stats::ConnectionRegistration {
            id: context.connection_id,
            host_name: context.upstream.clone(),
            tunnel_id: context.tunnel_id.clone(),
            peer_address: context.peer_addr.to_string(),
            target: None,
            protocol: Some(super::engine::proxy_protocol_name(context.protocol).to_string()),
            session_id: Some(context.session_id),
        },
    ));
    let session_recorder = stats::session_transfer_recorder(context.session_id);
    let remote_stream = PayloadActivityStream::new(remote_stream, context.payload_generation);
    let remote_stream =
        stats::TimedIo::with_transfer_recorder(remote_stream, Instant::now(), session_recorder);
    let result = handle_proxy_session(
        remote_stream,
        context.protocol,
        ProxySessionContext {
            local_forward_name: context.forward,
            peer_addr: context.peer_addr,
            host_name: format!("remote/{}", context.upstream),
            stats_host_name: context.upstream.clone(),
            tunnel_id: context.tunnel_id.clone(),
            connection_id: context.connection_id,
            connection_started: Instant::now(),
            _connection_lifetime: Some(Arc::clone(&connection)),
        },
        Arc::new(LocalTcpDialer),
    )
    .await;
    if let Err(error) = &result {
        stats::record_connection_error(context.connection_id, &format!("{error:#}"), true);
        stats::record_tunnel_error(&context.upstream, &context.tunnel_id, &format!("{error:#}"));
    }
    result
}

fn remote_peer_addr(address: &str, port: u32) -> SocketAddr {
    let ip = address
        .trim_matches(['[', ']'])
        .parse()
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    SocketAddr::new(ip, u16::try_from(port).unwrap_or(0))
}

struct ProxyCommandStream {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr_task: AbortHandle,
}

impl ProxyCommandStream {
    fn spawn(endpoint: &ResolvedSshEndpoint) -> anyhow::Result<Self> {
        let command = endpoint
            .proxy_command
            .as_deref()
            .context("missing SSH ProxyCommand")?;
        let expanded = expand_proxy_command(command, endpoint);
        let mut process = proxy_command_process(&expanded);
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = process.spawn().with_context(|| {
            format!(
                "failed to start ProxyCommand for SSH host {}",
                endpoint.alias
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .context("ProxyCommand stdin was not available")?;
        let stdout = child
            .stdout
            .take()
            .context("ProxyCommand stdout was not available")?;
        let mut stderr = child
            .stderr
            .take()
            .context("ProxyCommand stderr was not available")?;
        let alias = endpoint.alias.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buffer = vec![0_u8; 4096];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(length) => debug!(
                        ssh_alias = %alias,
                        stderr = %String::from_utf8_lossy(&buffer[..length]).trim(),
                        "SSH ProxyCommand stderr"
                    ),
                    Err(error) => {
                        debug!(ssh_alias = %alias, %error, "failed to read ProxyCommand stderr");
                        break;
                    }
                }
            }
        })
        .abort_handle();
        debug!(
            ssh_alias = %endpoint.alias,
            "started SSH ProxyCommand"
        );
        Ok(Self {
            child,
            stdin,
            stdout,
            stderr_task,
        })
    }
}

impl AsyncRead for ProxyCommandStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(context, buffer)
    }
}

impl AsyncWrite for ProxyCommandStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(context)
    }
}

impl Drop for ProxyCommandStream {
    fn drop(&mut self) {
        self.stderr_task.abort();
        let _ = self.child.start_kill();
    }
}

#[cfg(unix)]
fn proxy_command_process(command: &str) -> Command {
    let mut process = Command::new("/bin/sh");
    process.arg("-c").arg(command);
    process
}

#[cfg(windows)]
fn proxy_command_process(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.arg("/C").arg(command);
    process
}

struct EstablishedSession {
    handle: Arc<NativeSshHandle>,
    _transport_chain: Vec<Arc<NativeSshHandle>>,
    disconnect_rx: mpsc::UnboundedReceiver<String>,
    startup_ms: f64,
}

struct SessionExit {
    id: u64,
    reached_healthy: bool,
    retired: bool,
    error: String,
}

struct ScheduledSessionRotation {
    candidate_session_id: u64,
    replacement_session_id: Option<u64>,
}

enum ScheduledRotationProgress {
    Waiting,
    Activated,
    Cancelled,
}

async fn manage_ssh_sessions(
    outbound: String,
    upstream: SshHostConfig,
    pool: SshPoolConfig,
    plan: Arc<ResolvedSshPlan>,
    probe: ProbeConfig,
    node: Arc<NativeSshNode>,
    connectivity: ConnectivityHandle,
) {
    let initial_backoff = Duration::from_millis(pool.restart_initial_millis);
    let max_backoff = Duration::from_secs(pool.restart_max_secs);
    let spawn_cooldown = Duration::from_millis(pool.session_spawn_cooldown_millis);
    let drain_timeout = Duration::from_secs(pool.session_drain_timeout_secs);
    let rotation_interval = pool
        .session_rotation_enabled
        .then(|| Duration::from_secs(pool.session_rotation_interval_secs));
    let mut backoff = initial_backoff;
    let mut next_spawn_at = Instant::now();
    let mut next_owner_attempt = Instant::now();
    let mut next_rotation_at = rotation_interval.map(|interval| Instant::now() + interval);
    let mut scheduled_rotation = None;
    let mut desired_sessions = pool.min_sessions_per_host;
    let mut high_pressure_since = None;
    let mut low_pressure_since = None;
    let mut next_scale_down_at = None;
    let requires_remote_availability = !upstream.remote_forwards.is_empty();
    let mut connectivity_rx = connectivity.subscribe();
    let mut connectivity_snapshot = *connectivity_rx.borrow();
    let mut connectivity_events_open = true;
    let mut demand_pending = false;
    let mut startup_spawn_budget = pool.min_sessions_per_host;
    let mut tasks = JoinSet::new();
    let mut resume_settle_until = None;
    let mut ticker = new_session_manager_ticker(session_manager_tick_interval(
        connectivity_snapshot,
        requires_remote_availability,
        session_spawn_authorized(
            connectivity_snapshot,
            requires_remote_availability,
            demand_pending,
            startup_spawn_budget,
        ),
    ));
    let mut last_manager_tick = Instant::now();

    if session_spawn_authorized(
        connectivity_snapshot,
        requires_remote_availability,
        demand_pending,
        startup_spawn_budget,
    ) {
        for _ in 0..initial_session_start_count(pool.min_sessions_per_host) {
            let session_id = stats::next_ssh_session_id();
            spawn_managed_session(
                &mut tasks,
                session_id,
                SessionSpawnContext {
                    outbound: &outbound,
                    upstream: &upstream,
                    plan: &plan,
                    probe,
                    node: &node,
                    connectivity: &connectivity,
                },
            )
            .await;
            startup_spawn_budget = startup_spawn_budget.saturating_sub(1);
        }
    }

    loop {
        tokio::select! {
            changed = connectivity_rx.changed(), if connectivity_events_open => {
                let mut resumed = false;
                if changed.is_err() {
                    connectivity_events_open = false;
                    connectivity_snapshot = connectivity.current();
                } else {
                    let previous = connectivity_snapshot;
                    connectivity_snapshot = *connectivity_rx.borrow_and_update();
                    resumed = connectivity_snapshot.resumed_since(previous);
                    if (previous.is_offline() && !connectivity_snapshot.is_offline())
                        || resumed
                    {
                        backoff = initial_backoff;
                        next_spawn_at = Instant::now();
                        next_owner_attempt = Instant::now();
                    }
                }
                let background_work_enabled = !tasks.is_empty()
                    || session_spawn_authorized(
                        connectivity_snapshot,
                        requires_remote_availability,
                        demand_pending,
                        startup_spawn_budget,
                    );
                if resumed {
                    last_manager_tick = Instant::now();
                    resume_settle_until = Some(Instant::now() + RESUME_SETTLE_INTERVAL);
                }
                ticker = new_session_manager_ticker_not_before(
                    session_manager_tick_interval(
                        connectivity_snapshot,
                        requires_remote_availability,
                        background_work_enabled,
                    ),
                    resume_settle_until,
                );
            }
            _ = node.connect_demand.notified() => {
                demand_pending = true;
                let previous_connectivity = connectivity_snapshot;
                if connectivity_snapshot.is_offline()
                    || connectivity_snapshot.availability == NetworkAvailability::Unknown
                    || !connectivity_snapshot.events_available
                {
                    connectivity_snapshot = connectivity.refresh().await;
                }
                if (previous_connectivity.is_offline() && !connectivity_snapshot.is_offline())
                    || (!connectivity_snapshot.events_available
                        && !requires_remote_availability
                        && tasks.is_empty())
                {
                    backoff = initial_backoff;
                    next_spawn_at = Instant::now();
                }
                ticker = new_session_manager_ticker_not_before(
                    session_manager_tick_interval(
                        connectivity_snapshot,
                        requires_remote_availability,
                        true,
                    ),
                    resume_settle_until,
                );
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                match joined {
                    Some(Ok(exit)) => {
                        node.sessions.write().await.retain(|session| session.id != exit.id);
                        notify_session_event(&node.session_events);
                        if scheduled_rotation.as_ref().is_some_and(|rotation: &ScheduledSessionRotation| {
                            rotation.candidate_session_id == exit.id
                        }) {
                            scheduled_rotation = None;
                        } else if let Some(rotation) = scheduled_rotation.as_mut()
                            && rotation.replacement_session_id == Some(exit.id)
                        {
                            rotation.replacement_session_id = None;
                        }
                        if exit.retired {
                            stats::remove_ssh_session(exit.id);
                        }
                        let mut owner = node.remote_owner.write().await;
                        if *owner == Some(exit.id) {
                            *owner = None;
                            mark_remote_tunnels_starting(&upstream);
                            next_owner_attempt = Instant::now();
                        }
                        drop(owner);
                        if !exit.retired {
                            let mut state = node.state.write().await;
                            state.restart_count = state.restart_count.saturating_add(1);
                            state.last_error = Some(exit.error.clone());
                        }
                        if exit.reached_healthy {
                            backoff = initial_backoff;
                            next_spawn_at = Instant::now() + spawn_cooldown;
                        } else {
                            next_spawn_at = Instant::now() + backoff;
                            backoff = next_backoff(backoff, max_backoff);
                        }
                        if exit.retired {
                            info!(
                                host_name = %outbound,
                                ssh_session_id = exit.id,
                                %exit.error,
                                "retired native SSH session left the pool"
                            );
                        } else {
                            warn!(
                                host_name = %outbound,
                                ssh_session_id = exit.id,
                                %exit.error,
                                "native SSH session left the pool"
                            );
                        }
                    }
                    Some(Err(error)) => {
                        warn!(%error, "SSH session task failed");
                        next_spawn_at = Instant::now() + backoff;
                        backoff = next_backoff(backoff, max_backoff);
                    }
                    None => {}
                }
                let background_work_enabled = !tasks.is_empty()
                    || session_spawn_authorized(
                        connectivity_snapshot,
                        requires_remote_availability,
                        demand_pending,
                        startup_spawn_budget,
                    );
                ticker = new_session_manager_ticker_not_before(
                    session_manager_tick_interval(
                        connectivity_snapshot,
                        requires_remote_availability,
                        background_work_enabled,
                    ),
                    resume_settle_until,
                );
            }
            _ = ticker.tick() => {
                let tick_started = Instant::now();
                let active_maintenance = !connectivity_snapshot.is_offline()
                    && (!tasks.is_empty()
                        || session_spawn_authorized(
                            connectivity_snapshot,
                            requires_remote_availability,
                            demand_pending,
                            startup_spawn_budget,
                        ));
                let resumed_after_gap = active_maintenance
                    && tick_started.saturating_duration_since(last_manager_tick)
                        >= SUSPECTED_SUSPEND_GAP;
                last_manager_tick = tick_started;
                connectivity_snapshot = connectivity.current();
                if resumed_after_gap {
                    connectivity_snapshot = connectivity.refresh().await;
                    resume_settle_until = Some(Instant::now() + RESUME_SETTLE_INTERVAL);
                    ticker = new_session_manager_ticker_not_before(
                        session_manager_tick_interval(
                            connectivity_snapshot,
                            requires_remote_availability,
                            active_maintenance,
                        ),
                        resume_settle_until,
                    );
                    continue;
                }
                if resume_settle_until.is_some_and(|deadline| tick_started >= deadline) {
                    resume_settle_until = None;
                }
                if connectivity_snapshot.is_offline() {
                    if requires_remote_availability {
                        connectivity_snapshot = connectivity.refresh().await;
                    }
                    if connectivity_snapshot.is_offline() {
                        refresh_node_state(&node).await;
                        ticker = new_delayed_session_manager_ticker(session_manager_tick_interval(
                            connectivity_snapshot,
                            requires_remote_availability,
                            false,
                        ));
                        continue;
                    }
                }
                refresh_node_state(&node).await;

                if Instant::now() >= next_owner_attempt
                    && let Err(error) = ensure_remote_owner(
                        &node,
                        &upstream,
                        plan.target.connect_timeout,
                    ).await
                {
                    debug!(
                        host_name = %outbound,
                        %error,
                        "SSH remote forward owner handover is not ready"
                    );
                    next_owner_attempt = Instant::now() + spawn_cooldown;
                }

                let sessions = node.sessions.read().await.clone();
                let mut active_count = 0_usize;
                let mut healthy_non_retiring = 0_usize;
                let mut connecting_non_retiring = 0_usize;
                let mut active_non_retiring = 0_usize;
                let mut healthy_in_flight = 0_usize;
                let mut retiring_active = 0_usize;
                let mut retirement_pending = false;
                let mut forced_turnover_in_progress = false;
                for session in &sessions {
                    let state = session.state.read().await;
                    if state.status != SshSessionStatus::Offline {
                        active_count += 1;
                        if !session.retire_requested.load(Ordering::Relaxed) {
                            active_non_retiring += 1;
                        } else {
                            retiring_active += 1;
                        }
                    }
                    if state.status == SshSessionStatus::Healthy
                        && !session.retire_requested.load(Ordering::Relaxed)
                    {
                        healthy_non_retiring += 1;
                        healthy_in_flight = healthy_in_flight.saturating_add(
                            session.in_flight.load(Ordering::Relaxed),
                        );
                    }
                    if state.status == SshSessionStatus::Connecting
                        && !session.retire_requested.load(Ordering::Relaxed)
                    {
                        connecting_non_retiring += 1;
                    }
                    retirement_pending |= session.retire_requested.load(Ordering::Relaxed);
                    forced_turnover_in_progress |= state.forced_turnover_requested;
                }

                let now = Instant::now();
                if healthy_non_retiring > 0 {
                    demand_pending = false;
                }
                let can_spawn_new_session = healthy_non_retiring > 0
                    || session_spawn_authorized(
                        connectivity_snapshot,
                        requires_remote_availability,
                        demand_pending,
                        startup_spawn_budget,
                    );
                if scheduled_rotation.is_some()
                    && healthy_non_retiring < pool.min_sessions_per_host
                {
                    scheduled_rotation = None;
                    debug!(
                        host_name = %outbound,
                        "scheduled SSH session rotation deferred while pool health recovers"
                    );
                }

                if let Some(rotation) = scheduled_rotation.as_ref() {
                    match advance_scheduled_session_rotation(&node, rotation).await {
                        ScheduledRotationProgress::Waiting => {}
                        ScheduledRotationProgress::Activated => {
                            scheduled_rotation = None;
                            retirement_pending = true;
                            healthy_non_retiring = healthy_non_retiring.saturating_sub(1);
                        }
                        ScheduledRotationProgress::Cancelled => {
                            scheduled_rotation = None;
                        }
                    }
                }

                let under_pressure = session_pool_is_under_pressure(
                    healthy_in_flight,
                    healthy_non_retiring,
                    pool.max_channels_per_session,
                );
                let can_scale_down = session_pool_can_scale_down(
                    healthy_in_flight,
                    healthy_non_retiring,
                    pool.min_sessions_per_host,
                    pool.max_channels_per_session,
                );
                if under_pressure {
                    high_pressure_since.get_or_insert(now);
                    low_pressure_since = None;
                    next_scale_down_at = None;
                } else {
                    high_pressure_since = None;
                    if can_scale_down {
                        low_pressure_since.get_or_insert(now);
                    } else {
                        low_pressure_since = None;
                        next_scale_down_at = None;
                    }
                }

                if next_rotation_at.is_some_and(|next| now >= next)
                    && can_spawn_new_session
                    && scheduled_rotation.is_none()
                    && !retirement_pending
                    && healthy_non_retiring >= pool.min_sessions_per_host
                    && active_count < pool.max_sessions_per_host
                    && let Some(candidate_session_id) = oldest_healthy_session_id(&node).await
                {
                    scheduled_rotation = Some(ScheduledSessionRotation {
                        candidate_session_id,
                        replacement_session_id: None,
                    });
                    next_rotation_at = rotation_interval.map(|interval| now + interval);
                    info!(
                        host_name = %outbound,
                        ssh_session_id = candidate_session_id,
                        "scheduled replacement for SSH session rotation"
                    );
                }

                let needs_desired_capacity = active_non_retiring < desired_sessions
                    && active_count < pool.max_sessions_per_host;
                let needs_scheduled_replacement = scheduled_rotation
                    .as_ref()
                    .is_some_and(|rotation| rotation.replacement_session_id.is_none())
                    && active_count < pool.max_sessions_per_host;
                let needs_elastic_capacity = scheduled_rotation.is_none()
                    && !needs_desired_capacity
                    && !needs_scheduled_replacement
                    && under_pressure
                    && high_pressure_since.is_some_and(|since| {
                        now.saturating_duration_since(since)
                            >= SESSION_SCALE_UP_SUSTAINED_INTERVAL
                    })
                    && connecting_non_retiring == 0
                    && active_count < pool.max_sessions_per_host;
                if (needs_desired_capacity
                    || needs_scheduled_replacement
                    || needs_elastic_capacity)
                    && can_spawn_new_session
                    && can_start_another_session(connecting_non_retiring)
                    && now >= next_spawn_at
                {
                    let session_id = stats::next_ssh_session_id();
                    if needs_scheduled_replacement
                        && let Some(rotation) = scheduled_rotation.as_mut()
                    {
                        rotation.replacement_session_id = Some(session_id);
                    }
                    if needs_elastic_capacity {
                        desired_sessions = desired_sessions
                            .saturating_add(1)
                            .min(pool.max_sessions_per_host);
                    }
                    spawn_managed_session(
                        &mut tasks,
                        session_id,
                        SessionSpawnContext {
                            outbound: &outbound,
                            upstream: &upstream,
                            plan: &plan,
                            probe,
                            node: &node,
                            connectivity: &connectivity,
                        },
                    )
                    .await;
                    startup_spawn_budget = startup_spawn_budget.saturating_sub(1);
                    demand_pending = false;
                    next_spawn_at = now + spawn_cooldown;
                    if needs_elastic_capacity {
                        high_pressure_since = None;
                        info!(
                            host_name = %outbound,
                            ssh_session_id = session_id,
                            active_channels = healthy_in_flight,
                            healthy_sessions = healthy_non_retiring,
                            max_sessions = pool.max_sessions_per_host,
                            "scaling up SSH session pool for sustained channel pressure"
                        );
                    }
                }

                let scale_down_ready = scheduled_rotation.is_none()
                    && !retirement_pending
                    && connecting_non_retiring == 0
                    && desired_sessions > pool.min_sessions_per_host
                    && low_pressure_since.is_some_and(|since| {
                        now.saturating_duration_since(since)
                            >= SESSION_SCALE_DOWN_IDLE_INTERVAL
                    })
                    && next_scale_down_at.is_none_or(|next| now >= next);
                if scale_down_ready
                    && let Some(session_id) = newest_idle_elastic_session_id(
                        &node,
                        pool.min_sessions_per_host,
                    ).await
                    && let Some(session) = sessions.iter().find(|session| session.id == session_id)
                {
                    request_session_retirement(session).await;
                    retirement_pending = true;
                    desired_sessions = desired_sessions
                        .saturating_sub(1)
                        .max(pool.min_sessions_per_host);
                    next_scale_down_at = Some(now + SESSION_SCALE_DOWN_STEP_INTERVAL);
                    info!(
                        host_name = %outbound,
                        ssh_session_id = session_id,
                        active_channels = healthy_in_flight,
                        healthy_sessions = healthy_non_retiring,
                        minimum_sessions = pool.min_sessions_per_host,
                        "retiring idle elastic SSH session"
                    );
                }

                let replacement_capacity_blocked = retirement_pending
                    && active_count >= pool.max_sessions_per_host
                    && active_non_retiring < desired_sessions
                    && connecting_non_retiring == 0;
                let all_slots_retiring = session_pool_requires_forced_turnover(
                    active_count,
                    retiring_active,
                    pool.max_sessions_per_host,
                    replacement_capacity_blocked,
                    forced_turnover_in_progress,
                );
                if all_slots_retiring && force_oldest_retiring_session(&node).await {
                    continue;
                }
                drain_replaced_sessions(
                    &node,
                    if replacement_capacity_blocked {
                        0
                    } else {
                        desired_sessions
                    },
                    drain_timeout,
                )
                .await;
                let background_work_enabled = !tasks.is_empty()
                    || session_spawn_authorized(
                        connectivity_snapshot,
                        requires_remote_availability,
                        demand_pending,
                        startup_spawn_budget,
                    );
                ticker = new_delayed_session_manager_ticker(session_manager_tick_interval(
                    connectivity_snapshot,
                    requires_remote_availability,
                    background_work_enabled,
                ));
            }
        }
    }
}

async fn newest_idle_elastic_session_id(
    node: &Arc<NativeSshNode>,
    minimum_sessions: usize,
) -> Option<u64> {
    let owner = *node.remote_owner.read().await;
    let sessions = node.sessions.read().await.clone();
    let mut healthy_non_retiring = 0_usize;
    let mut candidate = None;
    for session in sessions {
        if session.retire_requested.load(Ordering::Relaxed)
            || session.state.read().await.status != SshSessionStatus::Healthy
        {
            continue;
        }
        healthy_non_retiring += 1;
        if owner != Some(session.id)
            && session.in_flight.load(Ordering::Relaxed) == 0
            && candidate.is_none_or(|current| session.id > current)
        {
            candidate = Some(session.id);
        }
    }
    (healthy_non_retiring > minimum_sessions)
        .then_some(candidate)
        .flatten()
}

async fn oldest_healthy_session_id(node: &Arc<NativeSshNode>) -> Option<u64> {
    let sessions = node.sessions.read().await.clone();
    let mut candidate = None;
    for session in sessions {
        if session.retire_requested.load(Ordering::Relaxed) {
            continue;
        }
        if session.state.read().await.status != SshSessionStatus::Healthy {
            continue;
        }
        if candidate
            .as_ref()
            .is_none_or(|oldest: &Arc<ManagedSshSession>| session.id < oldest.id)
        {
            candidate = Some(session);
        }
    }
    candidate.map(|session| session.id)
}

async fn advance_scheduled_session_rotation(
    node: &Arc<NativeSshNode>,
    rotation: &ScheduledSessionRotation,
) -> ScheduledRotationProgress {
    let Some(replacement_session_id) = rotation.replacement_session_id else {
        return ScheduledRotationProgress::Waiting;
    };
    let sessions = node.sessions.read().await.clone();
    let Some(candidate) = sessions
        .iter()
        .find(|session| session.id == rotation.candidate_session_id)
    else {
        return ScheduledRotationProgress::Cancelled;
    };
    let Some(replacement) = sessions
        .iter()
        .find(|session| session.id == replacement_session_id)
    else {
        return ScheduledRotationProgress::Waiting;
    };
    if replacement.retire_requested.load(Ordering::Relaxed)
        || replacement.state.read().await.status != SshSessionStatus::Healthy
    {
        return ScheduledRotationProgress::Waiting;
    }

    request_session_retirement(candidate).await;
    info!(
        ssh_host = %node.name,
        ssh_session_id = candidate.id,
        replacement_ssh_session_id = replacement.id,
        "replacement SSH session is ready; retiring rotated session"
    );
    ScheduledRotationProgress::Activated
}

struct SessionSpawnContext<'a> {
    outbound: &'a str,
    upstream: &'a SshHostConfig,
    plan: &'a Arc<ResolvedSshPlan>,
    probe: ProbeConfig,
    node: &'a Arc<NativeSshNode>,
    connectivity: &'a ConnectivityHandle,
}

async fn spawn_managed_session(
    tasks: &mut JoinSet<SessionExit>,
    session_id: u64,
    context: SessionSpawnContext<'_>,
) {
    stats::register_ssh_session(stats::SshSessionRegistration {
        id: session_id,
        host_name: context.outbound.to_string(),
        ssh_alias: context.plan.target.alias.clone(),
        address: format_ssh_address(&context.plan.target.host, context.plan.target.port),
    });
    let session = Arc::new(ManagedSshSession {
        id: session_id,
        state: Arc::new(RwLock::new(SshSessionState::default())),
        handle: Arc::new(RwLock::new(None)),
        in_flight: Arc::new(AtomicUsize::new(0)),
        payload_generation: Arc::new(AtomicU64::new(0)),
        retire_requested: AtomicBool::new(false),
    });
    context
        .node
        .sessions
        .write()
        .await
        .push(Arc::clone(&session));
    let outbound = context.outbound.to_string();
    let upstream = context.upstream.clone();
    let plan = Arc::clone(context.plan);
    let connectivity = context.connectivity.clone();
    let session_events = context.node.session_events.clone();
    let span = info_span!(
        "ssh_session",
        host_name = %outbound,
        ssh_session_id = session_id
    );
    info!(
        host_name = %outbound,
        ssh_session_id = session_id,
        "starting native SSH session for pool capacity"
    );
    tasks.spawn(
        run_managed_session(
            outbound,
            upstream,
            plan,
            context.probe,
            session,
            connectivity,
            session_events,
        )
        .instrument(span),
    );
}

async fn run_managed_session(
    outbound: String,
    upstream: SshHostConfig,
    plan: Arc<ResolvedSshPlan>,
    probe: ProbeConfig,
    session: Arc<ManagedSshSession>,
    connectivity: ConnectivityHandle,
    session_events: watch::Sender<u64>,
) -> SessionExit {
    let _session_metric = stats::SshSessionGuard::start(session.id);
    let mut reached_healthy = false;
    let error = match establish_session(&upstream, &plan, &session).await {
        Ok(mut established) => {
            reached_healthy = true;
            {
                *session.handle.write().await = Some(Arc::clone(&established.handle));
                let mut state = session.state.write().await;
                state.status = SshSessionStatus::Healthy;
                state.rtt_millis = Some(established.startup_ms.round() as u64);
                state.startup_ms = Some(established.startup_ms);
                state.last_error = None;
            }
            notify_session_event(&session_events);
            sync_session_runtime_stats(&session, false).await;
            info!(
                host_name = %outbound,
                ssh_session_id = session.id,
                ssh_alias = %plan.target.alias,
                host = %plan.target.host,
                port = plan.target.port,
                startup_ms = established.startup_ms,
                "native SSH session joined the pool"
            );
            monitor_session(
                &established.handle,
                &mut established.disconnect_rx,
                &session,
                probe,
                &connectivity,
            )
            .await
        }
        Err(error) => format!("{error:#}"),
    };
    *session.handle.write().await = None;
    {
        let mut state = session.state.write().await;
        state.status = SshSessionStatus::Offline;
        state.last_error = Some(error.clone());
    }
    sync_session_runtime_stats(&session, false).await;
    if !session.retire_requested.load(Ordering::Relaxed) {
        stats::record_host_error(&outbound, &error);
    }
    SessionExit {
        id: session.id,
        reached_healthy,
        retired: session.retire_requested.load(Ordering::Relaxed),
        error,
    }
}

async fn establish_session(
    upstream: &SshHostConfig,
    plan: &ResolvedSshPlan,
    session: &Arc<ManagedSshSession>,
) -> anyhow::Result<EstablishedSession> {
    let started = Instant::now();
    let remote_forwards = Arc::new(build_remote_routes(upstream));
    let (handle, disconnect_rx, transport_chain) = if plan.jumps.is_empty() {
        let transport = open_initial_transport(&plan.target).await?;
        let (handle, disconnect_rx) = connect_ssh_endpoint(
            &upstream.name,
            session.id,
            &plan.target,
            transport,
            remote_forwards,
            Arc::clone(&session.in_flight),
            Arc::clone(&session.payload_generation),
        )
        .await?;
        (handle, disconnect_rx, Vec::new())
    } else {
        let first = plan.jumps.first().context("SSH ProxyJump chain is empty")?;
        let transport = open_initial_transport(first).await?;
        let (first_handle, _) = connect_ssh_endpoint(
            &upstream.name,
            session.id,
            first,
            transport,
            Arc::new(HashMap::new()),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
        .await?;
        let mut transport_chain = vec![first_handle];

        for endpoint in plan.jumps.iter().skip(1) {
            let transport = open_jump_transport(
                transport_chain
                    .last()
                    .expect("the SSH jump transport chain has a first handle"),
                endpoint,
            )
            .await?;
            let (handle, _) = connect_ssh_endpoint(
                &upstream.name,
                session.id,
                endpoint,
                transport,
                Arc::new(HashMap::new()),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicU64::new(0)),
            )
            .await?;
            transport_chain.push(handle);
        }

        let target_transport = open_jump_transport(
            transport_chain
                .last()
                .expect("the SSH jump transport chain has a handle"),
            &plan.target,
        )
        .await?;
        let (handle, disconnect_rx) = connect_ssh_endpoint(
            &upstream.name,
            session.id,
            &plan.target,
            target_transport,
            remote_forwards,
            Arc::clone(&session.in_flight),
            Arc::clone(&session.payload_generation),
        )
        .await?;
        (handle, disconnect_rx, transport_chain)
    };

    Ok(EstablishedSession {
        handle,
        _transport_chain: transport_chain,
        disconnect_rx,
        startup_ms: elapsed_ms(started),
    })
}

async fn open_initial_transport(
    endpoint: &ResolvedSshEndpoint,
) -> anyhow::Result<BoxedSshTransport> {
    if endpoint.proxy_command.is_some() {
        return Ok(Box::new(ProxyCommandStream::spawn(endpoint)?));
    }
    let stream = timeout(
        endpoint.connect_timeout,
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
    )
    .await
    .with_context(|| {
        format!(
            "timed out opening TCP transport for SSH host {} at {}:{}",
            endpoint.alias, endpoint.host, endpoint.port
        )
    })??;
    stream.set_nodelay(true)?;
    socket2::SockRef::from(&stream).set_keepalive(endpoint.tcp_keep_alive)?;
    Ok(Box::new(stream))
}

async fn open_jump_transport(
    previous: &Arc<NativeSshHandle>,
    endpoint: &ResolvedSshEndpoint,
) -> anyhow::Result<BoxedSshTransport> {
    if endpoint.proxy_command.is_some() {
        bail!(
            "SSH ProxyCommand for host {} cannot replace an active ProxyJump transport",
            endpoint.alias
        );
    }
    let channel = timeout(
        endpoint.connect_timeout,
        previous.channel_open_direct_tcpip(
            endpoint.host.clone(),
            u32::from(endpoint.port),
            "127.0.0.1",
            0,
        ),
    )
    .await
    .with_context(|| {
        format!(
            "timed out opening ProxyJump channel to {}:{}",
            endpoint.host, endpoint.port
        )
    })??;
    Ok(Box::new(channel.into_stream()))
}

async fn connect_ssh_endpoint(
    upstream: &str,
    session_id: u64,
    endpoint: &ResolvedSshEndpoint,
    transport: BoxedSshTransport,
    remote_forwards: Arc<HashMap<u32, RemoteForwardRoute>>,
    in_flight: Arc<AtomicUsize>,
    payload_generation: Arc<AtomicU64>,
) -> anyhow::Result<(Arc<NativeSshHandle>, mpsc::UnboundedReceiver<String>)> {
    let (transport, handshake_abort) = guard_ssh_handshake_transport(transport);
    let config = Arc::new(client::Config {
        nodelay: true,
        keepalive_interval: Some(endpoint.keep_alive),
        keepalive_max: endpoint.keep_alive_max,
        ..Default::default()
    });
    let (disconnect_tx, disconnect_rx) = mpsc::unbounded_channel();
    let handler = NativeSshHandler {
        upstream: upstream.to_string(),
        session_id,
        host: endpoint.host.clone(),
        port: endpoint.port,
        host_key_policy: endpoint.host_key_policy,
        host_key_name: endpoint.host_key_name.clone(),
        known_hosts_paths: endpoint.known_hosts_paths.clone(),
        remote_forwards,
        in_flight,
        payload_generation,
        disconnect_tx,
    };
    let mut handle = timeout(
        endpoint.connect_timeout,
        client::connect_stream(config, transport, handler),
    )
    .await
    .with_context(|| {
        format!(
            "timed out negotiating SSH host {} at {}:{}",
            endpoint.alias, endpoint.host, endpoint.port
        )
    })??;
    timeout(
        endpoint.connect_timeout,
        authenticate(&mut handle, endpoint),
    )
    .await
    .with_context(|| format!("timed out authenticating SSH host {}", endpoint.alias))??;
    handshake_abort.disarm();
    Ok((Arc::new(handle), disconnect_rx))
}

async fn authenticate(
    handle: &mut NativeSshHandle,
    endpoint: &ResolvedSshEndpoint,
) -> anyhow::Result<()> {
    let success = match &endpoint.auth.explicit {
        Some(SshAuthConfig::Agent) => {
            authenticate_with_agent(handle, &endpoint.username, &endpoint.alias).await?
        }
        Some(SshAuthConfig::PrivateKey {
            path,
            passphrase_env,
        }) => {
            let passphrase = passphrase_env
                .as_ref()
                .map(|name| env::var(name).with_context(|| format!("missing environment {name}")))
                .transpose()?;
            let key = keys::load_secret_key(expand_tilde(path), passphrase.as_deref())?;
            let hash = handle.best_supported_rsa_hash().await?.flatten();
            handle
                .authenticate_publickey(
                    endpoint.username.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await?
                .success()
        }
        Some(SshAuthConfig::Password { password_env }) => {
            let password = env::var(password_env)
                .with_context(|| format!("missing environment {password_env}"))?;
            handle
                .authenticate_password(endpoint.username.clone(), password)
                .await?
                .success()
        }
        None => authenticate_from_ssh_config(handle, endpoint).await?,
    };
    if !success {
        bail!("SSH authentication failed for host {}", endpoint.alias);
    }
    Ok(())
}

async fn authenticate_from_ssh_config(
    handle: &mut NativeSshHandle,
    endpoint: &ResolvedSshEndpoint,
) -> anyhow::Result<bool> {
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    for path in &endpoint.auth.identity_files {
        let key = match keys::load_secret_key(path, None) {
            Ok(key) => key,
            Err(error) => {
                debug!(
                    ssh_alias = %endpoint.alias,
                    identity_file = %path.display(),
                    %error,
                    "could not load SSH config identity file"
                );
                continue;
            }
        };
        match handle
            .authenticate_publickey(
                endpoint.username.clone(),
                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
        {
            Ok(result) if result.success() => return Ok(true),
            Ok(_) => debug!(
                ssh_alias = %endpoint.alias,
                identity_file = %path.display(),
                "SSH server rejected identity file"
            ),
            Err(error) => debug!(
                ssh_alias = %endpoint.alias,
                identity_file = %path.display(),
                %error,
                "SSH identity authentication attempt failed"
            ),
        }
    }
    if endpoint.auth.use_agent {
        return authenticate_with_agent(handle, &endpoint.username, &endpoint.alias).await;
    }
    Ok(false)
}

async fn authenticate_with_agent(
    handle: &mut NativeSshHandle,
    username: &str,
    alias: &str,
) -> anyhow::Result<bool> {
    let mut agent = connect_agent()
        .await
        .with_context(|| format!("failed to connect to SSH agent for host {alias}"))?;
    let identities = agent.request_identities().await?;
    if identities.is_empty() {
        return Ok(false);
    }
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    for identity in identities {
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                handle
                    .authenticate_publickey_with(username.to_string(), key, hash, &mut agent)
                    .await?
            }
            AgentIdentity::Certificate { certificate, .. } => {
                handle
                    .authenticate_certificate_with(
                        username.to_string(),
                        certificate,
                        hash,
                        &mut agent,
                    )
                    .await?
            }
        };
        if result.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(unix)]
async fn connect_agent() -> anyhow::Result<DynamicAgent> {
    Ok(AgentClient::<tokio::net::UnixStream>::connect_env()
        .await?
        .dynamic())
}

#[cfg(windows)]
async fn connect_agent() -> anyhow::Result<DynamicAgent> {
    if let Some(path) = env::var_os("SSH_AUTH_SOCK") {
        return Ok(
            AgentClient::<tokio::net::windows::named_pipe::NamedPipeClient>::connect_named_pipe(
                path,
            )
            .await?
            .dynamic(),
        );
    }
    Ok(AgentClient::connect_pageant().await?.dynamic())
}

async fn monitor_session(
    handle: &Arc<NativeSshHandle>,
    disconnect_rx: &mut mpsc::UnboundedReceiver<String>,
    session: &Arc<ManagedSshSession>,
    probe: ProbeConfig,
    connectivity: &ConnectivityHandle,
) -> String {
    if !probe.enabled {
        return disconnect_rx
            .recv()
            .await
            .unwrap_or_else(|| "SSH session disconnected".to_string());
    }
    let mut consecutive_failures = 0_u32;
    let mut consecutive_successes = 0_u32;
    let mut observed_payload_generation = session.payload_generation.load(Ordering::Relaxed);
    let replacement_threshold = probe.fail_threshold.saturating_sub(1).max(1);
    let mut connectivity_rx = connectivity.subscribe();
    let mut connectivity_events_open = true;
    loop {
        if connectivity_rx.borrow().is_offline() {
            consecutive_failures = 0;
            consecutive_successes = 0;
            tokio::select! {
                reason = disconnect_rx.recv() => {
                    return reason.unwrap_or_else(|| "SSH session disconnected".to_string());
                }
                changed = connectivity_rx.changed(), if connectivity_events_open => {
                    if changed.is_err() {
                        connectivity_events_open = false;
                    }
                }
            }
            continue;
        }
        tokio::select! {
            reason = disconnect_rx.recv() => {
                return reason.unwrap_or_else(|| "SSH session disconnected".to_string());
            }
            changed = connectivity_rx.changed(), if connectivity_events_open => {
                if changed.is_err() {
                    connectivity_events_open = false;
                }
                continue;
            }
            _ = sleep(Duration::from_secs(probe.interval_secs.max(1))) => {
                let payload_generation = session.payload_generation.load(Ordering::Relaxed);
                if session.in_flight.load(Ordering::Relaxed) > 0
                    && payload_generation != observed_payload_generation
                {
                    observed_payload_generation = payload_generation;
                    consecutive_failures = 0;
                    consecutive_successes = consecutive_successes.saturating_add(1);
                    let mut state = session.state.write().await;
                    if !session.retire_requested.load(Ordering::Relaxed)
                        && consecutive_successes >= probe.recovery_threshold
                    {
                        state.status = SshSessionStatus::Healthy;
                    }
                    state.last_error = None;
                    continue;
                }
                observed_payload_generation = payload_generation;
                let started = Instant::now();
                let result = timeout(
                    Duration::from_millis(probe.timeout_millis.max(1)),
                    handle.send_ping(),
                ).await;
                stats::record_ssh_session_probe(session.id);
                if connectivity.current().is_offline() {
                    consecutive_failures = 0;
                    consecutive_successes = 0;
                    continue;
                }
                match result {
                    Ok(Ok(())) => {
                        consecutive_failures = 0;
                        consecutive_successes = consecutive_successes.saturating_add(1);
                        let sample = elapsed_ms(started).round() as u64;
                        let mut state = session.state.write().await;
                        if !session.retire_requested.load(Ordering::Relaxed)
                            && consecutive_successes >= probe.recovery_threshold
                        {
                            state.status = SshSessionStatus::Healthy;
                        }
                        state.rtt_millis = Some(ewma_rtt(state.rtt_millis, sample));
                        state.last_error = None;
                    }
                    Ok(Err(error)) => {
                        consecutive_successes = 0;
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        let error = format!("SSH ping failed: {error}");
                        mark_session_probe_failure(
                            session,
                            &error,
                            consecutive_failures >= replacement_threshold,
                        ).await;
                        if probe_failure_requires_disconnect(
                            consecutive_failures,
                            probe.fail_threshold,
                            session.in_flight.load(Ordering::Relaxed),
                        ) {
                            let _ = handle.disconnect(Disconnect::ConnectionLost, &error, "en").await;
                            return error;
                        }
                    }
                    Err(_) => {
                        consecutive_successes = 0;
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        let error = format!(
                            "SSH ping timed out after {} ms",
                            probe.timeout_millis.max(1)
                        );
                        mark_session_probe_failure(
                            session,
                            &error,
                            consecutive_failures >= replacement_threshold,
                        ).await;
                        if probe_failure_requires_disconnect(
                            consecutive_failures,
                            probe.fail_threshold,
                            session.in_flight.load(Ordering::Relaxed),
                        ) {
                            let _ = handle.disconnect(Disconnect::ConnectionLost, &error, "en").await;
                            return error;
                        }
                    }
                }
            }
        }
    }
}

async fn mark_session_probe_failure(
    session: &Arc<ManagedSshSession>,
    error: &str,
    request_replacement: bool,
) {
    if request_replacement {
        request_session_retirement(session).await;
    }
    let mut state = session.state.write().await;
    if state.status != SshSessionStatus::Draining {
        state.status = SshSessionStatus::Suspect;
    }
    state.last_error = Some(error.to_string());
    warn!(
        ssh_session_id = session.id,
        request_replacement,
        %error,
        "native SSH session health probe failed"
    );
}

async fn request_session_retirement(session: &Arc<ManagedSshSession>) {
    if !session.retire_requested.swap(true, Ordering::AcqRel) {
        session.state.write().await.retirement_started = Some(Instant::now());
    }
}

async fn refresh_node_state(node: &Arc<NativeSshNode>) {
    let sessions = node.sessions.read().await.clone();
    let owner_id = *node.remote_owner.read().await;
    let mut healthy_rtt = None;
    let mut active = false;
    let mut last_error = None;
    for session in sessions {
        let state = session.state.read().await;
        active |= state.status != SshSessionStatus::Offline;
        if state.status == SshSessionStatus::Healthy
            && !session.retire_requested.load(Ordering::Relaxed)
        {
            healthy_rtt = match (healthy_rtt, state.rtt_millis) {
                (None, rtt) => rtt,
                (Some(current), Some(rtt)) => Some(current.min(rtt)),
                (current, None) => current,
            };
        }
        if state.last_error.is_some() {
            last_error.clone_from(&state.last_error);
        }
        drop(state);
        sync_session_runtime_stats(&session, owner_id == Some(session.id)).await;
    }
    let mut state = node.state.write().await;
    state.status = if healthy_rtt.is_some() {
        HealthStatus::Healthy
    } else if active {
        HealthStatus::Degraded
    } else {
        HealthStatus::Offline
    };
    state.rtt_millis = healthy_rtt;
    state.last_error = last_error;
    let update = stats::HostStateUpdate {
        status: match state.status {
            HealthStatus::Healthy => stats::HostRuntimeStatus::Healthy,
            HealthStatus::Degraded => stats::HostRuntimeStatus::Degraded,
            HealthStatus::Offline => stats::HostRuntimeStatus::Offline,
            HealthStatus::Unknown => stats::HostRuntimeStatus::Connecting,
        },
        rtt_ms: state.rtt_millis,
        restart_count: state.restart_count,
        last_error: state.last_error.clone(),
    };
    drop(state);
    stats::update_host_state(&node.name, update);
}

async fn sync_session_runtime_stats(session: &Arc<ManagedSshSession>, remote_owner: bool) {
    let state = session.state.read().await;
    stats::update_ssh_session_state(
        session.id,
        stats::SshSessionStateUpdate {
            status: match state.status {
                SshSessionStatus::Connecting => stats::SshSessionRuntimeStatus::Connecting,
                SshSessionStatus::Healthy => stats::SshSessionRuntimeStatus::Healthy,
                SshSessionStatus::Suspect => stats::SshSessionRuntimeStatus::Suspect,
                SshSessionStatus::Draining => stats::SshSessionRuntimeStatus::Draining,
                SshSessionStatus::Offline => stats::SshSessionRuntimeStatus::Offline,
            },
            startup_ms: state.startup_ms,
            rtt_ms: state.rtt_millis,
            retiring: session.retire_requested.load(Ordering::Relaxed),
            remote_forward_owner: remote_owner,
            last_error: state.last_error.clone(),
        },
    );
}

async fn ensure_remote_owner(
    node: &Arc<NativeSshNode>,
    upstream: &SshHostConfig,
    operation_timeout: Duration,
) -> anyhow::Result<()> {
    if upstream.remote_forwards.is_empty() {
        return Ok(());
    }
    let sessions = node.sessions.read().await.clone();
    let owner_id = *node.remote_owner.read().await;
    let current = owner_id.and_then(|id| sessions.iter().find(|session| session.id == id).cloned());
    if let Some(owner) = &current {
        let is_healthy = owner.state.read().await.status == SshSessionStatus::Healthy
            && !owner.retire_requested.load(Ordering::Relaxed);
        if is_healthy && owner.current_handle().await.is_some() {
            return Ok(());
        }
    }

    let mut candidates = Vec::new();
    for session in &sessions {
        if Some(session.id) == owner_id || session.retire_requested.load(Ordering::Relaxed) {
            continue;
        }
        let state = session.state.read().await;
        if state.status == SshSessionStatus::Healthy {
            candidates.push((state.rtt_millis.unwrap_or(u64::MAX), Arc::clone(session)));
        }
    }
    candidates.sort_by_key(|(rtt, _)| *rtt);
    if candidates.is_empty()
        && let Some(owner) = &current
        && owner.retire_requested.load(Ordering::Relaxed)
    {
        if let Some(handle) = owner.current_handle().await {
            cancel_remote_forwards(&handle, upstream, operation_timeout).await?;
        }
        *node.remote_owner.write().await = None;
        bail!("remote forwards released while waiting for a replacement SSH session");
    }
    let (_, replacement) = candidates
        .into_iter()
        .next()
        .context("no healthy standby SSH session is ready for remote forwarding")?;

    if let Some(owner) = current {
        if let Some(handle) = owner.current_handle().await
            && let Err(error) = cancel_remote_forwards(&handle, upstream, operation_timeout).await
        {
            let _ = handle
                .disconnect(
                    Disconnect::ConnectionLost,
                    &format!("remote forward handover: {error}"),
                    "en",
                )
                .await;
            *node.remote_owner.write().await = None;
            bail!(
                "failed to release remote forwards from session {}: {error}",
                owner.id
            );
        }
        *node.remote_owner.write().await = None;
    }

    let handle = replacement
        .current_handle()
        .await
        .context("replacement SSH session closed before remote forward registration")?;
    register_remote_forwards(&handle, upstream, operation_timeout, replacement.id).await?;
    *node.remote_owner.write().await = Some(replacement.id);
    info!(
        ssh_host = %node.name,
        ssh_session_id = replacement.id,
        remote_forward_count = upstream.remote_forwards.len(),
        "SSH remote forwards assigned to session owner"
    );
    Ok(())
}

async fn register_remote_forwards(
    handle: &Arc<NativeSshHandle>,
    upstream: &SshHostConfig,
    operation_timeout: Duration,
    owner_session_id: u64,
) -> anyhow::Result<()> {
    let mut registered: Vec<(SocketAddr, String)> = Vec::new();
    for forward in &upstream.remote_forwards {
        let listen = remote_forward_listen(forward);
        let tunnel_id = remote_forward_tunnel_id(upstream, forward);
        let result = timeout(
            operation_timeout,
            handle.tcpip_forward(listen.ip().to_string(), u32::from(listen.port())),
        )
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                let error = format!("timed out registering SSH remote forward at {listen}");
                stats::update_tunnel_status(
                    &tunnel_id,
                    stats::TunnelRuntimeStatus::Error,
                    None,
                    Some(error.clone()),
                );
                stats::record_tunnel_error(&upstream.name, &tunnel_id, &error);
                let _ = timeout(
                    operation_timeout,
                    handle.cancel_tcpip_forward(listen.ip().to_string(), u32::from(listen.port())),
                )
                .await;
                rollback_registered_remote_forwards(
                    handle,
                    upstream,
                    operation_timeout,
                    &registered,
                    &error,
                )
                .await;
                bail!(error);
            }
        };
        if let Err(error) = result {
            let error_text = format!("failed to register SSH remote forward at {listen}: {error}");
            stats::update_tunnel_status(
                &tunnel_id,
                stats::TunnelRuntimeStatus::Error,
                None,
                Some(error_text.clone()),
            );
            stats::record_tunnel_error(&upstream.name, &tunnel_id, &error_text);
            rollback_registered_remote_forwards(
                handle,
                upstream,
                operation_timeout,
                &registered,
                &error_text,
            )
            .await;
            return Err(error)
                .with_context(|| format!("failed to register SSH remote forward at {listen}"));
        }
        registered.push((listen, tunnel_id.clone()));
        stats::update_tunnel_status(
            &tunnel_id,
            stats::TunnelRuntimeStatus::Listening,
            Some(owner_session_id),
            None,
        );
    }
    Ok(())
}

async fn rollback_registered_remote_forwards(
    handle: &Arc<NativeSshHandle>,
    upstream: &SshHostConfig,
    operation_timeout: Duration,
    registered: &[(SocketAddr, String)],
    cause: &str,
) {
    for (listen, tunnel_id) in registered {
        let cancellation = timeout(
            operation_timeout,
            handle.cancel_tcpip_forward(listen.ip().to_string(), u32::from(listen.port())),
        )
        .await;
        let rollback_error = match cancellation {
            Ok(Ok(())) => format!(
                "remote forward at {listen} was rolled back because another listener failed: {cause}"
            ),
            Ok(Err(error)) => format!(
                "remote forward rollback at {listen} failed after another listener failed: {error}"
            ),
            Err(_) => format!(
                "remote forward rollback at {listen} timed out after another listener failed: {cause}"
            ),
        };
        stats::update_tunnel_status(
            tunnel_id,
            stats::TunnelRuntimeStatus::Error,
            None,
            Some(rollback_error.clone()),
        );
        stats::record_tunnel_error(&upstream.name, tunnel_id, &rollback_error);
    }
}

async fn cancel_remote_forwards(
    handle: &Arc<NativeSshHandle>,
    upstream: &SshHostConfig,
    operation_timeout: Duration,
) -> anyhow::Result<()> {
    for forward in &upstream.remote_forwards {
        let listen = remote_forward_listen(forward);
        timeout(
            operation_timeout,
            handle.cancel_tcpip_forward(listen.ip().to_string(), u32::from(listen.port())),
        )
        .await
        .with_context(|| format!("timed out cancelling SSH remote forward at {listen}"))??;
        stats::update_tunnel_status(
            &remote_forward_tunnel_id(upstream, forward),
            stats::TunnelRuntimeStatus::Starting,
            None,
            None,
        );
    }
    Ok(())
}

fn mark_remote_tunnels_starting(upstream: &SshHostConfig) {
    for forward in &upstream.remote_forwards {
        stats::update_tunnel_status(
            &remote_forward_tunnel_id(upstream, forward),
            stats::TunnelRuntimeStatus::Starting,
            None,
            None,
        );
    }
}

fn remote_forward_tunnel_id(upstream: &SshHostConfig, forward: &SshRemoteForwardConfig) -> String {
    match forward {
        SshRemoteForwardConfig::Tcp { name, .. } => {
            stats::tunnel_id(&upstream.name, stats::TunnelKind::RemoteForward, name)
        }
        SshRemoteForwardConfig::Dynamic { name, .. } => {
            stats::tunnel_id(&upstream.name, stats::TunnelKind::RemoteProxy, name)
        }
    }
}

async fn drain_replaced_sessions(
    node: &Arc<NativeSshNode>,
    minimum_healthy: usize,
    drain_timeout: Duration,
) {
    let owner = *node.remote_owner.read().await;
    let sessions = node.sessions.read().await.clone();
    let healthy_non_retiring = {
        let mut count = 0;
        for session in &sessions {
            let state = session.state.read().await;
            if state.status == SshSessionStatus::Healthy
                && !session.retire_requested.load(Ordering::Relaxed)
            {
                count += 1;
            }
        }
        count
    };
    if healthy_non_retiring < minimum_healthy {
        return;
    }

    let mut draining_in_progress = false;
    for session in &sessions {
        if session.state.read().await.status == SshSessionStatus::Draining {
            draining_in_progress = true;
            break;
        }
    }

    for session in sessions {
        if !session.retire_requested.load(Ordering::Relaxed) || owner == Some(session.id) {
            continue;
        }
        let mut state = session.state.write().await;
        if state.forced_turnover_requested {
            continue;
        }
        let now = Instant::now();
        let payload_generation = session.payload_generation.load(Ordering::Relaxed);
        if state.status != SshSessionStatus::Draining {
            if draining_in_progress {
                continue;
            }
            state.status = SshSessionStatus::Draining;
            state.drain_started = Some(now);
            state.drain_idle_since = Some(now);
            state.drain_payload_generation = payload_generation;
            draining_in_progress = true;
            info!(
                ssh_host = %node.name,
                ssh_session_id = session.id,
                active_channels = session.in_flight.load(Ordering::Relaxed),
                "SSH session started draining"
            );
        }
        let decision = session_drain_decision(
            &mut state,
            session.in_flight.load(Ordering::Relaxed),
            payload_generation,
            now,
            drain_timeout,
        );
        drop(state);
        if let Some(reason) = decision.disconnect_reason()
            && let Some(handle) = session.current_handle().await
        {
            let _ = handle
                .disconnect(Disconnect::ByApplication, reason, "en")
                .await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionDrainDecision {
    Waiting,
    Drained,
    Stalled,
}

impl SessionDrainDecision {
    fn disconnect_reason(self) -> Option<&'static str> {
        match self {
            Self::Waiting => None,
            Self::Drained => Some("replacement session is ready and active channels drained"),
            Self::Stalled => Some("SSH session made no payload progress before the drain timeout"),
        }
    }
}

fn session_drain_decision(
    state: &mut SshSessionState,
    active_channels: usize,
    payload_generation: u64,
    now: Instant,
    drain_timeout: Duration,
) -> SessionDrainDecision {
    if active_channels == 0 {
        return SessionDrainDecision::Drained;
    }
    if payload_generation != state.drain_payload_generation {
        state.drain_payload_generation = payload_generation;
        state.drain_idle_since = Some(now);
        return SessionDrainDecision::Waiting;
    }
    if state
        .drain_idle_since
        .is_some_and(|idle_since| now.saturating_duration_since(idle_since) >= drain_timeout)
    {
        SessionDrainDecision::Stalled
    } else {
        SessionDrainDecision::Waiting
    }
}

async fn force_oldest_retiring_session(node: &Arc<NativeSshNode>) -> bool {
    let Some((is_owner, session)) = oldest_retiring_session(node).await else {
        return false;
    };
    let Some(handle) = session.current_handle().await else {
        return false;
    };
    {
        let mut state = session.state.write().await;
        if state.forced_turnover_requested {
            return false;
        }
        state.forced_turnover_requested = true;
        state.status = SshSessionStatus::Draining;
        state.drain_started.get_or_insert_with(Instant::now);
        state.last_error =
            Some("forced turnover because all max-sessions slots were draining".to_string());
    }
    warn!(
        ssh_host = %node.name,
        ssh_session_id = session.id,
        active_channels = session.in_flight.load(Ordering::Relaxed),
        remote_forward_owner = is_owner,
        "forcing the longest-draining SSH session to leave the saturated pool"
    );
    let reason = "all max-sessions slots are draining; forcing the oldest draining session out";
    if let Err(error) = handle
        .disconnect(Disconnect::ByApplication, reason, "en")
        .await
    {
        let mut state = session.state.write().await;
        state.forced_turnover_requested = false;
        state.last_error = Some(format!(
            "failed to force draining session turnover: {error}"
        ));
        warn!(
            ssh_host = %node.name,
            ssh_session_id = session.id,
            %error,
            "failed to force the longest-draining SSH session out of the saturated pool"
        );
        return false;
    }
    true
}

async fn oldest_retiring_session(
    node: &Arc<NativeSshNode>,
) -> Option<(bool, Arc<ManagedSshSession>)> {
    let owner = *node.remote_owner.read().await;
    let sessions = node.sessions.read().await.clone();
    let mut candidates = Vec::new();
    for session in sessions {
        if !session.retire_requested.load(Ordering::Relaxed) {
            continue;
        }
        let state = session.state.read().await;
        if state.status == SshSessionStatus::Offline || state.forced_turnover_requested {
            continue;
        }
        let elapsed = state
            .drain_started
            .or(state.retirement_started)
            .map(|started| started.elapsed())
            .unwrap_or_default();
        candidates.push((owner == Some(session.id), elapsed, Arc::clone(&session)));
    }
    let prefer_non_owner = candidates.iter().any(|(is_owner, _, _)| !is_owner);
    candidates
        .into_iter()
        .filter(|(is_owner, _, _)| !prefer_non_owner || !is_owner)
        .max_by_key(|(_, elapsed, _)| *elapsed)
        .map(|(is_owner, _, session)| (is_owner, session))
}

fn build_remote_routes(upstream: &SshHostConfig) -> HashMap<u32, RemoteForwardRoute> {
    upstream
        .remote_forwards
        .iter()
        .map(|forward| match forward {
            SshRemoteForwardConfig::Tcp {
                name,
                listen,
                local_host,
                local_port,
            } => (
                u32::from(listen.port()),
                RemoteForwardRoute::Tcp {
                    name: name.clone(),
                    tunnel_id: stats::tunnel_id(
                        &upstream.name,
                        stats::TunnelKind::RemoteForward,
                        name,
                    ),
                    local_host: local_host.clone(),
                    local_port: *local_port,
                },
            ),
            SshRemoteForwardConfig::Dynamic {
                name,
                listen,
                protocol,
            } => (
                u32::from(listen.port()),
                RemoteForwardRoute::Dynamic {
                    name: name.clone(),
                    tunnel_id: stats::tunnel_id(
                        &upstream.name,
                        stats::TunnelKind::RemoteProxy,
                        name,
                    ),
                    protocol: *protocol,
                },
            ),
        })
        .collect()
}

fn remote_forward_listen(forward: &SshRemoteForwardConfig) -> SocketAddr {
    match forward {
        SshRemoteForwardConfig::Tcp { listen, .. }
        | SshRemoteForwardConfig::Dynamic { listen, .. } => *listen,
    }
}

fn target_host_port(target: &TargetAddr) -> (String, u32) {
    match target {
        TargetAddr::DomainPort { domain, port } => (domain.clone(), u32::from(*port)),
        TargetAddr::Socket(address) => (address.ip().to_string(), u32::from(address.port())),
    }
}

fn format_ssh_address(host: &str, port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));
        if let Some(home) = home {
            let suffix = path
                .strip_prefix("~/")
                .or_else(|| path.strip_prefix("~\\"))
                .unwrap_or_default();
            return Path::new(&home).join(suffix);
        }
    }
    PathBuf::from(path)
}

fn next_backoff(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

fn ewma_rtt(current: Option<u64>, sample: u64) -> u64 {
    current
        .map(|current| current.saturating_mul(7).saturating_add(sample) / 8)
        .unwrap_or(sample)
}

struct ActiveChannelGuard {
    counter: Arc<AtomicUsize>,
    session_id: u64,
}

impl ActiveChannelGuard {
    fn new(counter: Arc<AtomicUsize>, session_id: u64) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        stats::ssh_session_channel_accepted(session_id);
        Self {
            counter,
            session_id,
        }
    }
}

impl Drop for ActiveChannelGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
        stats::ssh_session_channel_closed(self.session_id);
    }
}

struct InFlightReservation {
    counter: Arc<AtomicUsize>,
    payload_generation: Arc<AtomicU64>,
    session_id: u64,
    active: bool,
}

impl InFlightReservation {
    fn into_stream<S>(mut self, stream: S) -> CountedStream<S> {
        self.active = false;
        CountedStream {
            inner: stream,
            counter: Arc::clone(&self.counter),
            payload_generation: Arc::clone(&self.payload_generation),
            session_id: self.session_id,
            transfer_recorder: stats::session_transfer_recorder(self.session_id),
        }
    }
}

impl Drop for InFlightReservation {
    fn drop(&mut self) {
        if self.active {
            self.counter.fetch_sub(1, Ordering::AcqRel);
            stats::ssh_session_channel_closed(self.session_id);
        }
    }
}

fn reserve_in_flight(
    counter: &Arc<AtomicUsize>,
    payload_generation: &Arc<AtomicU64>,
    maximum: usize,
    session_id: u64,
) -> Option<InFlightReservation> {
    let previous = counter.fetch_add(1, Ordering::AcqRel);
    if previous >= maximum {
        counter.fetch_sub(1, Ordering::AcqRel);
        return None;
    }
    stats::ssh_session_channel_reserved(session_id);
    Some(InFlightReservation {
        counter: Arc::clone(counter),
        payload_generation: Arc::clone(payload_generation),
        session_id,
        active: true,
    })
}

struct CountedStream<S> {
    inner: S,
    counter: Arc<AtomicUsize>,
    payload_generation: Arc<AtomicU64>,
    session_id: u64,
    transfer_recorder: stats::TransferRecorder,
}

impl<S> Drop for CountedStream<S> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
        stats::ssh_session_channel_closed(self.session_id);
    }
}

impl<S> AsyncRead for CountedStream<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = &result {
            let read = buffer.filled().len().saturating_sub(before);
            if read > 0 {
                self.payload_generation.fetch_add(1, Ordering::Relaxed);
            }
            self.transfer_recorder.record(0, read as u64);
        }
        result
    }
}

impl<S> AsyncWrite for CountedStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(context, buffer);
        if let Poll::Ready(Ok(written)) = result {
            if written > 0 {
                self.payload_generation.fetch_add(1, Ordering::Relaxed);
            }
            self.transfer_recorder.record(written as u64, 0);
            Poll::Ready(Ok(written))
        } else {
            result
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

struct PayloadActivityStream<S> {
    inner: S,
    payload_generation: Arc<AtomicU64>,
}

impl<S> PayloadActivityStream<S> {
    fn new(inner: S, payload_generation: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            payload_generation,
        }
    }
}

impl<S> AsyncRead for PayloadActivityStream<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = &result
            && buffer.filled().len() > before
        {
            self.payload_generation.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}

impl<S> AsyncWrite for PayloadActivityStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(context, buffer);
        if let Poll::Ready(Ok(written)) = result {
            if written > 0 {
                self.payload_generation.fetch_add(1, Ordering::Relaxed);
            }
            Poll::Ready(Ok(written))
        } else {
            result
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SshHostKeyPolicy;
    use russh::server;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::net::TcpStream;

    static NEXT_TEST_KEY_ID: AtomicU64 = AtomicU64::new(1);

    struct StalledHandshakeClient;

    impl client::Handler for StalledHandshakeClient {
        type Error = anyhow::Error;

        async fn check_server_key(
            &mut self,
            _server_public_key: &russh::keys::ssh_key::PublicKey,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    async fn start_stalled_ssh_handshake_server()
    -> (SocketAddr, tokio::task::JoinHandle<std::io::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            stream.write_all(b"SSH-2.0-stalled-test\r\n").await?;
            let mut buffer = [0_u8; 4096];
            loop {
                if stream.read(&mut buffer).await? == 0 {
                    return Ok(());
                }
            }
        });
        (address, task)
    }

    #[tokio::test]
    async fn cancelled_ssh_handshake_closes_spawned_russh_transport() {
        let (address, server_task) = start_stalled_ssh_handshake_server().await;
        let stream = TcpStream::connect(address).await.unwrap();
        let (transport, handshake_abort) = guard_ssh_handshake_transport(Box::new(stream));
        let config = Arc::new(client::Config {
            keepalive_interval: Some(Duration::from_millis(10)),
            ..Default::default()
        });

        let result = timeout(
            Duration::from_millis(100),
            client::connect_stream(config, transport, StalledHandshakeClient),
        )
        .await;
        assert!(
            result.is_err(),
            "the deliberately stalled KEX must time out"
        );
        drop(handshake_abort);

        timeout(Duration::from_secs(1), server_task)
            .await
            .expect("cancelled SSH handshake left its TCP transport open")
            .unwrap()
            .unwrap();
    }

    fn detached_session(
        id: u64,
        status: SshSessionStatus,
        retiring: bool,
    ) -> Arc<ManagedSshSession> {
        Arc::new(ManagedSshSession {
            id,
            state: Arc::new(RwLock::new(SshSessionState {
                status,
                retirement_started: retiring.then(Instant::now),
                ..SshSessionState::default()
            })),
            handle: Arc::new(RwLock::new(None)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            payload_generation: Arc::new(AtomicU64::new(0)),
            retire_requested: AtomicBool::new(retiring),
        })
    }

    fn detached_node(sessions: Vec<Arc<ManagedSshSession>>) -> Arc<NativeSshNode> {
        Arc::new(NativeSshNode {
            name: "detached-test-node".to_string(),
            state: Arc::new(RwLock::new(SshNodeState::default())),
            sessions: Arc::new(RwLock::new(sessions)),
            remote_owner: Arc::new(RwLock::new(None)),
            connect_demand: Arc::new(Notify::new()),
            session_events: watch::channel(0).0,
            channel_open_timeout: Duration::from_secs(1),
            max_channels_per_session: 8,
        })
    }

    #[test]
    fn restart_backoff_is_capped() {
        assert_eq!(
            next_backoff(Duration::from_secs(20), Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn ssh_session_start_concurrency_is_bounded_per_host() {
        assert_eq!(initial_session_start_count(1), 1);
        assert_eq!(initial_session_start_count(10), 2);
        assert!(can_start_another_session(1));
        assert!(!can_start_another_session(2));
    }

    #[test]
    fn resume_settle_deadline_survives_intermediate_manager_events() {
        let now = Instant::now();
        let deadline = now + RESUME_SETTLE_INTERVAL;

        assert_eq!(
            session_manager_ticker_delay(Some(deadline), now),
            RESUME_SETTLE_INTERVAL
        );
        assert_eq!(
            session_manager_ticker_delay(Some(deadline), now + RESUME_SETTLE_INTERVAL / 2),
            RESUME_SETTLE_INTERVAL / 2
        );
        assert_eq!(
            session_manager_ticker_delay(Some(deadline), deadline),
            Duration::ZERO
        );
    }

    #[test]
    fn connectivity_policy_blocks_offline_spawns_and_preserves_remote_recovery() {
        let snapshot = |availability, events_available| ConnectivitySnapshot {
            availability,
            events_available,
            resume_generation: 0,
            generation: 1,
        };
        assert!(!session_spawn_authorized(
            snapshot(NetworkAvailability::Offline, true),
            true,
            true,
            1,
        ));
        assert!(!session_spawn_authorized(
            snapshot(NetworkAvailability::Online, false),
            false,
            false,
            0,
        ));
        assert!(session_spawn_authorized(
            snapshot(NetworkAvailability::Online, false),
            false,
            true,
            0,
        ));
        assert!(session_spawn_authorized(
            snapshot(NetworkAvailability::Online, false),
            true,
            false,
            0,
        ));
        assert_eq!(
            session_manager_tick_interval(
                snapshot(NetworkAvailability::Offline, false),
                true,
                false,
            ),
            OFFLINE_REMOTE_REFRESH_INTERVAL
        );
    }

    #[tokio::test]
    async fn offline_connectivity_defers_initial_spawn_until_network_recovers() {
        let (ssh_address, server) = start_test_ssh_server().await;
        let key_path = write_test_key("offline-gate");
        let (connectivity, controller) =
            ConnectivityHandle::controlled(NetworkAvailability::Offline, true);
        let pool = Arc::new(
            SshPoolDialer::start_with_connectivity(
                "offline-gate",
                test_pool_config(ssh_address, &key_path, 1, 1, 8, Vec::new()),
                ProbeConfig {
                    enabled: false,
                    ..ProbeConfig::default()
                },
                connectivity,
            )
            .unwrap(),
        );
        let (target, echo) = start_echo_server().await;
        let dial_pool = Arc::clone(&pool);
        let dial = tokio::spawn(async move {
            dial_pool
                .dial(DialContext {
                    host_name: "offline-gate".to_string(),
                    target: TargetAddr::Socket(target),
                    connection_id: None,
                })
                .await
        });

        sleep(Duration::from_millis(100)).await;
        assert!(pool.nodes[0].sessions.read().await.is_empty());
        assert!(
            !dial.is_finished(),
            "an accepted connection must wait rather than fail while offline"
        );

        controller.set(NetworkAvailability::Online);
        let mut stream = timeout(Duration::from_secs(3), dial)
            .await
            .expect("pending dial did not recover with the network")
            .unwrap()
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut reply = [0_u8; 4];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ping");

        drop(stream);
        drop(pool);
        let _ = std::fs::remove_file(key_path);
        echo.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn eventless_local_pool_reconnects_on_incoming_demand() {
        let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_address = reserved.local_addr().unwrap();
        drop(reserved);
        let key_path = write_test_key("eventless-demand");
        let (connectivity, _controller) =
            ConnectivityHandle::controlled(NetworkAvailability::Online, false);
        let pool = SshPoolDialer::start_with_connectivity(
            "eventless-demand",
            test_pool_config(ssh_address, &key_path, 1, 1, 8, Vec::new()),
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
            connectivity,
        )
        .unwrap();

        timeout(Duration::from_secs(2), async {
            loop {
                if pool.nodes[0].state.read().await.restart_count >= 1
                    && pool.nodes[0].sessions.read().await.is_empty()
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("initial eventless connection attempt did not finish");

        let ssh_listener = TcpListener::bind(ssh_address).await.unwrap();
        let server = start_test_ssh_server_on(ssh_listener);
        let (target, echo) = start_echo_server().await;
        let mut stream = pool
            .dial(DialContext {
                host_name: "eventless-demand".to_string(),
                target: TargetAddr::Socket(target),
                connection_id: None,
            })
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut reply = [0_u8; 4];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ping");

        drop(stream);
        drop(pool);
        let _ = std::fs::remove_file(key_path);
        echo.await.unwrap();
        server.abort();
    }

    #[test]
    fn elastic_session_pool_uses_hysteresis() {
        assert!(!session_pool_is_under_pressure(2, 1, 4));
        assert!(session_pool_is_under_pressure(3, 1, 4));
        assert!(session_pool_is_under_pressure(6, 2, 4));

        assert!(!session_pool_can_scale_down(2, 2, 1, 4));
        assert!(session_pool_can_scale_down(1, 2, 1, 4));
        assert!(!session_pool_can_scale_down(0, 1, 1, 4));
    }

    #[test]
    fn saturated_pool_forces_turnover_only_when_every_slot_is_retiring() {
        assert!(session_pool_requires_forced_turnover(3, 3, 3, true, false));
        assert!(!session_pool_requires_forced_turnover(3, 2, 3, true, false));
        assert!(!session_pool_requires_forced_turnover(2, 2, 3, true, false));
        assert!(!session_pool_requires_forced_turnover(
            3, 3, 3, false, false
        ));
        assert!(!session_pool_requires_forced_turnover(3, 3, 3, true, true));
    }

    #[test]
    fn failed_probe_does_not_disconnect_a_session_with_active_channels() {
        assert!(!probe_failure_requires_disconnect(3, 3, 1));
        assert!(!probe_failure_requires_disconnect(2, 3, 0));
        assert!(probe_failure_requires_disconnect(3, 3, 0));
    }

    #[tokio::test]
    async fn suspect_and_retiring_sessions_do_not_accept_new_channels() {
        let suspect = detached_session(1, SshSessionStatus::Suspect, false);
        let retiring = detached_session(2, SshSessionStatus::Healthy, true);
        let node = detached_node(vec![suspect, retiring]);
        assert!(!node.has_available_session().await);
    }

    #[test]
    fn draining_session_waits_while_payload_is_progressing() {
        let now = Instant::now();
        let timeout = Duration::from_secs(30);
        let mut state = SshSessionState {
            status: SshSessionStatus::Draining,
            drain_started: Some(now - Duration::from_secs(60)),
            drain_idle_since: Some(now - Duration::from_secs(31)),
            drain_payload_generation: 10,
            ..SshSessionState::default()
        };

        assert_eq!(
            session_drain_decision(&mut state, 1, 11, now, timeout),
            SessionDrainDecision::Waiting
        );
        assert_eq!(state.drain_idle_since, Some(now));
        assert_eq!(
            session_drain_decision(&mut state, 1, 11, now + Duration::from_secs(29), timeout,),
            SessionDrainDecision::Waiting
        );
        assert_eq!(
            session_drain_decision(&mut state, 1, 11, now + Duration::from_secs(30), timeout,),
            SessionDrainDecision::Stalled
        );
        assert_eq!(
            session_drain_decision(&mut state, 0, 11, now, timeout),
            SessionDrainDecision::Drained
        );
    }

    #[tokio::test]
    async fn forced_turnover_prefers_the_oldest_non_owner_session() {
        let now = Instant::now();
        let owner = detached_session(1, SshSessionStatus::Draining, true);
        let oldest_non_owner = detached_session(2, SshSessionStatus::Draining, true);
        let newest_non_owner = detached_session(3, SshSessionStatus::Draining, true);
        owner.state.write().await.drain_started = Some(now - Duration::from_secs(90));
        oldest_non_owner.state.write().await.drain_started = Some(now - Duration::from_secs(60));
        newest_non_owner.state.write().await.drain_started = Some(now - Duration::from_secs(30));
        let node = detached_node(vec![
            Arc::clone(&owner),
            Arc::clone(&oldest_non_owner),
            newest_non_owner,
        ]);
        *node.remote_owner.write().await = Some(owner.id);

        let (is_owner, selected) = oldest_retiring_session(&node).await.unwrap();
        assert!(!is_owner);
        assert_eq!(selected.id, oldest_non_owner.id);

        *node.remote_owner.write().await = None;
        let (_, selected) = oldest_retiring_session(&node).await.unwrap();
        assert_eq!(selected.id, owner.id);
    }

    #[test]
    fn target_host_port_preserves_domains_and_ips() {
        assert_eq!(
            target_host_port(&TargetAddr::from_host_port("example.com", 443)),
            ("example.com".to_string(), 443)
        );
        assert_eq!(
            target_host_port(&TargetAddr::from_host_port("127.0.0.1", 8080)),
            ("127.0.0.1".to_string(), 8080)
        );
    }

    #[test]
    fn remote_routes_distinguish_fixed_and_dynamic_modes() {
        let upstream = sample_upstream();
        let routes = build_remote_routes(&upstream);
        assert!(matches!(
            routes.get(&18080),
            Some(RemoteForwardRoute::Tcp { .. })
        ));
        assert!(matches!(
            routes.get(&1080),
            Some(RemoteForwardRoute::Dynamic { .. })
        ));
    }

    #[tokio::test]
    async fn dynamic_remote_forward_serves_socks5_without_external_ssh_process() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4];
            stream.read_exact(&mut buffer).await.unwrap();
            stream.write_all(&buffer).await.unwrap();
        });
        let (mut client, server) = duplex(4096);
        let proxy = tokio::spawn(serve_remote_proxy(
            RemoteProxyContext {
                upstream: "test-upstream".to_string(),
                session_id: 1,
                connection_id: next_connection_id(),
                forward: "remote-socks".to_string(),
                tunnel_id: "test-upstream/remote-proxy/remote-socks".to_string(),
                protocol: ProxyProtocol::Socks5h,
                peer_addr: "127.0.0.1:12345".parse().unwrap(),
                payload_generation: Arc::new(AtomicU64::new(0)),
            },
            server,
        ));

        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method = [0_u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [0x05, 0x00]);

        let mut request = vec![0x05, 0x01, 0x00, 0x01];
        request.extend_from_slice(
            &target
                .ip()
                .to_string()
                .parse::<std::net::Ipv4Addr>()
                .unwrap()
                .octets(),
        );
        request.extend_from_slice(&target.port().to_be_bytes());
        client.write_all(&request).await.unwrap();
        let mut reply = [0_u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], SOCKS5_REPLY_SUCCEEDED);

        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0_u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        drop(client);

        proxy.await.unwrap().unwrap();
        echo.await.unwrap();
    }

    #[tokio::test]
    async fn mixed_remote_proxy_supports_http_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4];
            stream.read_exact(&mut buffer).await.unwrap();
            stream.write_all(&buffer).await.unwrap();
        });
        let (mut client, server) = duplex(4096);
        let proxy = tokio::spawn(serve_remote_proxy(
            RemoteProxyContext {
                upstream: "test-upstream".to_string(),
                session_id: 2,
                connection_id: next_connection_id(),
                forward: "remote-mixed".to_string(),
                tunnel_id: "test-upstream/remote-proxy/remote-mixed".to_string(),
                protocol: ProxyProtocol::Mixed,
                peer_addr: "127.0.0.1:12345".parse().unwrap(),
                payload_generation: Arc::new(AtomicU64::new(0)),
            },
            server,
        ));

        client
            .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = Vec::new();
        loop {
            response.push(client.read_u8().await.unwrap());
            if response.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert!(response.starts_with(b"HTTP/1.1 200 "));

        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0_u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        drop(client);

        proxy.await.unwrap().unwrap();
        echo.await.unwrap();
    }

    #[tokio::test]
    async fn native_ssh_pool_supports_dynamic_and_remote_forwarding() {
        let (dynamic_target, dynamic_echo) = start_echo_server().await;
        let (remote_fixed_target, remote_fixed_echo) = start_echo_server().await;
        let (remote_dynamic_target, remote_dynamic_echo) = start_echo_server().await;
        let remote_fixed_listen = unused_local_address().await;
        let remote_dynamic_listen = unused_local_address().await;

        let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_address = ssh_listener.local_addr().unwrap();
        let server_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let server_config = Arc::new(server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![server_key],
            ..Default::default()
        });
        let server = tokio::spawn(async move {
            let (socket, _) = ssh_listener.accept().await.unwrap();
            let running = server::run_stream(server_config, socket, TestSshServer::default())
                .await
                .unwrap();
            let _ = running.await;
        });

        let client_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let key_path = env::temp_dir().join(format!(
            "stk-native-ssh-test-{}",
            NEXT_TEST_KEY_ID.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let key_text = client_key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        std::fs::write(&key_path, key_text.as_bytes()).unwrap();

        let pool_config = SshPoolConfig {
            policy: crate::config::LoadBalancePolicy::RoundRobin,
            keep_alive_secs: Some(5),
            min_sessions_per_host: 1,
            max_sessions_per_host: 2,
            session_rotation_enabled: false,
            session_rotation_interval_secs: 3_600,
            max_channels_per_session: 8,
            server_alive_count_max: Some(2),
            connect_timeout_secs: Some(2),
            restart_initial_millis: 50,
            restart_max_secs: 1,
            session_spawn_cooldown_millis: 20,
            session_drain_timeout_secs: 2,
            hosts: vec![SshHostConfig {
                name: "local-test".to_string(),
                host: Some(ssh_address.ip().to_string()),
                ssh_config_host: None,
                port: Some(ssh_address.port()),
                username: Some("test".to_string()),
                auth: Some(SshAuthConfig::PrivateKey {
                    path: key_path.to_string_lossy().into_owned(),
                    passphrase_env: None,
                }),
                ssh_config_path: None,
                host_key_policy: Some(SshHostKeyPolicy::InsecureAcceptAny),
                known_hosts_path: None,
                remote_forwards: vec![
                    SshRemoteForwardConfig::Tcp {
                        name: "remote-fixed".to_string(),
                        listen: remote_fixed_listen,
                        local_host: "localhost".to_string(),
                        local_port: remote_fixed_target.port(),
                    },
                    SshRemoteForwardConfig::Dynamic {
                        name: "remote-dynamic".to_string(),
                        listen: remote_dynamic_listen,
                        protocol: crate::config::ProxyProtocol::Socks5h,
                    },
                ],
            }],
        };
        let pool = SshPoolDialer::start(
            "native-test",
            pool_config,
            ProbeConfig {
                interval_secs: 1,
                timeout_millis: 500,
                ..ProbeConfig::default()
            },
        )
        .unwrap();

        timeout(Duration::from_secs(3), async {
            loop {
                if pool.nodes[0].state.read().await.status == HealthStatus::Healthy {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("native SSH session did not become healthy");

        let mut stream = pool
            .dial(DialContext {
                host_name: "native-test".to_string(),
                target: TargetAddr::from_host_port("localhost", dynamic_target.port()),
                connection_id: None,
            })
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut echoed = [0_u8; 4];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        drop(stream);

        let mut remote_fixed_stream = timeout(Duration::from_secs(3), async {
            loop {
                match TcpStream::connect(remote_fixed_listen).await {
                    Ok(stream) => break stream,
                    Err(_) => sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("remote fixed forward did not start listening");
        remote_fixed_stream.write_all(b"ping").await.unwrap();
        remote_fixed_stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        drop(remote_fixed_stream);

        let mut remote_dynamic_stream = timeout(Duration::from_secs(3), async {
            loop {
                match TcpStream::connect(remote_dynamic_listen).await {
                    Ok(stream) => break stream,
                    Err(_) => sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("remote dynamic forward did not start listening");
        remote_dynamic_stream
            .write_all(&[0x05, 0x01, 0x00])
            .await
            .unwrap();
        let mut method = [0_u8; 2];
        remote_dynamic_stream.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [0x05, 0x00]);
        let mut request = vec![0x05, 0x01, 0x00, 0x01];
        request.extend_from_slice(
            &remote_dynamic_target
                .ip()
                .to_string()
                .parse::<std::net::Ipv4Addr>()
                .unwrap()
                .octets(),
        );
        request.extend_from_slice(&remote_dynamic_target.port().to_be_bytes());
        remote_dynamic_stream.write_all(&request).await.unwrap();
        let mut socks_reply = [0_u8; 10];
        remote_dynamic_stream
            .read_exact(&mut socks_reply)
            .await
            .unwrap();
        assert_eq!(socks_reply[1], SOCKS5_REPLY_SUCCEEDED);
        remote_dynamic_stream.write_all(b"ping").await.unwrap();
        remote_dynamic_stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        drop(remote_dynamic_stream);

        drop(pool);
        let _ = std::fs::remove_file(key_path);
        dynamic_echo.await.unwrap();
        remote_fixed_echo.await.unwrap();
        remote_dynamic_echo.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn native_ssh_session_reconnects_after_server_disconnect() {
        let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_address = ssh_listener.local_addr().unwrap();
        let server_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let server_config = Arc::new(server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![server_key],
            ..Default::default()
        });
        let (server_handle_tx, mut server_handle_rx) = mpsc::unbounded_channel();
        let accepted_connections = Arc::new(AtomicUsize::new(0));
        let server_connection_count = Arc::clone(&accepted_connections);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (socket, _) = ssh_listener.accept().await.unwrap();
                server_connection_count.fetch_add(1, AtomicOrdering::Relaxed);
                let running = server::run_stream(
                    Arc::clone(&server_config),
                    socket,
                    TestSshServer::default(),
                )
                .await
                .unwrap();
                server_handle_tx.send(running.handle()).unwrap();
                tokio::spawn(async move {
                    let _ = running.await;
                });
            }
        });

        let client_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let key_path = env::temp_dir().join(format!(
            "stk-native-ssh-reconnect-test-{}",
            NEXT_TEST_KEY_ID.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let key_text = client_key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        std::fs::write(&key_path, key_text.as_bytes()).unwrap();

        let pool = SshPoolDialer::start(
            "reconnect-test",
            SshPoolConfig {
                policy: crate::config::LoadBalancePolicy::RoundRobin,
                keep_alive_secs: Some(5),
                min_sessions_per_host: 1,
                max_sessions_per_host: 2,
                session_rotation_enabled: false,
                session_rotation_interval_secs: 3_600,
                max_channels_per_session: 8,
                server_alive_count_max: Some(2),
                connect_timeout_secs: Some(2),
                restart_initial_millis: 20,
                restart_max_secs: 1,
                session_spawn_cooldown_millis: 20,
                session_drain_timeout_secs: 2,
                hosts: vec![SshHostConfig {
                    name: "local-test".to_string(),
                    host: Some(ssh_address.ip().to_string()),
                    ssh_config_host: None,
                    port: Some(ssh_address.port()),
                    username: Some("test".to_string()),
                    auth: Some(SshAuthConfig::PrivateKey {
                        path: key_path.to_string_lossy().into_owned(),
                        passphrase_env: None,
                    }),
                    ssh_config_path: None,
                    host_key_policy: Some(SshHostKeyPolicy::InsecureAcceptAny),
                    known_hosts_path: None,
                    remote_forwards: Vec::new(),
                }],
            },
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
        )
        .unwrap();

        timeout(Duration::from_secs(3), async {
            loop {
                if pool.nodes[0].state.read().await.status == HealthStatus::Healthy {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("initial native SSH session did not become healthy");
        let first_server_handle = timeout(Duration::from_secs(1), server_handle_rx.recv())
            .await
            .unwrap()
            .unwrap();
        first_server_handle
            .disconnect(
                Disconnect::ByApplication,
                "test disconnect".to_string(),
                "en".to_string(),
            )
            .await
            .unwrap();

        timeout(Duration::from_secs(3), async {
            loop {
                let state = pool.nodes[0].state.read().await.clone();
                if state.restart_count >= 1
                    && state.status == HealthStatus::Healthy
                    && accepted_connections.load(AtomicOrdering::Relaxed) >= 2
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("native SSH session did not reconnect");

        drop(pool);
        let _ = std::fs::remove_file(key_path);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn scheduled_rotation_replaces_one_healthy_session() {
        let (ssh_address, server) = start_test_ssh_server().await;
        let key_path = write_test_key("scheduled-rotation");
        let mut config = test_pool_config(ssh_address, &key_path, 1, 2, 8, Vec::new());
        config.session_rotation_enabled = true;
        config.session_rotation_interval_secs = 1;
        config.session_drain_timeout_secs = 1;
        config.hosts[0].name = "scheduled-rotation".to_string();

        let pool = SshPoolDialer::start(
            "scheduled-rotation",
            config,
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
        )
        .unwrap();
        let node = Arc::clone(&pool.nodes[0]);
        wait_for_healthy_sessions(&node, 1).await;
        let original = node.sessions.read().await[0].id;

        let replacement = timeout(Duration::from_secs(4), async {
            loop {
                let sessions = node.sessions.read().await.clone();
                if !sessions.iter().any(|session| session.id == original) {
                    let mut replacement = None;
                    for session in sessions {
                        if session.state.read().await.status == SshSessionStatus::Healthy
                            && !session.retire_requested.load(AtomicOrdering::Relaxed)
                        {
                            replacement = Some(session.id);
                            break;
                        }
                    }
                    if let Some(replacement) = replacement {
                        break replacement;
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scheduled SSH session rotation did not complete");

        assert_ne!(replacement, original);
        drop(pool);
        let _ = std::fs::remove_file(key_path);
        server.abort();
    }

    #[tokio::test]
    async fn unhealthy_session_is_replaced_without_reducing_target_capacity() {
        let (ssh_address, server) = start_test_ssh_server().await;
        let key_path = write_test_key("health-replacement");
        let pool = SshPoolDialer::start(
            "health-replacement",
            test_pool_config(ssh_address, &key_path, 2, 3, 8, Vec::new()),
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
        )
        .unwrap();
        let node = Arc::clone(&pool.nodes[0]);
        wait_for_healthy_sessions(&node, 2).await;
        let original = node.sessions.read().await[0].clone();

        mark_session_probe_failure(&original, "test health failure", true).await;

        timeout(Duration::from_secs(3), async {
            loop {
                let sessions = node.sessions.read().await.clone();
                let mut healthy = 0_usize;
                let mut original_present = false;
                for session in sessions {
                    original_present |= session.id == original.id;
                    if session.state.read().await.status == SshSessionStatus::Healthy
                        && !session.retire_requested.load(AtomicOrdering::Relaxed)
                    {
                        healthy += 1;
                    }
                }
                if !original_present && healthy == 2 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("unhealthy SSH session was not replaced at the target pool size");

        drop(pool);
        let _ = std::fs::remove_file(key_path);
        server.abort();
    }

    #[tokio::test]
    async fn saturated_retiring_pool_forces_one_session_out_before_replacement() {
        let (ssh_address, server) = start_test_ssh_server().await;
        let key_path = write_test_key("saturated-retiring");
        let pool = SshPoolDialer::start(
            "saturated-retiring",
            test_pool_config(ssh_address, &key_path, 2, 2, 8, Vec::new()),
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
        )
        .unwrap();
        let node = Arc::clone(&pool.nodes[0]);
        wait_for_healthy_sessions(&node, 2).await;
        let originals = node.sessions.read().await.clone();
        let original_ids = originals
            .iter()
            .map(|session| session.id)
            .collect::<HashSet<_>>();
        for session in &originals {
            session.in_flight.store(1, AtomicOrdering::Relaxed);
            mark_session_probe_failure(session, "test saturated retirement", true).await;
            sleep(Duration::from_millis(5)).await;
        }

        timeout(Duration::from_secs(4), async {
            loop {
                let sessions = node.sessions.read().await.clone();
                let original_count = sessions
                    .iter()
                    .filter(|session| original_ids.contains(&session.id))
                    .count();
                let healthy_replacements = sessions
                    .iter()
                    .filter(|session| !original_ids.contains(&session.id))
                    .filter(|session| !session.retire_requested.load(AtomicOrdering::Relaxed))
                    .filter(|session| {
                        session
                            .state
                            .try_read()
                            .is_ok_and(|state| state.status == SshSessionStatus::Healthy)
                    })
                    .count();
                if original_count == 1 && healthy_replacements == 1 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("saturated retiring pool did not force one session turnover");

        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            node.sessions
                .read()
                .await
                .iter()
                .filter(|session| original_ids.contains(&session.id))
                .count(),
            1,
            "forced turnover must wait instead of killing every draining session"
        );

        drop(pool);
        let _ = std::fs::remove_file(key_path);
        server.abort();
    }

    #[tokio::test]
    async fn scheduled_rotation_waits_for_a_healthy_replacement() {
        let candidate = Arc::new(ManagedSshSession {
            id: 1,
            state: Arc::new(RwLock::new(SshSessionState {
                status: SshSessionStatus::Healthy,
                ..SshSessionState::default()
            })),
            handle: Arc::new(RwLock::new(None)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            payload_generation: Arc::new(AtomicU64::new(0)),
            retire_requested: AtomicBool::new(false),
        });
        let replacement = Arc::new(ManagedSshSession {
            id: 2,
            state: Arc::new(RwLock::new(SshSessionState::default())),
            handle: Arc::new(RwLock::new(None)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            payload_generation: Arc::new(AtomicU64::new(0)),
            retire_requested: AtomicBool::new(false),
        });
        let node = Arc::new(NativeSshNode {
            name: "rotation-order".to_string(),
            state: Arc::new(RwLock::new(SshNodeState::default())),
            sessions: Arc::new(RwLock::new(vec![
                Arc::clone(&candidate),
                Arc::clone(&replacement),
            ])),
            remote_owner: Arc::new(RwLock::new(None)),
            connect_demand: Arc::new(Notify::new()),
            session_events: watch::channel(0).0,
            channel_open_timeout: Duration::from_secs(1),
            max_channels_per_session: 8,
        });
        let rotation = ScheduledSessionRotation {
            candidate_session_id: candidate.id,
            replacement_session_id: Some(replacement.id),
        };

        assert!(matches!(
            advance_scheduled_session_rotation(&node, &rotation).await,
            ScheduledRotationProgress::Waiting
        ));
        assert!(!candidate.retire_requested.load(Ordering::Relaxed));

        replacement.state.write().await.status = SshSessionStatus::Healthy;
        assert!(matches!(
            advance_scheduled_session_rotation(&node, &rotation).await,
            ScheduledRotationProgress::Activated
        ));
        assert!(candidate.retire_requested.load(Ordering::Relaxed));
        assert!(!replacement.retire_requested.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn health_replacements_wait_for_capacity_and_drain_serially() {
        let session = |id, status, retiring| {
            Arc::new(ManagedSshSession {
                id,
                state: Arc::new(RwLock::new(SshSessionState {
                    status,
                    ..SshSessionState::default()
                })),
                handle: Arc::new(RwLock::new(None)),
                in_flight: Arc::new(AtomicUsize::new(0)),
                payload_generation: Arc::new(AtomicU64::new(0)),
                retire_requested: AtomicBool::new(retiring),
            })
        };
        let first_unhealthy = session(1, SshSessionStatus::Suspect, true);
        let second_unhealthy = session(2, SshSessionStatus::Suspect, true);
        let first_replacement = session(3, SshSessionStatus::Healthy, false);
        let second_replacement = session(4, SshSessionStatus::Connecting, false);
        let node = Arc::new(NativeSshNode {
            name: "serialized-health-replacement".to_string(),
            state: Arc::new(RwLock::new(SshNodeState::default())),
            sessions: Arc::new(RwLock::new(vec![
                Arc::clone(&first_unhealthy),
                Arc::clone(&second_unhealthy),
                first_replacement,
                Arc::clone(&second_replacement),
            ])),
            remote_owner: Arc::new(RwLock::new(None)),
            connect_demand: Arc::new(Notify::new()),
            session_events: watch::channel(0).0,
            channel_open_timeout: Duration::from_secs(1),
            max_channels_per_session: 8,
        });

        drain_replaced_sessions(&node, 2, Duration::from_secs(10)).await;
        assert_eq!(
            [Arc::clone(&first_unhealthy), Arc::clone(&second_unhealthy)]
                .into_iter()
                .filter(|session| {
                    session
                        .state
                        .try_read()
                        .is_ok_and(|state| state.status == SshSessionStatus::Draining)
                })
                .count(),
            0
        );

        second_replacement.state.write().await.status = SshSessionStatus::Healthy;
        drain_replaced_sessions(&node, 2, Duration::from_secs(10)).await;
        assert_eq!(
            [first_unhealthy, second_unhealthy]
                .into_iter()
                .filter(|session| {
                    session
                        .state
                        .try_read()
                        .is_ok_and(|state| state.status == SshSessionStatus::Draining)
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn native_ssh_pool_uses_multiple_sessions_concurrently() {
        let (target, target_task) = start_persistent_echo_server(2).await;
        let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_address = ssh_listener.local_addr().unwrap();
        let server_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let server_config = Arc::new(server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![server_key],
            ..Default::default()
        });
        let server = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = ssh_listener.accept().await else {
                    return;
                };
                let running = server::run_stream(
                    Arc::clone(&server_config),
                    socket,
                    TestSshServer::default(),
                )
                .await
                .unwrap();
                tokio::spawn(async move {
                    let _ = running.await;
                });
            }
        });
        let key_path = write_test_key("multi-session");
        let pool = SshPoolDialer::start(
            "multi-session-test",
            test_pool_config(ssh_address, &key_path, 2, 3, 1, Vec::new()),
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
        )
        .unwrap();

        wait_for_healthy_sessions(&pool.nodes[0], 2).await;
        let mut first = pool
            .dial(DialContext {
                host_name: "multi-session-test".to_string(),
                target: TargetAddr::Socket(target),
                connection_id: None,
            })
            .await
            .unwrap();
        let mut second = pool
            .dial(DialContext {
                host_name: "multi-session-test".to_string(),
                target: TargetAddr::Socket(target),
                connection_id: None,
            })
            .await
            .unwrap();

        let sessions = pool.nodes[0].sessions.read().await.clone();
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.in_flight.load(AtomicOrdering::Relaxed) == 1)
                .count(),
            2
        );
        first.write_all(b"one1").await.unwrap();
        second.write_all(b"two2").await.unwrap();
        let mut first_reply = [0_u8; 4];
        let mut second_reply = [0_u8; 4];
        first.read_exact(&mut first_reply).await.unwrap();
        second.read_exact(&mut second_reply).await.unwrap();
        assert_eq!(&first_reply, b"one1");
        assert_eq!(&second_reply, b"two2");

        drop(first);
        drop(second);
        drop(pool);
        let _ = std::fs::remove_file(key_path);
        target_task.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn native_ssh_pool_scales_with_pressure_and_returns_to_minimum() {
        let (target, target_task) = start_persistent_echo_server(3).await;
        let (ssh_address, server) = start_test_ssh_server().await;
        let key_path = write_test_key("elastic-session-pool");
        let pool = SshPoolDialer::start(
            "elastic-session-pool",
            test_pool_config(ssh_address, &key_path, 1, 3, 1, Vec::new()),
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
        )
        .unwrap();
        let node = Arc::clone(&pool.nodes[0]);
        wait_for_healthy_sessions(&node, 1).await;

        let first = pool
            .dial(DialContext {
                host_name: "elastic-session-pool".to_string(),
                target: TargetAddr::Socket(target),
                connection_id: None,
            })
            .await
            .unwrap();
        wait_for_healthy_sessions(&node, 2).await;

        let second = pool
            .dial(DialContext {
                host_name: "elastic-session-pool".to_string(),
                target: TargetAddr::Socket(target),
                connection_id: None,
            })
            .await
            .unwrap();
        wait_for_healthy_sessions(&node, 3).await;

        let third = pool
            .dial(DialContext {
                host_name: "elastic-session-pool".to_string(),
                target: TargetAddr::Socket(target),
                connection_id: None,
            })
            .await
            .unwrap();

        let sessions = node.sessions.read().await.clone();
        assert_eq!(sessions.len(), 3);
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.in_flight.load(AtomicOrdering::Relaxed) == 1)
                .count(),
            3
        );
        sleep(Duration::from_millis(100)).await;
        assert!(node.sessions.read().await.len() <= 3);

        drop(first);
        drop(second);
        drop(third);
        timeout(Duration::from_secs(3), async {
            loop {
                let sessions = node.sessions.read().await.clone();
                if sessions.len() == 1
                    && sessions[0].state.read().await.status == SshSessionStatus::Healthy
                    && !sessions[0].retire_requested.load(AtomicOrdering::Relaxed)
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("elastic SSH session pool did not return to its minimum size");

        drop(pool);
        let _ = std::fs::remove_file(key_path);
        target_task.await.unwrap();
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_command_stream_is_bidirectional() {
        let endpoint = ResolvedSshEndpoint {
            alias: "proxy-command-test".to_string(),
            host: "example.test".to_string(),
            port: 22,
            username: "test".to_string(),
            auth: crate::ssh_config::ResolvedSshAuth {
                explicit: None,
                identity_files: Vec::new(),
                use_agent: false,
            },
            host_key_policy: ResolvedHostKeyPolicy::InsecureAcceptAny,
            host_key_name: "example.test".to_string(),
            known_hosts_paths: Vec::new(),
            connect_timeout: Duration::from_secs(1),
            keep_alive: Duration::from_secs(1),
            keep_alive_max: 1,
            tcp_keep_alive: true,
            proxy_command: Some("cat".to_string()),
        };
        let mut stream = ProxyCommandStream::spawn(&endpoint).unwrap();
        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();
        let mut reply = [0_u8; 4];
        timeout(Duration::from_secs(1), stream.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply, b"ping");
    }

    #[tokio::test]
    async fn native_ssh_pool_connects_multiple_sessions_through_two_proxy_jumps() {
        let (target, target_task) = start_echo_server().await;
        let (final_ssh, final_server) = start_test_ssh_server().await;
        let (jump_b, jump_b_server) = start_test_ssh_server().await;
        let (jump_a, jump_a_server) = start_test_ssh_server().await;
        let key_path = write_test_key("proxy-jump");
        let config_path = env::temp_dir().join(format!(
            "stk-proxy-jump-config-{}",
            NEXT_TEST_KEY_ID.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        std::fs::write(
            &config_path,
            format!(
                "Host target\n  HostName {}\n  Port {}\n  ProxyJump jump-a,jump-b\nHost jump-a\n  HostName {}\n  Port {}\nHost jump-b\n  HostName {}\n  Port {}\nHost *\n  User test\n  IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n",
                final_ssh.ip(),
                final_ssh.port(),
                jump_a.ip(),
                jump_a.port(),
                jump_b.ip(),
                jump_b.port(),
                key_path.display()
            ),
        )
        .unwrap();

        let mut config = test_pool_config(final_ssh, &key_path, 2, 2, 8, Vec::new());
        let upstream = &mut config.hosts[0];
        upstream.host = None;
        upstream.ssh_config_host = Some("target".to_string());
        upstream.ssh_config_path = Some(config_path.to_string_lossy().into_owned());
        upstream.port = None;
        upstream.username = None;
        upstream.auth = None;
        upstream.host_key_policy = None;

        let pool = SshPoolDialer::start(
            "proxy-jump-test",
            config,
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
        )
        .unwrap();
        wait_for_healthy_sessions(&pool.nodes[0], 2).await;

        let mut stream = pool
            .dial(DialContext {
                host_name: "proxy-jump-test".to_string(),
                target: TargetAddr::Socket(target),
                connection_id: None,
            })
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut reply = [0_u8; 4];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ping");

        drop(stream);
        drop(pool);
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_file(key_path);
        target_task.await.unwrap();
        final_server.abort();
        jump_b_server.abort();
        jump_a_server.abort();
    }

    #[tokio::test]
    async fn remote_forward_owner_handover_preserves_existing_channel() {
        let (local_target, target_task) = start_persistent_echo_server(2).await;
        let remote_listen = unused_local_address().await;
        let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_address = ssh_listener.local_addr().unwrap();
        let server_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let server_config = Arc::new(server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![server_key],
            ..Default::default()
        });
        let server = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = ssh_listener.accept().await else {
                    return;
                };
                let running = server::run_stream(
                    Arc::clone(&server_config),
                    socket,
                    TestSshServer::default(),
                )
                .await
                .unwrap();
                tokio::spawn(async move {
                    let _ = running.await;
                });
            }
        });
        let key_path = write_test_key("remote-owner");
        let remote_forward = SshRemoteForwardConfig::Tcp {
            name: "handover".to_string(),
            listen: remote_listen,
            local_host: local_target.ip().to_string(),
            local_port: local_target.port(),
        };
        let pool = SshPoolDialer::start(
            "remote-owner-test",
            test_pool_config(ssh_address, &key_path, 2, 3, 8, vec![remote_forward]),
            ProbeConfig {
                enabled: false,
                ..ProbeConfig::default()
            },
        )
        .unwrap();
        let node = Arc::clone(&pool.nodes[0]);
        wait_for_healthy_sessions(&node, 2).await;
        let original_owner = timeout(Duration::from_secs(3), async {
            loop {
                if let Some(owner) = *node.remote_owner.read().await {
                    break owner;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("remote forward owner was not assigned");

        let mut existing = TcpStream::connect(remote_listen).await.unwrap();
        existing.write_all(b"old1").await.unwrap();
        let mut reply = [0_u8; 4];
        existing.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"old1");

        let owner_session = node
            .sessions
            .read()
            .await
            .iter()
            .find(|session| session.id == original_owner)
            .cloned()
            .unwrap();
        owner_session
            .retire_requested
            .store(true, AtomicOrdering::Release);
        owner_session.state.write().await.status = SshSessionStatus::Suspect;

        let replacement_owner = timeout(Duration::from_secs(3), async {
            loop {
                if let Some(owner) = *node.remote_owner.read().await
                    && owner != original_owner
                {
                    break owner;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("remote forward ownership did not move to a standby session");
        assert_ne!(replacement_owner, original_owner);

        existing.write_all(b"old2").await.unwrap();
        existing.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"old2");

        let mut new_connection = timeout(Duration::from_secs(3), async {
            loop {
                match TcpStream::connect(remote_listen).await {
                    Ok(stream) => break stream,
                    Err(_) => sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("replacement remote listener did not become available");
        new_connection.write_all(b"new1").await.unwrap();
        new_connection.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"new1");

        drop(existing);
        drop(new_connection);
        timeout(Duration::from_secs(3), async {
            loop {
                if !node
                    .sessions
                    .read()
                    .await
                    .iter()
                    .any(|session| session.id == original_owner)
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("drained remote forward owner was not retired");

        drop(pool);
        let _ = std::fs::remove_file(key_path);
        target_task.await.unwrap();
        server.abort();
    }

    fn write_test_key(prefix: &str) -> PathBuf {
        let client_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let key_path = env::temp_dir().join(format!(
            "stk-{prefix}-{}",
            NEXT_TEST_KEY_ID.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let key_text = client_key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        std::fs::write(&key_path, key_text.as_bytes()).unwrap();
        key_path
    }

    fn test_pool_config(
        ssh_address: SocketAddr,
        key_path: &Path,
        min_sessions: usize,
        max_sessions: usize,
        max_channels: usize,
        remote_forwards: Vec<SshRemoteForwardConfig>,
    ) -> SshPoolConfig {
        SshPoolConfig {
            policy: crate::config::LoadBalancePolicy::RoundRobin,
            keep_alive_secs: Some(5),
            min_sessions_per_host: min_sessions,
            max_sessions_per_host: max_sessions,
            session_rotation_enabled: false,
            session_rotation_interval_secs: 3_600,
            max_channels_per_session: max_channels,
            server_alive_count_max: Some(2),
            connect_timeout_secs: Some(1),
            restart_initial_millis: 20,
            restart_max_secs: 1,
            session_spawn_cooldown_millis: 20,
            session_drain_timeout_secs: 2,
            hosts: vec![SshHostConfig {
                name: "local-test".to_string(),
                host: Some(ssh_address.ip().to_string()),
                ssh_config_host: None,
                port: Some(ssh_address.port()),
                username: Some("test".to_string()),
                auth: Some(SshAuthConfig::PrivateKey {
                    path: key_path.to_string_lossy().into_owned(),
                    passphrase_env: None,
                }),
                ssh_config_path: None,
                host_key_policy: Some(SshHostKeyPolicy::InsecureAcceptAny),
                known_hosts_path: None,
                remote_forwards,
            }],
        }
    }

    async fn wait_for_healthy_sessions(node: &Arc<NativeSshNode>, expected: usize) {
        timeout(Duration::from_secs(3), async {
            loop {
                let sessions = node.sessions.read().await.clone();
                let mut healthy = 0;
                for session in sessions {
                    if session.state.read().await.status == SshSessionStatus::Healthy
                        && !session.retire_requested.load(AtomicOrdering::Relaxed)
                    {
                        healthy += 1;
                    }
                }
                if healthy >= expected {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SSH session pool did not reach the expected healthy size");
    }

    async fn start_persistent_echo_server(
        expected_connections: usize,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            for _ in 0..expected_connections {
                let (mut stream, _) = listener.accept().await.unwrap();
                connections.spawn(async move {
                    let mut buffer = [0_u8; 64];
                    loop {
                        let read = stream.read(&mut buffer).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        stream.write_all(&buffer[..read]).await.unwrap();
                    }
                });
            }
            while let Some(result) = connections.join_next().await {
                result.unwrap();
            }
        });
        (address, task)
    }

    async fn start_echo_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4];
            stream.read_exact(&mut buffer).await.unwrap();
            stream.write_all(&buffer).await.unwrap();
        });
        (address, task)
    }

    async fn start_test_ssh_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        (address, start_test_ssh_server_on(listener))
    }

    fn start_test_ssh_server_on(listener: TcpListener) -> tokio::task::JoinHandle<()> {
        let server_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let server_config = Arc::new(server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![server_key],
            ..Default::default()
        });
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let running = server::run_stream(
                    Arc::clone(&server_config),
                    socket,
                    TestSshServer::default(),
                )
                .await
                .unwrap();
                tokio::spawn(async move {
                    let _ = running.await;
                });
            }
        })
    }

    async fn unused_local_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    }

    #[derive(Clone, Default)]
    struct TestSshServer {
        remote_listeners: Arc<tokio::sync::Mutex<HashMap<(String, u32), tokio::task::AbortHandle>>>,
    }

    impl server::Handler for TestSshServer {
        type Error = anyhow::Error;

        async fn auth_publickey(
            &mut self,
            _user: &str,
            _public_key: &russh::keys::ssh_key::PublicKey,
        ) -> Result<server::Auth, Self::Error> {
            Ok(server::Auth::Accept)
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: Channel<server::Msg>,
            host_to_connect: &str,
            port_to_connect: u32,
            _originator_address: &str,
            _originator_port: u32,
            reply: server::ChannelOpenHandle,
            _session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            let host = host_to_connect.to_string();
            tokio::spawn(async move {
                let Ok(port) = u16::try_from(port_to_connect) else {
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                    return;
                };
                match TcpStream::connect((host.as_str(), port)).await {
                    Ok(mut target) => {
                        reply.accept().await;
                        let mut stream = channel.into_stream();
                        let _ = copy_bidirectional(&mut stream, &mut target).await;
                    }
                    Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
                }
            });
            Ok(())
        }

        async fn tcpip_forward(
            &mut self,
            address: &str,
            port: &mut u32,
            session: &mut server::Session,
        ) -> Result<bool, Self::Error> {
            let Ok(requested_port) = u16::try_from(*port) else {
                return Ok(false);
            };
            let listener = match TcpListener::bind((address, requested_port)).await {
                Ok(listener) => listener,
                Err(_) => return Ok(false),
            };
            let listen = listener.local_addr()?;
            *port = u32::from(listen.port());
            let listen_port = u32::from(listen.port());
            let handle = session.handle();
            let connected_address = address.to_string();
            let listener_task = tokio::spawn(async move {
                loop {
                    let Ok((mut remote_stream, originator)) = listener.accept().await else {
                        return;
                    };
                    let connection_handle = handle.clone();
                    let connection_address = connected_address.clone();
                    tokio::spawn(async move {
                        let Ok(channel) = connection_handle
                            .channel_open_forwarded_tcpip(
                                connection_address,
                                listen_port,
                                originator.ip().to_string(),
                                u32::from(originator.port()),
                            )
                            .await
                        else {
                            return;
                        };
                        let mut ssh_stream = channel.into_stream();
                        let _ = copy_bidirectional(&mut remote_stream, &mut ssh_stream).await;
                    });
                }
            });
            self.remote_listeners.lock().await.insert(
                (address.to_string(), listen_port),
                listener_task.abort_handle(),
            );
            Ok(true)
        }

        async fn cancel_tcpip_forward(
            &mut self,
            address: &str,
            port: u32,
            _session: &mut server::Session,
        ) -> Result<bool, Self::Error> {
            let Some(listener) = self
                .remote_listeners
                .lock()
                .await
                .remove(&(address.to_string(), port))
            else {
                return Ok(false);
            };
            listener.abort();
            Ok(true)
        }
    }

    fn sample_upstream() -> SshHostConfig {
        SshHostConfig {
            name: "jump-a".to_string(),
            host: Some("ssh.example.com".to_string()),
            ssh_config_host: None,
            port: Some(22),
            username: Some("alice".to_string()),
            auth: Some(SshAuthConfig::Agent),
            ssh_config_path: None,
            host_key_policy: Some(SshHostKeyPolicy::KnownHosts),
            known_hosts_path: None,
            remote_forwards: vec![
                SshRemoteForwardConfig::Tcp {
                    name: "local-web".to_string(),
                    listen: "127.0.0.1:18080".parse().unwrap(),
                    local_host: "127.0.0.1".to_string(),
                    local_port: 8080,
                },
                SshRemoteForwardConfig::Dynamic {
                    name: "remote-socks".to_string(),
                    listen: "127.0.0.1:1080".parse().unwrap(),
                    protocol: crate::config::ProxyProtocol::Socks5h,
                },
            ],
        }
    }
}
