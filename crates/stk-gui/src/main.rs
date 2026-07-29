#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use clap::Parser;
use dioxus::desktop::trayicon::{
    DioxusTray, Icon as TrayIcon, init_tray_icon,
    menu::{Menu, MenuItem, PredefinedMenuItem},
};
use dioxus::desktop::{Config, LogicalSize, WindowBuilder, WindowCloseBehaviour};
#[cfg(target_os = "linux")]
use std::env;
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    thread::{self, JoinHandle},
};
use stk_core::{
    AppConfig, ConfigScope, ControlConfig, ControlEndpoint, RuntimeProfile,
    RuntimeSnapshotSubscription, default_config_directory, fetch_runtime_snapshot,
    fetch_traffic_history,
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
    subscribe_runtime_snapshots,
};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

mod app;
mod autostart;
mod gui_config;
mod logging;

use app::App;
use gui_config::{GuiConfig, Language};

const TRAY_SHOW_ID: &str = "stk-show";
const TRAY_RELOAD_ID: &str = "stk-reload";

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
}

struct GuiRuntimeManager {
    config_path: PathBuf,
    runtime_error: Arc<Mutex<Option<String>>>,
    state: Mutex<GuiRuntimeState>,
}

struct GuiRuntimeState {
    runtime: Option<GuiRuntime>,
    reload_handle: Option<ReloadHandle>,
    attached_endpoint: Option<ControlEndpoint>,
    last_snapshot: Option<RuntimeSnapshot>,
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
    let args = GuiArgs::parse();
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
            std::process::exit(1);
        }
        return;
    }

    let runtime = Arc::new(GuiRuntimeManager::new(config_path));
    assert!(
        GUI_CONTEXT
            .set(GuiContext {
                runtime: Arc::clone(&runtime),
                gui_config_path,
                gui_config,
            })
            .is_ok(),
        "GUI context must only be initialized once"
    );
    if let Err(error) = runtime.start() {
        runtime.set_error(format!("failed to start GUI runtime thread: {error}"));
    }
    dioxus::LaunchBuilder::desktop()
        .with_cfg(desktop_config(args.hidden))
        .launch(App);
    runtime.stop();
}

fn desktop_config(start_hidden: bool) -> Config {
    let window = WindowBuilder::new()
        .with_title("SSH Tunnel Keeper")
        .with_inner_size(LogicalSize::new(974.0, 790.0))
        .with_min_inner_size(LogicalSize::new(620.0, 500.0))
        .with_visible(!start_hidden)
        .with_always_on_top(false);
    let config = Config::new()
        .with_window(window)
        .with_icon(create_window_icon())
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
            runtime_error: Arc::new(Mutex::new(None)),
            state: Mutex::new(GuiRuntimeState {
                runtime: None,
                reload_handle: None,
                attached_endpoint: None,
                last_snapshot: None,
            }),
        }
    }

    fn start(&self) -> io::Result<()> {
        let mut state = self.state.lock().expect("GUI runtime state lock poisoned");
        state.reload_handle = None;
        state.runtime = None;
        state.attached_endpoint = None;
        state.last_snapshot = None;
        self.clear_error();

        let endpoint = self.configured_endpoint().map_err(io::Error::other)?;
        if let Some(snapshot) = probe_runtime(&endpoint) {
            info!(%endpoint, "GUI attached to an existing runtime");
            state.attached_endpoint = Some(endpoint);
            state.last_snapshot = Some(snapshot);
            return Ok(());
        }

        let reload_control = ReloadControl::new();
        let reload_handle = reload_control.handle();
        let runtime = GuiRuntime::start(
            self.config_path.clone(),
            Arc::clone(&self.runtime_error),
            reload_control,
        )?;
        state.reload_handle = Some(reload_handle);
        state.runtime = Some(runtime);
        Ok(())
    }

    fn reload_or_restart(self: &Arc<Self>) -> io::Result<()> {
        let attached_endpoint = self
            .state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .attached_endpoint
            .clone();
        if let Some(endpoint) = attached_endpoint {
            let manager = Arc::clone(self);
            thread::Builder::new()
                .name("stk-gui-control".to_string())
                .spawn(move || {
                    let result = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(anyhow::Error::from)
                        .and_then(|runtime| {
                            runtime.block_on(async {
                                tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    request_runtime_reload(&endpoint),
                                )
                                .await
                                .map_err(|_| {
                                    anyhow::anyhow!("timed out requesting runtime reload")
                                })?
                            })
                        });
                    match result {
                        Ok(()) => manager.clear_error(),
                        Err(error) => manager.set_error(format!(
                            "failed to reload attached runtime at {endpoint}: {error:#}"
                        )),
                    }
                })?;
            return Ok(());
        }

        let should_restart = {
            let state = self.state.lock().expect("GUI runtime state lock poisoned");
            let running = state
                .runtime
                .as_ref()
                .is_some_and(|runtime| !runtime.is_finished());
            !running
                || !state
                    .reload_handle
                    .as_ref()
                    .is_some_and(ReloadHandle::request_reload)
        };
        if should_restart {
            self.start()?;
        }
        Ok(())
    }

    async fn status_subscription(&self) -> anyhow::Result<RuntimeSnapshotSubscription> {
        let attached_endpoint = self
            .state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .attached_endpoint
            .clone();
        let configured_endpoint = self.configured_endpoint()?;
        let endpoint = attached_endpoint
            .clone()
            .unwrap_or_else(|| configured_endpoint.clone());
        let primary = subscribe_status_endpoint(&endpoint).await;
        match primary {
            Ok(subscription) => {
                self.clear_error();
                Ok(subscription)
            }
            Err(primary_error)
                if attached_endpoint.is_some() && configured_endpoint != endpoint =>
            {
                if let Ok(subscription) = subscribe_status_endpoint(&configured_endpoint).await {
                    self.state
                        .lock()
                        .expect("GUI runtime state lock poisoned")
                        .attached_endpoint = Some(configured_endpoint);
                    self.clear_error();
                    return Ok(subscription);
                }
                let message =
                    format!("failed to subscribe to runtime at {endpoint}: {primary_error:#}");
                self.set_error(message.clone());
                Err(anyhow::anyhow!(message))
            }
            Err(error) => {
                let message = format!("failed to subscribe to runtime at {endpoint}: {error:#}");
                self.set_error(message.clone());
                Err(anyhow::anyhow!(message))
            }
        }
    }

    fn accept_stream_snapshot(&self, snapshot: RuntimeSnapshot) {
        self.state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .last_snapshot = Some(snapshot);
        self.clear_error();
    }

    async fn traffic_history(&self) -> Option<TrafficHistorySnapshot> {
        let attached_endpoint = self
            .state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .attached_endpoint
            .clone();
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
        let attached_endpoint = self
            .state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .attached_endpoint
            .clone();
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
        let attached_endpoint = self
            .state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .attached_endpoint
            .clone();
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
        let attached_endpoint = self
            .state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .attached_endpoint
            .clone();
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
        self.state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .last_snapshot
            .clone()
            .unwrap_or_else(runtime_snapshot)
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
        self.state
            .lock()
            .expect("GUI runtime state lock poisoned")
            .attached_endpoint
            .is_some()
    }

    fn stop(&self) {
        let mut state = self.state.lock().expect("GUI runtime state lock poisoned");
        state.reload_handle = None;
        state.runtime = None;
        state.attached_endpoint = None;
        state.last_snapshot = None;
    }

    fn error(&self) -> Option<String> {
        self.runtime_error
            .lock()
            .expect("runtime error lock poisoned")
            .clone()
    }

    fn set_error(&self, error: String) {
        let mut current = self
            .runtime_error
            .lock()
            .expect("runtime error lock poisoned");
        if current.as_deref() != Some(error.as_str()) {
            error!(%error, "GUI runtime error");
            *current = Some(error);
        }
    }

    fn clear_error(&self) {
        *self
            .runtime_error
            .lock()
            .expect("runtime error lock poisoned") = None;
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

async fn subscribe_status_endpoint(
    endpoint: &ControlEndpoint,
) -> anyhow::Result<RuntimeSnapshotSubscription> {
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        subscribe_runtime_snapshots(endpoint),
    )
    .await
    .map_err(|_| anyhow::anyhow!("status subscription timed out"))?
}

impl GuiRuntime {
    fn start(
        config_path: PathBuf,
        runtime_error: Arc<Mutex<Option<String>>>,
        reload_control: ReloadControl,
    ) -> io::Result<Self> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("stk-gui-runtime".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        error!(%error, "failed to create GUI Tokio runtime");
                        *runtime_error.lock().expect("runtime error lock poisoned") =
                            Some(format!("failed to create Tokio runtime: {error}"));
                        return;
                    }
                };
                let result = runtime.block_on(run_config_file_with_control_until_shutdown(
                    config_path,
                    RuntimeProfile::Foreground,
                    reload_control,
                    async move {
                        let _ = shutdown_rx.await;
                    },
                ));
                if let Err(error) = result {
                    error!(
                        error = %format_args!("{error:#}"),
                        "GUI proxy runtime stopped with an error"
                    );
                    *runtime_error.lock().expect("runtime error lock poisoned") =
                        Some(format!("{error:#}"));
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

fn init_system_tray(language: Language) -> SystemTray {
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
    ])
    .expect("tray menu items must be valid");

    let tray = init_tray_icon(menu, Some(create_tray_icon()));
    #[cfg(target_os = "macos")]
    {
        tray.set_icon_as_template(true);
        configure_macos_tray_title(&tray);
    }
    SystemTray {
        tray,
        show_item,
        download_item,
        upload_item,
        reload_item,
        quit_item,
    }
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

fn create_tray_icon() -> TrayIcon {
    let (rgba, width, height) = decode_icon(include_bytes!("../assets/stk-tray-icon.png"));
    TrayIcon::from_rgba(rgba, width, height).expect("tray icon pixels must be valid")
}

fn create_window_icon() -> dioxus::desktop::tao::window::Icon {
    let (rgba, width, height) = decode_icon(include_bytes!("../assets/stk-icon-64.png"));
    dioxus::desktop::tao::window::Icon::from_rgba(rgba, width, height)
        .expect("window icon pixels must be valid")
}

fn decode_icon(bytes: &[u8]) -> (Vec<u8>, u32, u32) {
    let image = image::load_from_memory(bytes)
        .expect("embedded icon must be a valid PNG")
        .into_rgba8();
    let (width, height) = image.dimensions();
    (image.into_raw(), width, height)
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
        let (_, width, height) = decode_icon(include_bytes!("../assets/stk-icon-64.png"));
        assert_eq!((width, height), (64, 64));

        let (_, width, height) = decode_icon(include_bytes!("../assets/stk-tray-icon.png"));
        assert_eq!((width, height), (22, 22));
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
        let reloaded_error = wait_for_runtime_error(&manager);
        assert!(reloaded_error.contains("either host or ssh-config-host must be configured"));
        assert!(!reloaded_error.contains("runtime is not available for reload"));

        manager.stop();
        fs::remove_file(path).unwrap();
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
}
