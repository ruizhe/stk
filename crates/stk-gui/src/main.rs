#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use clap::{Parser, error::ErrorKind};
use dioxus::desktop::trayicon::{
    DioxusTray, Icon as TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuItem, PredefinedMenuItem},
};
use dioxus::desktop::{Config, LogicalSize, WindowBuilder, WindowCloseBehaviour};
#[cfg(target_os = "linux")]
use std::env;
use std::{
    any::Any,
    cell::RefCell,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Instant,
};
use stk_core::{
    AppConfig, ConfigScope, ControlConfig, ControlEndpoint, RuntimeProfile,
    default_config_directory, fetch_runtime_snapshot, fetch_traffic_history,
    reload::{
        ReloadControl, ReloadHandle, run_config_file_until_shutdown,
        run_config_file_with_control_until_shutdown,
    },
    request_clear_captured_connections, request_connection_capture_auto_clear_closed,
    request_connection_capture_recording, request_runtime_reload, resolve_config_path,
    stats::{
        RuntimeSnapshot, TrafficHistorySnapshot, clear_captured_connections, runtime_snapshot,
        set_connection_capture_auto_clear_closed, set_connection_capture_recording,
        traffic_history_snapshot,
    },
};
use tokio::sync::oneshot;
#[cfg(target_os = "macos")]
use tracing::debug;
use tracing::{error, info, warn};

mod app;
mod autostart;
mod gui_config;
mod logging;

use app::App;
use gui_config::{GuiConfig, Language};

const TRAY_SHOW_ID: &str = "stk-show";
const TRAY_RELOAD_ID: &str = "stk-reload";
const STATUS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(
    name = "stk-gui",
    version,
    about = "Reliable SSH proxies and tunnel management"
)]
struct GuiArgs {
    #[arg(
        short,
        long,
        help = "Config file or directory; defaults to ~/.config/stk"
    )]
    config: Option<PathBuf>,

    #[arg(long, hide = true)]
    hidden: bool,
}

struct GuiContext {
    runtime: Arc<GuiRuntimeManager>,
    gui_config_path: PathBuf,
    gui_config: GuiConfig,
}

#[derive(Clone)]
struct SystemTray {
    tray: DioxusTray,
    show_item: MenuItem,
    download_item: MenuItem,
    upload_item: MenuItem,
    reload_item: MenuItem,
    quit_item: PredefinedMenuItem,
    display_state: Rc<RefCell<SystemTrayDisplayState>>,
}

#[derive(Debug, Default)]
struct SystemTrayDisplayState {
    show_label: Option<String>,
    download_label: Option<String>,
    upload_label: Option<String>,
    reload_label: Option<String>,
    quit_label: Option<String>,
    title: Option<String>,
    tooltip: Option<String>,
}

struct GuiRuntimeManager {
    config_path: PathBuf,
    errors: Arc<Mutex<GuiErrors>>,
    reload_in_progress: AtomicBool,
    state: Mutex<GuiRuntimeState>,
    status_monitor: Mutex<Option<GuiStatusMonitor>>,
    #[cfg(target_os = "macos")]
    system_tray: Mutex<Option<MacosSystemTray>>,
}

struct GuiRuntimeState {
    runtime: Option<GuiRuntime>,
    reload_handle: Option<ReloadHandle>,
    attached_endpoint: Option<ControlEndpoint>,
    last_snapshot: Option<RuntimeSnapshot>,
}

struct GuiStatusMonitor {
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct MacosSystemTray {
    tray: Arc<dispatch2::MainThreadBound<SystemTray>>,
    updates: Arc<Mutex<MacosTrayUpdateQueue>>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct MacosTrayUpdateQueue {
    pending: Option<(u64, u64)>,
    scheduled: bool,
}

#[cfg(target_os = "macos")]
impl MacosTrayUpdateQueue {
    fn request(&mut self, rates: (u64, u64)) -> bool {
        self.pending = Some(rates);
        if self.scheduled {
            false
        } else {
            self.scheduled = true;
            true
        }
    }

    fn take(&mut self) -> Option<(u64, u64)> {
        self.scheduled = false;
        self.pending.take()
    }
}

#[derive(Debug, Default)]
struct GuiErrors {
    runtime: Option<String>,
    status: Option<String>,
    action: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum GuiErrorKind {
    Runtime,
    Status,
    Action,
}

impl GuiErrors {
    fn get_mut(&mut self, kind: GuiErrorKind) -> &mut Option<String> {
        match kind {
            GuiErrorKind::Runtime => &mut self.runtime,
            GuiErrorKind::Status => &mut self.status,
            GuiErrorKind::Action => &mut self.action,
        }
    }

    fn message(&self) -> Option<String> {
        let mut messages = Vec::new();
        for message in [
            self.action.as_deref(),
            self.runtime.as_deref(),
            self.status.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !messages.contains(&message) {
                messages.push(message);
            }
        }
        (!messages.is_empty()).then(|| messages.join("\n"))
    }
}

static GUI_CONTEXT: OnceLock<GuiContext> = OnceLock::new();

fn main() {
    let gui_config_path = gui_config::gui_config_path();
    let log_path = default_config_directory(ConfigScope::User).join("stk.log");
    logging::init(log_path.clone());
    let gui_config_exists = gui_config_path.exists();
    let gui_config = match GuiConfig::load(&gui_config_path) {
        Ok(config) => config,
        Err(error) => {
            warn!(
                config = %gui_config_path.display(),
                %error,
                "failed to load GUI configuration; using defaults"
            );
            GuiConfig::default()
        }
    };
    if !gui_config_exists && let Err(error) = gui_config.save(&gui_config_path) {
        warn!(
            config = %gui_config_path.display(),
            %error,
            "failed to create default GUI configuration"
        );
    }
    let (args, argument_error) = match GuiArgs::try_parse() {
        Ok(args) => (args, None),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return;
        }
        Err(error) => {
            let message = error.to_string();
            eprintln!("{message}");
            (
                GuiArgs {
                    config: None,
                    hidden: false,
                },
                Some(message),
            )
        }
    };
    let config_path = resolve_config_path(args.config.as_deref(), ConfigScope::User);
    info!(
        config = %config_path.display(),
        gui_config = %gui_config_path.display(),
        log = %log_path.display(),
        "SSH Tunnel Keeper desktop starting"
    );

    if !gui_available() {
        if let Err(error) = run_headless(config_path) {
            error!(error = %format_args!("{error:#}"), "SSH Tunnel Keeper headless runtime failed");
            eprintln!("SSH Tunnel Keeper runtime failed: {error:#}");
        }
        return;
    }

    let runtime = Arc::new(GuiRuntimeManager::new(config_path));
    if GUI_CONTEXT
        .set(GuiContext {
            runtime: Arc::clone(&runtime),
            gui_config_path,
            gui_config,
        })
        .is_err()
    {
        error!("GUI context was initialized more than once");
        return;
    }
    if let Err(error) = runtime.start() {
        runtime.set_runtime_error(format!("failed to start GUI runtime thread: {error}"));
    }
    if let Err(error) = runtime.start_status_monitor() {
        runtime.set_status_error(format!("failed to start GUI status monitor: {error}"));
    }
    if let Some(error) = argument_error {
        runtime.set_error(format!("invalid GUI arguments; using defaults:\n{error}"));
    }
    launch_desktop(desktop_config(args.hidden));
    runtime.stop();
}

fn launch_desktop(config: Config) {
    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .launch(App);
}

fn desktop_config(start_hidden: bool) -> Config {
    let window = WindowBuilder::new()
        .with_title("SSH Tunnel Keeper")
        .with_inner_size(LogicalSize::new(974.0, 790.0))
        .with_min_inner_size(LogicalSize::new(620.0, 500.0))
        .with_visible(!start_hidden)
        .with_always_on_top(false);
    let config = Config::new().with_window(window);
    let config = match create_window_icon() {
        Ok(icon) => config.with_icon(icon),
        Err(error) => {
            error!(error = %format_args!("{error:#}"), "failed to load the GUI window icon");
            config
        }
    };
    let config = config
        .with_close_behaviour(WindowCloseBehaviour::WindowHides)
        .with_exits_when_last_window_closes(false)
        .with_tray_icon_show_window_on_click(true);

    #[cfg(target_os = "macos")]
    let config = {
        use dioxus::desktop::tao::event::{Event, StartCause, WindowEvent};
        use dioxus::desktop::tao::platform::macos::{
            ActivationPolicy, EventLoopWindowTargetExtMacOS,
        };

        let mut dock_visible = None;
        config.with_custom_event_handler(move |event, event_loop| {
            let requested_visibility = match event {
                Event::NewEvents(StartCause::Init) => Some(!start_hidden),
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => Some(false),
                Event::WindowEvent {
                    event: WindowEvent::Focused(true),
                    ..
                } => Some(true),
                _ => None,
            };
            if let Some(visible) = requested_visibility
                && dock_visible != Some(visible)
            {
                let policy = if visible {
                    ActivationPolicy::Regular
                } else {
                    ActivationPolicy::Accessory
                };
                event_loop.set_activation_policy_at_runtime(policy);
                event_loop.set_dock_visibility(visible);
                dock_visible = Some(visible);
            }
        })
    };

    config
}

#[cfg(target_os = "linux")]
fn gui_available() -> bool {
    env::var_os("DISPLAY").is_some() || env::var_os("WAYLAND_DISPLAY").is_some()
}

#[cfg(not(target_os = "linux"))]
fn gui_available() -> bool {
    true
}

fn run_headless(config_path: PathBuf) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_config_file_until_shutdown(
        config_path,
        RuntimeProfile::Foreground,
        shutdown_signal(),
    ))
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for shutdown signal: {error}");
    }
}

struct GuiRuntime {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl GuiRuntimeManager {
    fn new(config_path: PathBuf) -> Self {
        Self {
            config_path,
            errors: Arc::new(Mutex::new(GuiErrors::default())),
            reload_in_progress: AtomicBool::new(false),
            state: Mutex::new(GuiRuntimeState {
                runtime: None,
                reload_handle: None,
                attached_endpoint: None,
                last_snapshot: None,
            }),
            status_monitor: Mutex::new(None),
            #[cfg(target_os = "macos")]
            system_tray: Mutex::new(None),
        }
    }

    fn start_status_monitor(self: &Arc<Self>) -> io::Result<()> {
        let mut monitor = lock_or_recover(&self.status_monitor);
        if monitor.is_some() {
            return Ok(());
        }
        *monitor = Some(GuiStatusMonitor::start(Arc::downgrade(self))?);
        Ok(())
    }

    fn start(&self) -> io::Result<()> {
        let previous_runtime = {
            let mut state = lock_or_recover(&self.state);
            state.reload_handle = None;
            state.attached_endpoint = None;
            state.last_snapshot = None;
            state.runtime.take()
        };
        drop(previous_runtime);
        self.clear_all_errors();

        let endpoint = self.configured_endpoint().map_err(io::Error::other)?;
        if let Some(snapshot) = probe_runtime(&endpoint) {
            info!(%endpoint, "GUI attached to an existing runtime");
            let mut state = lock_or_recover(&self.state);
            state.attached_endpoint = Some(endpoint);
            state.last_snapshot = Some(snapshot);
            return Ok(());
        }

        let reload_control = ReloadControl::new();
        let reload_handle = reload_control.handle();
        let runtime = GuiRuntime::start(
            self.config_path.clone(),
            Arc::clone(&self.errors),
            reload_control,
        )?;
        let mut state = lock_or_recover(&self.state);
        state.reload_handle = Some(reload_handle);
        state.runtime = Some(runtime);
        Ok(())
    }

    fn reload_or_restart(self: &Arc<Self>) -> io::Result<()> {
        if self
            .reload_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        let manager = Arc::clone(self);
        let spawn = thread::Builder::new()
            .name("stk-gui-reload".to_string())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| manager.reload_or_restart_inner()));
                match outcome {
                    Ok(Ok(())) => manager.clear_error(),
                    Ok(Err(error)) => {
                        manager.set_error(format!("failed to reload runtime: {error:#}"));
                    }
                    Err(payload) => manager.set_error(format!(
                        "runtime reload panicked: {}",
                        panic_payload_message(payload.as_ref())
                    )),
                }
                manager.reload_in_progress.store(false, Ordering::Release);
            });
        if let Err(error) = spawn {
            self.reload_in_progress.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    fn reload_or_restart_inner(&self) -> anyhow::Result<()> {
        let attached_endpoint = lock_or_recover(&self.state).attached_endpoint.clone();
        if let Some(endpoint) = attached_endpoint {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    request_runtime_reload(&endpoint),
                )
                .await
                .map_err(|_| {
                    anyhow::anyhow!("timed out requesting runtime reload at {endpoint}")
                })??;
                Ok(())
            })
        } else {
            let reload_requested = {
                let state = lock_or_recover(&self.state);
                let running = state
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| !runtime.is_finished());
                running
                    && state
                        .reload_handle
                        .as_ref()
                        .is_some_and(ReloadHandle::request_reload)
            };
            if reload_requested {
                Ok(())
            } else {
                self.start().map_err(anyhow::Error::from)
            }
        }
    }

    async fn status_snapshot(&self) -> anyhow::Result<RuntimeSnapshot> {
        let attached_endpoint = lock_or_recover(&self.state).attached_endpoint.clone();
        let Some(endpoint) = attached_endpoint else {
            self.clear_error_kind(GuiErrorKind::Status);
            return Ok(runtime_snapshot());
        };
        let configured_endpoint = self.configured_endpoint().map_err(|error| {
            let message = format!("failed to resolve runtime control endpoint: {error:#}");
            self.set_status_error(message.clone());
            anyhow::anyhow!(message)
        })?;
        let primary = fetch_status_endpoint(&endpoint).await;
        match primary {
            Ok(snapshot) => {
                self.clear_error_kind(GuiErrorKind::Status);
                Ok(snapshot)
            }
            Err(primary_error) if configured_endpoint != endpoint => {
                if let Ok(snapshot) = fetch_status_endpoint(&configured_endpoint).await {
                    lock_or_recover(&self.state).attached_endpoint = Some(configured_endpoint);
                    self.clear_error_kind(GuiErrorKind::Status);
                    return Ok(snapshot);
                }
                let message =
                    format!("failed to fetch runtime status at {endpoint}: {primary_error:#}");
                self.set_status_error(message.clone());
                Err(anyhow::anyhow!(message))
            }
            Err(error) => {
                let message = format!("failed to fetch runtime status at {endpoint}: {error:#}");
                self.set_status_error(message.clone());
                Err(anyhow::anyhow!(message))
            }
        }
    }

    fn accept_snapshot(&self, snapshot: RuntimeSnapshot) {
        lock_or_recover(&self.state).last_snapshot = Some(snapshot);
    }

    fn cached_snapshot(&self) -> RuntimeSnapshot {
        lock_or_recover(&self.state)
            .last_snapshot
            .clone()
            .unwrap_or_else(runtime_snapshot)
    }

    fn register_system_tray(&self, tray: SystemTray) {
        #[cfg(target_os = "macos")]
        {
            let Some(main_thread) = objc2_foundation::MainThreadMarker::new() else {
                self.set_status_error(
                    "failed to register the macOS tray outside the main thread".to_string(),
                );
                return;
            };
            let updater = MacosSystemTray {
                tray: Arc::new(dispatch2::MainThreadBound::new(tray, main_thread)),
                updates: Arc::new(Mutex::new(MacosTrayUpdateQueue::default())),
            };
            let snapshot = self.cached_snapshot();
            updater.update_throughput(snapshot.upload_bps, snapshot.download_bps);
            *lock_or_recover(&self.system_tray) = Some(updater);
        }

        #[cfg(not(target_os = "macos"))]
        let _ = tray;
    }

    fn publish_system_tray_throughput(&self, snapshot: &RuntimeSnapshot) {
        #[cfg(target_os = "macos")]
        if let Some(tray) = lock_or_recover(&self.system_tray).clone() {
            tray.update_throughput(snapshot.upload_bps, snapshot.download_bps);
        }

        #[cfg(not(target_os = "macos"))]
        let _ = snapshot;
    }

    async fn traffic_history(&self) -> Option<TrafficHistorySnapshot> {
        let attached_endpoint = lock_or_recover(&self.state).attached_endpoint.clone();
        let Some(endpoint) = attached_endpoint else {
            return Some(traffic_history_snapshot());
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            fetch_traffic_history(&endpoint),
        )
        .await
        .ok()
        .and_then(Result::ok)
    }

    async fn set_connection_capture_recording(&self, recording: bool) -> anyhow::Result<()> {
        let attached_endpoint = lock_or_recover(&self.state).attached_endpoint.clone();
        let Some(endpoint) = attached_endpoint else {
            set_connection_capture_recording(recording);
            self.clear_error();
            return Ok(());
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            request_connection_capture_recording(&endpoint, recording),
        )
        .await
        .map_err(|_| anyhow::anyhow!("connection capture request timed out"))??;
        self.clear_error();
        Ok(())
    }

    async fn clear_captured_connections(&self) -> anyhow::Result<()> {
        let attached_endpoint = lock_or_recover(&self.state).attached_endpoint.clone();
        let Some(endpoint) = attached_endpoint else {
            clear_captured_connections();
            self.clear_error();
            return Ok(());
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            request_clear_captured_connections(&endpoint),
        )
        .await
        .map_err(|_| anyhow::anyhow!("clear captured connections request timed out"))??;
        self.clear_error();
        Ok(())
    }

    async fn set_connection_capture_auto_clear_closed(&self, enabled: bool) -> anyhow::Result<()> {
        let attached_endpoint = lock_or_recover(&self.state).attached_endpoint.clone();
        let Some(endpoint) = attached_endpoint else {
            set_connection_capture_auto_clear_closed(enabled);
            self.clear_error();
            return Ok(());
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            request_connection_capture_auto_clear_closed(&endpoint, enabled),
        )
        .await
        .map_err(|_| anyhow::anyhow!("connection auto-clear request timed out"))??;
        self.clear_error();
        Ok(())
    }

    fn initial_snapshot(&self) -> RuntimeSnapshot {
        self.cached_snapshot()
    }

    fn initial_traffic_history(&self) -> TrafficHistorySnapshot {
        traffic_history_snapshot()
    }

    fn configured_endpoint(&self) -> anyhow::Result<ControlEndpoint> {
        let control = if self.config_path.is_file() {
            AppConfig::from_path(&self.config_path)?.control
        } else {
            ControlConfig::default()
        };
        ControlEndpoint::from_config(&control, ConfigScope::User)
    }

    #[cfg(test)]
    fn is_attached(&self) -> bool {
        lock_or_recover(&self.state).attached_endpoint.is_some()
    }

    fn stop(&self) {
        let monitor = lock_or_recover(&self.status_monitor).take();
        drop(monitor);
        #[cfg(target_os = "macos")]
        {
            lock_or_recover(&self.system_tray).take();
        }
        let mut state = lock_or_recover(&self.state);
        state.reload_handle = None;
        state.runtime = None;
        state.attached_endpoint = None;
        state.last_snapshot = None;
    }

    fn error(&self) -> Option<String> {
        lock_or_recover(&self.errors).message()
    }

    fn set_error(&self, error: String) {
        set_shared_error(&self.errors, GuiErrorKind::Action, error);
    }

    fn set_runtime_error(&self, error: String) {
        set_shared_error(&self.errors, GuiErrorKind::Runtime, error);
    }

    fn set_status_error(&self, error: String) {
        set_shared_error(&self.errors, GuiErrorKind::Status, error);
    }

    fn clear_error(&self) {
        self.clear_error_kind(GuiErrorKind::Action);
    }

    fn clear_error_kind(&self, kind: GuiErrorKind) {
        clear_shared_error(&self.errors, kind);
    }

    fn clear_all_errors(&self) {
        *lock_or_recover(&self.errors) = GuiErrors::default();
    }
}

fn probe_runtime(endpoint: &ControlEndpoint) -> Option<RuntimeSnapshot> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                fetch_runtime_snapshot(endpoint),
            )
            .await
        })
        .ok()?
        .ok()
}

async fn fetch_status_endpoint(endpoint: &ControlEndpoint) -> anyhow::Result<RuntimeSnapshot> {
    tokio::time::timeout(STATUS_POLL_INTERVAL, fetch_runtime_snapshot(endpoint))
        .await
        .map_err(|_| anyhow::anyhow!("status request timed out"))?
}

impl GuiStatusMonitor {
    fn start(manager: std::sync::Weak<GuiRuntimeManager>) -> io::Result<Self> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("stk-gui-status".to_string())
            .spawn(move || run_gui_status_monitor(manager, shutdown_rx))?;
        Ok(Self {
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
    }
}

impl Drop for GuiStatusMonitor {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            error!("GUI status monitor panicked during shutdown");
        }
    }
}

fn run_gui_status_monitor(
    manager: std::sync::Weak<GuiRuntimeManager>,
    shutdown: mpsc::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            if let Some(manager) = manager.upgrade() {
                manager.set_status_error(format!("failed to create GUI status runtime: {error}"));
            }
            return;
        }
    };

    loop {
        let cycle_started = Instant::now();
        let Some(manager) = manager.upgrade() else {
            break;
        };
        if let Ok(snapshot) = runtime.block_on(manager.status_snapshot()) {
            manager.accept_snapshot(snapshot.clone());
            manager.publish_system_tray_throughput(&snapshot);
        }
        drop(manager);

        let wait = STATUS_POLL_INTERVAL.saturating_sub(cycle_started.elapsed());
        match shutdown.recv_timeout(wait) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(target_os = "macos")]
impl MacosSystemTray {
    fn update_throughput(&self, upload_bps: u64, download_bps: u64) {
        let should_schedule = lock_or_recover(&self.updates).request((upload_bps, download_bps));
        if !should_schedule {
            return;
        }

        let tray = Arc::clone(&self.tray);
        let updates = Arc::clone(&self.updates);
        dispatch2::DispatchQueue::main().exec_async(move || {
            let Some((upload_bps, download_bps)) = lock_or_recover(&updates).take() else {
                return;
            };
            let Some(main_thread) = objc2_foundation::MainThreadMarker::new() else {
                error!("macOS tray throughput update ran outside the main thread");
                return;
            };
            app::update_system_tray_throughput(
                tray.get(main_thread),
                upload_bps as f64,
                download_bps as f64,
            );
            debug!(upload_bps, download_bps, "native tray throughput refreshed");
        });
    }
}

impl GuiRuntime {
    fn start(
        config_path: PathBuf,
        errors: Arc<Mutex<GuiErrors>>,
        reload_control: ReloadControl,
    ) -> io::Result<Self> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("stk-gui-runtime".to_string())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run_gui_runtime(config_path, reload_control, shutdown_rx)
                }));
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => set_shared_error(
                        &errors,
                        GuiErrorKind::Runtime,
                        format!("GUI proxy runtime stopped: {error:#}"),
                    ),
                    Err(payload) => set_shared_error(
                        &errors,
                        GuiErrorKind::Runtime,
                        format!(
                            "GUI proxy runtime panicked: {}",
                            panic_payload_message(payload.as_ref())
                        ),
                    ),
                }
            })?;
        Ok(Self {
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
    }

    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

fn run_gui_runtime(
    config_path: PathBuf,
    reload_control: ReloadControl,
    shutdown_rx: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)?;
    runtime.block_on(run_config_file_with_control_until_shutdown(
        config_path,
        RuntimeProfile::Foreground,
        reload_control,
        async move {
            let _ = shutdown_rx.await;
        },
    ))
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn set_shared_error(errors: &Mutex<GuiErrors>, kind: GuiErrorKind, message: String) {
    let changed = {
        let mut errors = lock_or_recover(errors);
        let current = errors.get_mut(kind);
        if current.as_deref() == Some(message.as_str()) {
            false
        } else {
            *current = Some(message.clone());
            true
        }
    };
    if changed {
        error!(error_kind = ?kind, error = %message, "GUI operation failed");
    }
}

fn clear_shared_error(errors: &Mutex<GuiErrors>, kind: GuiErrorKind) {
    *lock_or_recover(errors).get_mut(kind) = None;
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_string())
        })
        .unwrap_or_else(|| "unknown panic payload".to_string())
}

impl Drop for GuiRuntime {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn init_system_tray(language: Language) -> anyhow::Result<SystemTray> {
    let menu = Menu::new();
    let (show_label, download_label, upload_label, reload_label, quit_label) =
        tray_labels(language);
    let show_item = MenuItem::with_id(TRAY_SHOW_ID, show_label, true, None);
    let download_item = MenuItem::new(download_label, false, None);
    let upload_item = MenuItem::new(upload_label, false, None);
    let reload_item = MenuItem::with_id(TRAY_RELOAD_ID, reload_label, true, None);
    let quit_item = PredefinedMenuItem::quit(Some(quit_label));
    menu.append_items(&[
        &show_item,
        &PredefinedMenuItem::separator(),
        &upload_item,
        &download_item,
        &reload_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(create_tray_icon()?)
        .build()?;
    #[cfg(target_os = "macos")]
    {
        tray.set_icon_as_template(true);
        configure_macos_tray_title(&tray);
    }
    Ok(SystemTray {
        tray,
        show_item,
        download_item,
        upload_item,
        reload_item,
        quit_item,
        display_state: Rc::new(RefCell::new(SystemTrayDisplayState::default())),
    })
}

#[cfg(target_os = "macos")]
fn configure_macos_tray_title(tray: &DioxusTray) {
    use objc2_app_kit::{NSFont, NSLineBreakMode};
    use objc2_foundation::MainThreadMarker;

    let Some(main_thread) = MainThreadMarker::new() else {
        warn!("macOS tray title configuration requested outside the main thread");
        return;
    };
    let Some(status_item) = tray.ns_status_item() else {
        warn!("macOS tray status item is unavailable");
        return;
    };
    let Some(button) = status_item.button(main_thread) else {
        warn!("macOS tray status button is unavailable");
        return;
    };

    button.setUsesSingleLineMode(false);
    button.setLineBreakMode(NSLineBreakMode::ByClipping);
    let font = NSFont::monospacedSystemFontOfSize_weight(9.5, 0.0);
    button.setFont(Some(&font));
}

#[cfg(target_os = "macos")]
fn style_macos_tray_title(tray: &DioxusTray, title_text: &str) {
    use objc2_app_kit::{
        NSBaselineOffsetAttributeName, NSMutableParagraphStyle, NSParagraphStyleAttributeName,
    };
    use objc2_foundation::{MainThreadMarker, NSMutableAttributedString, NSNumber, NSRange};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let Some(status_item) = tray.ns_status_item() else {
        return;
    };
    let Some(button) = status_item.button(main_thread) else {
        return;
    };

    let title = button.attributedTitle();
    let adjusted = NSMutableAttributedString::new();
    adjusted.setAttributedString(&title);
    let range = NSRange::new(0, adjusted.length());
    let paragraph_style = NSMutableParagraphStyle::new();
    paragraph_style.setMinimumLineHeight(8.5);
    paragraph_style.setMaximumLineHeight(8.5);
    let title_offset = NSNumber::numberWithDouble(2.5);
    unsafe {
        adjusted.addAttribute_value_range(NSParagraphStyleAttributeName, &paragraph_style, range);
        adjusted.addAttribute_value_range(NSBaselineOffsetAttributeName, &title_offset, range);
    }

    let (upload_line, download_line) = title_text.split_once('\n').unwrap_or((title_text, ""));
    let download_start = upload_line.encode_utf16().count() + 1;
    let download_len = download_line.encode_utf16().count();
    if download_len > 0 {
        let download_offset = NSNumber::numberWithDouble(-3.75);
        unsafe {
            adjusted.addAttribute_value_range(
                NSBaselineOffsetAttributeName,
                &download_offset,
                NSRange::new(download_start, download_len),
            );
        }
    }

    button.setAttributedTitle(&adjusted);
}

fn tray_labels(
    language: Language,
) -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    match language {
        Language::English => (
            "Open SSH Tunnel Keeper",
            "Download  0 B/s",
            "Upload  0 B/s",
            "Reload configuration",
            "Quit SSH Tunnel Keeper",
        ),
        Language::Chinese => (
            "打开 SSH Tunnel Keeper",
            "下载  0 B/s",
            "上传  0 B/s",
            "重新加载配置",
            "退出 SSH Tunnel Keeper",
        ),
    }
}

#[cfg(target_os = "macos")]
fn activate_macos_application() {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::MainThreadMarker;

    let Some(main_thread) = MainThreadMarker::new() else {
        warn!("macOS application activation requested outside the main thread");
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);
    application.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    application.activate();
}

#[cfg(not(target_os = "macos"))]
fn activate_macos_application() {}

fn create_tray_icon() -> anyhow::Result<TrayIcon> {
    let (rgba, width, height) = decode_icon(tray_icon_bytes())?;
    TrayIcon::from_rgba(rgba, width, height).map_err(anyhow::Error::from)
}

fn tray_icon_bytes() -> &'static [u8] {
    #[cfg(target_os = "macos")]
    {
        include_bytes!("../assets/stk-tray-icon.png")
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Windows and Linux do not recolor macOS-style black template icons.
        include_bytes!("../assets/stk-icon-64.png")
    }
}

fn create_window_icon() -> anyhow::Result<dioxus::desktop::tao::window::Icon> {
    let (rgba, width, height) = decode_icon(include_bytes!("../assets/stk-icon-64.png"))?;
    dioxus::desktop::tao::window::Icon::from_rgba(rgba, width, height).map_err(anyhow::Error::from)
}

fn decode_icon(bytes: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory(bytes)
        .map_err(anyhow::Error::from)?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

fn current_runtime_error() -> Option<String> {
    GUI_CONTEXT
        .get()
        .and_then(|context| context.runtime.error())
}

fn request_gui_reload() {
    if let Some(context) = GUI_CONTEXT.get()
        && let Err(error) = context.runtime.reload_or_restart()
    {
        context
            .runtime
            .set_error(format!("failed to restart GUI runtime: {error}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn embedded_application_icons_have_expected_dimensions() {
        let (_, width, height) = decode_icon(include_bytes!("../assets/stk-icon-64.png")).unwrap();
        assert_eq!((width, height), (64, 64));

        let (_, width, height) = decode_icon(include_bytes!("../assets/stk-icon-256.png")).unwrap();
        assert_eq!((width, height), (256, 256));

        let (_, width, height) = decode_icon(tray_icon_bytes()).unwrap();
        let expected = if cfg!(target_os = "macos") {
            (22, 22)
        } else {
            (64, 64)
        };
        assert_eq!((width, height), expected);
    }

    #[test]
    fn hidden_argument_is_reserved_for_system_startup() {
        let arguments = GuiArgs::try_parse_from(["stk-gui", "--hidden"]).unwrap();

        assert!(arguments.hidden);
        assert!(arguments.config.is_none());
    }

    #[test]
    fn reload_restarts_failed_runtime_and_preserves_detailed_error() {
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let path = std::env::temp_dir().join(format!(
            "stk-gui-reload-test-{}-{}.yaml",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(
            &path,
            format!(
                "control:\n  endpoint: tcp:{address}\nhosts:\n  invalid:\n    local-proxies: []\n"
            ),
        )
        .unwrap();
        let manager = Arc::new(GuiRuntimeManager::new(path.clone()));
        manager.start().unwrap();

        let initial_error = wait_for_runtime_error(&manager);
        assert!(initial_error.contains("either host or ssh-config-host must be configured"));

        thread::sleep(Duration::from_millis(50));
        manager.reload_or_restart().unwrap();
        wait_for_reload_completion(&manager);
        let reloaded_error = manager.error().expect("reload error must remain visible");
        assert!(reloaded_error.contains("either host or ssh-config-host must be configured"));
        assert!(!reloaded_error.contains("runtime is not available for reload"));

        manager.stop();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn independent_gui_errors_do_not_clear_each_other() {
        let manager = GuiRuntimeManager::new(PathBuf::from("config.yaml"));
        manager.set_runtime_error("runtime failed".to_string());
        manager.set_status_error("status failed".to_string());
        manager.set_error("reload failed".to_string());

        let error = manager.error().unwrap();
        assert!(error.contains("runtime failed"));
        assert!(error.contains("status failed"));
        assert!(error.contains("reload failed"));

        manager.clear_error();
        let error = manager.error().unwrap();
        assert!(error.contains("runtime failed"));
        assert!(error.contains("status failed"));
        assert!(!error.contains("reload failed"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_tray_updates_are_coalesced_until_the_main_queue_consumes_them() {
        let mut updates = MacosTrayUpdateQueue::default();

        assert!(updates.request((1, 2)));
        assert!(!updates.request((3, 4)));
        assert_eq!(updates.take(), Some((3, 4)));
        assert!(updates.request((5, 6)));
    }

    #[test]
    fn second_gui_manager_attaches_to_the_existing_runtime() {
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let path = std::env::temp_dir().join(format!(
            "stk-gui-attach-test-{}-{}.yaml",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(
            &path,
            format!("control:\n  endpoint: tcp:{address}\nhosts:\n  idle:\n    auto: false\n"),
        )
        .unwrap();

        let owner = Arc::new(GuiRuntimeManager::new(path.clone()));
        owner.start().unwrap();
        let endpoint = ControlEndpoint::Tcp(address);
        for _ in 0..100 {
            if probe_runtime(&endpoint).is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(probe_runtime(&endpoint).is_some());

        let attached = Arc::new(GuiRuntimeManager::new(path.clone()));
        attached.start().unwrap();
        assert!(attached.is_attached());

        attached.stop();
        owner.stop();
        fs::remove_file(path).unwrap();
    }

    fn wait_for_runtime_error(manager: &GuiRuntimeManager) -> String {
        for _ in 0..100 {
            if let Some(error) = manager.error() {
                return error;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("GUI runtime did not report its startup error");
    }

    fn wait_for_reload_completion(manager: &GuiRuntimeManager) {
        for _ in 0..250 {
            if !manager.reload_in_progress.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("GUI reload worker did not complete");
    }
}
