use anyhow::Context as _;
use dioxus::desktop::{use_tray_menu_event_handler, use_window};
use dioxus::prelude::*;
use lucide_dioxus::{
    Activity, ArrowDown, ArrowUp, CircleAlert, CircleCheck, CircleDot, Clock, FileText, Languages,
    Network, RefreshCw, Router, Save, ScrollText, Search, Server, Square, Trash2, Unplug,
};
use std::{
    collections::VecDeque,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use stk_core::{
    AppConfig,
    config::ConfigFormat,
    stats::{
        ConnectionRuntimeSnapshot, ConnectionRuntimeStatus, HostRuntimeSnapshot, HostRuntimeStatus,
        RuntimeSnapshot, SshSessionRuntimeSnapshot, SshSessionRuntimeStatus, TrafficHistoryPoint,
        TunnelKind, TunnelRuntimeSnapshot, TunnelRuntimeStatus,
    },
};
use tempfile::NamedTempFile;
use time::{OffsetDateTime, UtcOffset, macros::format_description};
use tracing::{info, warn};

use super::{
    gui_config::{GuiConfig, Language},
    logging::{GuiLogEntry, GuiLogLevel},
};

const APP_CSS: &str = include_str!("style.css");
const TRAFFIC_HISTORY_WINDOW_MINUTES: usize = 60;
const TRAFFIC_CHART_BUCKET_MINUTES: usize = 2;
const TRAFFIC_CHART_BAR_COUNT: usize =
    TRAFFIC_HISTORY_WINDOW_MINUTES / TRAFFIC_CHART_BUCKET_MINUTES;
const EVENT_LIMIT: usize = 7;

fn tr(language: Language, english: &'static str, chinese: &'static str) -> &'static str {
    match language {
        Language::English => english,
        Language::Chinese => chinese,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Overview,
    Runtime,
    Connections,
    Logs,
    Configuration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum LogLevelFilter {
    #[default]
    All,
    Info,
    Warn,
    Error,
}

impl LogLevelFilter {
    fn allows(self, level: GuiLogLevel) -> bool {
        match self {
            Self::All => true,
            Self::Info => level == GuiLogLevel::Info,
            Self::Warn => level == GuiLogLevel::Warn,
            Self::Error => level == GuiLogLevel::Error,
        }
    }
}

impl View {
    fn title(self, language: Language) -> &'static str {
        match self {
            Self::Overview => tr(language, "Overview", "概览"),
            Self::Runtime => tr(language, "Hosts & sessions", "主机与会话"),
            Self::Connections => tr(language, "Connections", "连接"),
            Self::Logs => tr(language, "Logs", "日志"),
            Self::Configuration => tr(language, "Configuration", "配置"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct ThroughputPoint {
    upload_bps: f64,
    download_bps: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct TrafficChartSample {
    started_at_unix_ms: u64,
    upload_bps: f64,
    download_bps: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct TrafficChartBar {
    sample: Option<TrafficChartSample>,
    upload_height: f64,
    download_height: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct ConnectionTableEntry {
    host_name: String,
    connection: ConnectionRuntimeSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeEventKind {
    Success,
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
struct RuntimeEvent {
    recorded_at: Instant,
    kind: RuntimeEventKind,
    message: RuntimeEventMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeEventMessage {
    DesktopInitialized,
    RuntimeStarted,
    RuntimeStopped,
    ConfigurationApplied(u64),
    ActiveSessions(u64),
}

impl RuntimeEventMessage {
    fn localized(&self, language: Language) -> String {
        match self {
            Self::DesktopInitialized => tr(
                language,
                "Desktop runtime initialized",
                "桌面运行时已初始化",
            )
            .to_string(),
            Self::RuntimeStarted => tr(language, "Runtime started", "运行时已启动").to_string(),
            Self::RuntimeStopped => tr(language, "Runtime stopped", "运行时已停止").to_string(),
            Self::ConfigurationApplied(generation) => match language {
                Language::English => format!("Configuration generation {generation} applied"),
                Language::Chinese => format!("配置版本 {generation} 已生效"),
            },
            Self::ActiveSessions(count) => match language {
                Language::English => format!("{count} SSH sessions active"),
                Language::Chinese => format!("{count} 个 SSH 会话正在运行"),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorNoticeKind {
    Success,
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorNotice {
    kind: EditorNoticeKind,
    message: String,
}

#[derive(Clone)]
struct ConfigEditorState {
    text: String,
    saved_text: String,
    disk_config: Option<AppConfig>,
    notice: Option<EditorNotice>,
}

impl ConfigEditorState {
    fn load(path: &Path, language: Language) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => match parse_configuration(path, &text) {
                Ok(config) => Self {
                    saved_text: text.clone(),
                    text,
                    disk_config: Some(config),
                    notice: None,
                },
                Err(error) => Self {
                    saved_text: text.clone(),
                    text,
                    disk_config: None,
                    notice: Some(EditorNotice::error(error)),
                },
            },
            Err(error) => Self {
                text: String::new(),
                saved_text: String::new(),
                disk_config: None,
                notice: Some(EditorNotice::error(format!(
                    "{} {}: {error}",
                    tr(language, "Failed to read", "读取失败"),
                    path.display(),
                ))),
            },
        }
    }

    fn dirty(&self) -> bool {
        self.text != self.saved_text
    }
}

impl EditorNotice {
    fn success(message: impl Into<String>) -> Self {
        Self {
            kind: EditorNoticeKind::Success,
            message: message.into(),
        }
    }

    fn info(message: impl Into<String>) -> Self {
        Self {
            kind: EditorNoticeKind::Info,
            message: message.into(),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            kind: EditorNoticeKind::Error,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AutoStartUiState {
    enabled: bool,
    supported: bool,
    error: Option<String>,
}

impl AutoStartUiState {
    fn load() -> Self {
        let supported = super::autostart::is_supported();
        match super::autostart::is_enabled() {
            Ok(enabled) => Self {
                enabled,
                supported,
                error: None,
            },
            Err(error) => Self {
                enabled: false,
                supported,
                error: Some(format!("{error:#}")),
            },
        }
    }
}

fn update_system_tray(
    tray: &super::SystemTray,
    language: Language,
    snapshot: &RuntimeSnapshot,
    rates: ThroughputPoint,
) {
    let state = if snapshot.running {
        tr(language, "Running", "运行中")
    } else {
        tr(language, "Stopped", "已停止")
    };
    let download = format_rate(rates.download_bps);
    let upload = format_rate(rates.upload_bps);
    tray.show_item.set_text(tr(
        language,
        "Open SSH Tunnel Keeper",
        "打开 SSH Tunnel Keeper",
    ));
    tray.download_item.set_text(match language {
        Language::English => format!("Download  {download}"),
        Language::Chinese => format!("下载  {download}"),
    });
    tray.upload_item.set_text(match language {
        Language::English => format!("Upload  {upload}"),
        Language::Chinese => format!("上传  {upload}"),
    });
    tray.reload_item
        .set_text(tr(language, "Reload configuration", "重新加载配置"));
    tray.quit_item.set_text(tr(
        language,
        "Quit SSH Tunnel Keeper",
        "退出 SSH Tunnel Keeper",
    ));
    let tray_title = tray_throughput_title(rates.upload_bps, rates.download_bps);
    tray.tray.set_title(Some(&tray_title));
    #[cfg(target_os = "macos")]
    super::style_macos_tray_title(&tray.tray, &tray_title);
    let tooltip = match language {
        Language::English => {
            format!("SSH Tunnel Keeper - {state}\nUpload {upload}\nDownload {download}")
        }
        Language::Chinese => {
            format!("SSH Tunnel Keeper - {state}\n上传 {upload}\n下载 {download}")
        }
    };
    let _ = tray.tray.set_tooltip(Some(tooltip));
}

#[component]
pub fn App() -> Element {
    let Some(context) = super::GUI_CONTEXT.get() else {
        return rsx! {
            style { dangerous_inner_html: APP_CSS }
            main { class: "fatal-startup-error",
                CircleAlert { size: 24 }
                h1 { "SSH Tunnel Keeper" }
                p { "The GUI runtime context is unavailable. Check stk.log for startup details." }
            }
        };
    };
    let initial_gui_config = context.gui_config.clone();
    let initial_language = initial_gui_config.language;
    let system_tray = use_hook(move || {
        super::init_system_tray(initial_language).map_err(|error| format!("{error:#}"))
    });
    let system_tray = match system_tray {
        Ok(system_tray) => system_tray,
        Err(error) => {
            context
                .runtime
                .set_error(format!("failed to initialize the system tray: {error}"));
            return rsx! {
                style { dangerous_inner_html: APP_CSS }
                main { class: "fatal-startup-error",
                    CircleAlert { size: 24 }
                    h1 { "SSH Tunnel Keeper" }
                    p { "Failed to initialize the system tray: {error}" }
                }
            };
        }
    };
    let window = use_window();
    let window_for_menu = window.clone();
    let _tray_menu_handler =
        use_tray_menu_event_handler(move |event| match event.id().0.as_str() {
            super::TRAY_SHOW_ID => {
                super::activate_macos_application();
                window_for_menu.window.set_visible(true);
                window_for_menu.window.set_focus();
            }
            super::TRAY_RELOAD_ID => super::request_gui_reload(),
            _ => {}
        });

    let config_path = context.runtime.config_path.clone();
    let gui_config_path = context.gui_config_path.clone();
    let initial_snapshot = context.runtime.initial_snapshot();
    let initial_throughput = snapshot_throughput(&initial_snapshot);
    let initial_history = context.runtime.initial_traffic_history().points;
    let editor_path = config_path.clone();

    let mut active_view = use_signal(|| View::Overview);
    let mut gui_config = use_signal(move || initial_gui_config);
    let mut status = use_signal(|| initial_snapshot);
    let mut runtime_error = use_signal(super::current_runtime_error);
    let mut throughput = use_signal(|| initial_throughput);
    let mut history = use_signal(|| initial_history);
    let mut events = use_signal(|| {
        VecDeque::from([RuntimeEvent {
            recorded_at: Instant::now(),
            kind: RuntimeEventKind::Info,
            message: RuntimeEventMessage::DesktopInitialized,
        }])
    });
    let mut logs = use_signal(super::logging::snapshot);
    let mut editor = use_signal(move || ConfigEditorState::load(&editor_path, initial_language));
    let auto_start = use_signal(AutoStartUiState::load);

    let config_path_for_poll = config_path.clone();
    let runtime_for_poll = Arc::clone(&context.runtime);
    let tray_for_poll = system_tray.clone();
    use_future(move || {
        let config_path_for_poll = config_path_for_poll.clone();
        let runtime_for_poll = Arc::clone(&runtime_for_poll);
        let tray_for_poll = tray_for_poll.clone();
        async move {
            let mut last_auxiliary_refresh = Instant::now() - Duration::from_secs(1);
            let mut poll_interval = tokio::time::interval(super::STATUS_POLL_INTERVAL);
            poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                poll_interval.tick().await;
                let next = match runtime_for_poll.status_snapshot().await {
                    Ok(next) => next,
                    Err(_) => {
                        runtime_error.set(super::current_runtime_error());
                        continue;
                    }
                };
                runtime_for_poll.accept_snapshot(next.clone());
                let next_error = super::current_runtime_error();
                let previous = status.read().clone();
                let next_throughput = snapshot_throughput(&next);
                let language = gui_config.read().language;

                append_runtime_events(&mut events, &previous, &next);
                if next.config_generation != previous.config_generation {
                    observe_configuration_change(&config_path_for_poll, &mut editor, language);
                }

                update_system_tray(&tray_for_poll, language, &next, next_throughput);
                throughput.set(next_throughput);
                status.set(next);
                runtime_error.set(next_error);
                if last_auxiliary_refresh.elapsed() >= Duration::from_secs(1) {
                    logs.set(super::logging::snapshot());
                    last_auxiliary_refresh = Instant::now();
                }
            }
        }
    });

    let runtime_for_history = Arc::clone(&context.runtime);
    use_future(move || {
        let runtime_for_history = Arc::clone(&runtime_for_history);
        async move {
            loop {
                if let Some(next) = runtime_for_history.traffic_history().await {
                    history.set(next.points);
                }
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }
    });

    let tray_for_effect = system_tray.clone();
    use_effect(move || {
        let rates = *throughput.read();
        let snapshot = status.read();
        let language = gui_config.read().language;
        update_system_tray(&tray_for_effect, language, &snapshot, rates);
    });

    let active = *active_view.read();
    let language = gui_config.read().language;
    let snapshot = status.read().clone();
    let error = runtime_error.read().clone();
    let current_throughput = *throughput.read();
    let history_points = history.read().clone();
    let recent_events = events.read().iter().cloned().collect::<Vec<_>>();
    let runtime_hosts = snapshot.hosts.clone();
    let current_logs = logs.read().clone();
    let generation = snapshot.config_generation;
    let gui_config_path_for_english = gui_config_path.clone();
    let gui_config_path_for_chinese = gui_config_path.clone();

    rsx! {
        style { dangerous_inner_html: APP_CSS }
        div { class: "app-shell",
            aside { class: "sidebar",
                div { class: "brand",
                    div { class: "brand-mark",
                        svg { view_box: "0 0 64 64",
                            rect { x: "2", y: "2", width: "60", height: "60", rx: "15", fill: "#20272b", stroke: "#465157", stroke_width: "2" }
                            path { d: "M18 48V29A14 14 0 0 1 46 29V48", fill: "none", stroke: "#f1f5f3", stroke_width: "7", stroke_linecap: "round" }
                            path { d: "M7 46H21C25 46 25 39 29 39H35C39 39 39 30 43 30H57", fill: "none", stroke: "#35a874", stroke_width: "5", stroke_linecap: "round", stroke_linejoin: "round" }
                        }
                    }
                    div { class: "brand-copy",
                        strong { "SSH Tunnel Keeper" }
                        span { "STK" }
                    }
                }
                nav { class: "primary-nav", aria_label: tr(language, "Primary navigation", "主导航"),
                    NavButton {
                        label: tr(language, "Overview", "概览"),
                        active: active == View::Overview,
                        onclick: move |_| active_view.set(View::Overview),
                        Activity { size: 18 }
                    }
                    NavButton {
                        label: tr(language, "Runtime", "运行"),
                        active: active == View::Runtime,
                        onclick: move |_| active_view.set(View::Runtime),
                        Server { size: 18 }
                    }
                    NavButton {
                        label: tr(language, "Connections", "连接"),
                        active: active == View::Connections,
                        onclick: move |_| active_view.set(View::Connections),
                        Network { size: 18 }
                    }
                    NavButton {
                        label: tr(language, "Logs", "日志"),
                        active: active == View::Logs,
                        onclick: move |_| active_view.set(View::Logs),
                        ScrollText { size: 18 }
                    }
                    NavButton {
                        label: tr(language, "Config", "配置"),
                        active: active == View::Configuration,
                        onclick: move |_| active_view.set(View::Configuration),
                        FileText { size: 18 }
                    }
                }
                div { class: "sidebar-footer",
                    span { {tr(language, "Generation", "版本")} }
                    strong { "{generation}" }
                }
            }
            main { class: "workspace",
                header { class: "topbar",
                    div { class: "topbar-title",
                        h1 { "{active.title(language)}" }
                        span { class: "config-path", title: "{config_path.display()}",
                            "{config_path.display()}"
                        }
                    }
                    div { class: "topbar-actions",
                        RuntimeBadge { running: snapshot.running, failed: error.is_some(), language }
                        div { class: "language-switch", title: tr(language, "Interface language", "界面语言"),
                            Languages { size: 15 }
                            button {
                                class: if language == Language::Chinese { "active" } else { "" },
                                aria_label: "中文",
                                onclick: move |_| {
                                    let next = GuiConfig { language: Language::Chinese };
                                    match next.save(&gui_config_path_for_chinese) {
                                        Ok(()) => gui_config.set(next),
                                        Err(error) => warn!(
                                            config = %gui_config_path_for_chinese.display(),
                                            %error,
                                            "failed to save GUI language"
                                        ),
                                    }
                                },
                                "中"
                            }
                            button {
                                class: if language == Language::English { "active" } else { "" },
                                aria_label: "English",
                                onclick: move |_| {
                                    let next = GuiConfig { language: Language::English };
                                    match next.save(&gui_config_path_for_english) {
                                        Ok(()) => gui_config.set(next),
                                        Err(error) => warn!(
                                            config = %gui_config_path_for_english.display(),
                                            %error,
                                            "failed to save GUI language"
                                        ),
                                    }
                                },
                                "EN"
                            }
                        }
                        button {
                            class: "icon-button",
                            title: tr(language, "Reload configuration", "重新加载配置"),
                            aria_label: tr(language, "Reload configuration", "重新加载配置"),
                            onclick: move |_| super::request_gui_reload(),
                            RefreshCw { size: 17 }
                        }
                    }
                }
                if let Some(error) = error.as_deref() {
                    div { class: "runtime-error-banner", role: "alert", title: "{error}",
                        CircleAlert { size: 18 }
                        div {
                            strong { {tr(language, "Runtime error", "运行错误")} }
                            span { "{error}" }
                        }
                    }
                }
                div { class: "content",
                    match active {
                        View::Overview => rsx! {
                            Overview {
                                status: snapshot,
                                throughput: current_throughput,
                                history: history_points,
                                events: recent_events,
                                language,
                            }
                        },
                        View::Runtime => rsx! {
                            RuntimeView {
                                hosts: runtime_hosts,
                                language,
                            }
                        },
                        View::Connections => rsx! {
                            ConnectionsView {
                                status: snapshot,
                                language,
                            }
                        },
                        View::Logs => rsx! {
                            LogsView { entries: current_logs, language }
                        },
                        View::Configuration => rsx! {
                            ConfigurationView {
                                config_path: config_path.clone(),
                                editor,
                                auto_start,
                                language,
                            }
                        },
                    }
                }
            }
        }
    }
}

#[component]
fn NavButton(
    label: &'static str,
    active: bool,
    onclick: EventHandler<MouseEvent>,
    children: Element,
) -> Element {
    rsx! {
        button {
            class: if active { "nav-button active" } else { "nav-button" },
            title: "{label}",
            onclick: move |event| onclick.call(event),
            {children}
            span { "{label}" }
        }
    }
}

#[component]
fn RuntimeBadge(running: bool, failed: bool, language: Language) -> Element {
    let (class, label) = if failed {
        ("runtime-badge failed", tr(language, "Attention", "需关注"))
    } else if running {
        ("runtime-badge running", tr(language, "Running", "运行中"))
    } else {
        ("runtime-badge stopped", tr(language, "Stopped", "已停止"))
    };
    rsx! {
        div { class,
            span { class: "status-dot" }
            "{label}"
        }
    }
}

#[component]
fn Overview(
    status: RuntimeSnapshot,
    throughput: ThroughputPoint,
    history: Vec<TrafficHistoryPoint>,
    events: Vec<RuntimeEvent>,
    language: Language,
) -> Element {
    let mut hovered_chart_index = use_signal(|| None::<usize>);
    let recent_history = recent_traffic_history(&history);
    let chart_bars = build_chart_bars(recent_history);
    let hovered_chart = (*hovered_chart_index.read()).and_then(|index| {
        chart_bars
            .get(index)
            .and_then(|bar| bar.sample)
            .map(|sample| {
                let position = (index as f64 + 0.5) / TRAFFIC_CHART_BAR_COUNT as f64 * 100.0;
                let alignment = if position < 18.0 {
                    "align-start"
                } else if position > 82.0 {
                    "align-end"
                } else {
                    ""
                };
                (sample, position, alignment)
            })
    });
    let uptime = status
        .uptime_ms
        .map(|value| format_duration(value, language))
        .unwrap_or_else(|| "-".to_string());
    let latency = status
        .ssh_channel_open
        .average_ms
        .map(|value| format!("{value:.1} ms"))
        .unwrap_or_else(|| "-".to_string());
    let rate_sample_window = if status.rate_window_ms == 0 {
        tr(language, "Waiting for sample", "等待采样").to_string()
    } else {
        match language {
            Language::English => format!("{} s rolling rate", status.rate_window_ms / 1_000),
            Language::Chinese => format!("{} 秒滚动速率", status.rate_window_ms / 1_000),
        }
    };

    rsx! {
        section { class: "overview-page",
            section { class: "traffic-panel",
                div { class: "overview-traffic-summary",
                    section { class: "current-speed-section",
                        div { class: "section-heading",
                            div {
                                span { class: "eyebrow", {tr(language, "Live", "实时")} }
                                h2 { {tr(language, "Current speed", "当前速度")} }
                            }
                            span { class: "traffic-sample-window", "{rate_sample_window}" }
                        }
                        div { class: "rate-pair",
                            div { class: "rate-block upload",
                                ArrowUp { size: 20 }
                                div {
                                    span { {tr(language, "Upload", "上传")} }
                                    strong { "{format_rate(throughput.upload_bps)}" }
                                }
                            }
                            div { class: "rate-block download",
                                ArrowDown { size: 20 }
                                div {
                                    span { {tr(language, "Download", "下载")} }
                                    strong { "{format_rate(throughput.download_bps)}" }
                                }
                            }
                        }
                    }
                    section { class: "total-traffic-section",
                        div { class: "section-heading",
                            div {
                                span { class: "eyebrow", {tr(language, "Cumulative", "累计")} }
                                h2 { {tr(language, "Total traffic", "总流量")} }
                            }
                            span { class: "traffic-sample-window", {tr(language, "Runtime", "本次运行")} }
                        }
                        div { class: "total-traffic-grid",
                            div { class: "total-traffic-value upload",
                                span { ArrowUp { size: 11 } {tr(language, "Upload", "上传")} }
                                strong { "{format_bytes(status.uploaded_bytes_total as f64)}" }
                            }
                            div { class: "total-traffic-value download",
                                span { ArrowDown { size: 11 } {tr(language, "Download", "下载")} }
                                strong { "{format_bytes(status.downloaded_bytes_total as f64)}" }
                            }
                            div { class: "total-traffic-value total",
                                span { {tr(language, "Total", "合计")} }
                                strong { "{format_bytes(status.transferred_bytes_total as f64)}" }
                            }
                        }
                    }
                }
                section { class: "speed-chart-section",
                    div { class: "section-heading",
                        div {
                            span { class: "eyebrow", {tr(language, "History", "历史")} }
                            h2 { {tr(language, "Speed in the last hour", "1 小时内速度图")} }
                        }
                        div { class: "chart-legend compact",
                            span { class: "upload", ArrowUp { size: 12 } {tr(language, "Upload", "上传")} }
                            span { class: "download", ArrowDown { size: 12 } {tr(language, "Download", "下载")} }
                        }
                    }
                    div {
                        class: "traffic-chart-shell",
                        onmouseleave: move |_| hovered_chart_index.set(None),
                        div { class: "traffic-chart", aria_label: tr(language, "One-hour network speed", "最近一小时网络速度"),
                            for (index, bar) in chart_bars.into_iter().enumerate() {
                                div { class: "chart-column", key: "{index}",
                                    span {
                                        class: "chart-bar upload",
                                        style: "height: {bar.upload_height}%",
                                    }
                                    span {
                                        class: "chart-bar download",
                                        style: "height: {bar.download_height}%",
                                    }
                                }
                            }
                            div { class: "chart-hover-layer",
                                for index in 0..TRAFFIC_CHART_BAR_COUNT {
                                    span {
                                        class: "chart-hit-target",
                                        key: "hover-{index}",
                                        onmouseenter: move |_| hovered_chart_index.set(Some(index)),
                                        onmousemove: move |_| hovered_chart_index.set(Some(index)),
                                    }
                                }
                            }
                            if let Some((sample, position, alignment)) = hovered_chart {
                                span { class: "chart-crosshair", style: "left: {position}%" }
                                div {
                                    class: "chart-tooltip {alignment}",
                                    style: "left: {position}%",
                                    strong { "{format_chart_timestamp(sample.started_at_unix_ms)}" }
                                    span { class: "upload", ArrowUp { size: 11 } "{format_rate(sample.upload_bps)}" }
                                    span { class: "download", ArrowDown { size: 11 } "{format_rate(sample.download_bps)}" }
                                }
                            }
                        }
                        div { class: "chart-axis",
                            span { "-60m" }
                            span { "-45m" }
                            span { "-30m" }
                            span { "-15m" }
                            span { {tr(language, "Now", "现在")} }
                        }
                    }
                }
            }

            section { class: "metric-grid",
                Metric {
                    label: tr(language, "Connections", "连接"),
                    value: status.local_connections_active.to_string(),
                    detail: match language {
                        Language::English => format!("{} accepted", status.local_connections_total),
                        Language::Chinese => format!("累计接受 {} 个", status.local_connections_total),
                    },
                    Activity { size: 18 }
                }
                Metric {
                    label: tr(language, "SSH sessions", "SSH 会话"),
                    value: status.ssh_sessions_active.to_string(),
                    detail: match language {
                        Language::English => format!("{} created", status.ssh_sessions_total),
                        Language::Chinese => format!("累计创建 {} 个", status.ssh_sessions_total),
                    },
                    Server { size: 18 }
                }
                Metric {
                    label: tr(language, "Channel latency", "Channel 延迟"),
                    value: latency,
                    detail: match language {
                        Language::English => format!("{} samples", status.ssh_channel_open.samples),
                        Language::Chinese => format!("{} 个样本", status.ssh_channel_open.samples),
                    },
                    Clock { size: 18 }
                }
                Metric {
                    label: tr(language, "Errors", "错误"),
                    value: status.errors_total.to_string(),
                    detail: match language {
                        Language::English => format!("{} reload errors", status.config_reload_errors_total),
                        Language::Chinese => format!("{} 个重载错误", status.config_reload_errors_total),
                    },
                    CircleAlert { size: 18 }
                }
            }

            div { class: "overview-lower",
                section { class: "detail-panel",
                    div { class: "section-heading compact",
                        div {
                            span { class: "eyebrow", {tr(language, "Runtime", "运行时")} }
                            h2 { {tr(language, "Service health", "服务状态")} }
                        }
                    }
                    dl { class: "health-list",
                        HealthRow { label: tr(language, "Uptime", "运行时间"), value: uptime }
                        HealthRow { label: tr(language, "Configured hosts", "已配置主机"), value: status.configured_hosts.to_string() }
                        HealthRow {
                            label: tr(language, "Local listeners", "本地监听"),
                            value: status.configured_local_listeners.to_string(),
                        }
                        HealthRow {
                            label: tr(language, "Remote listeners", "远端监听"),
                            value: status.configured_remote_listeners.to_string(),
                        }
                        HealthRow {
                            label: tr(language, "Configuration reloads", "配置重载"),
                            value: status.config_reloads_total.to_string(),
                        }
                    }
                }
                section { class: "detail-panel activity-panel",
                    div { class: "section-heading compact",
                        div {
                            span { class: "eyebrow", {tr(language, "Runtime", "运行时")} }
                            h2 { {tr(language, "Recent activity", "近期活动")} }
                        }
                    }
                    div { class: "event-list",
                        for event in events {
                            RuntimeEventRow { event, language }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Metric(label: &'static str, value: String, detail: String, children: Element) -> Element {
    rsx! {
        article { class: "metric",
            div { class: "metric-icon", {children} }
            div { class: "metric-content",
                span { "{label}" }
                strong { "{value}" }
                small { "{detail}" }
            }
        }
    }
}

#[component]
fn HealthRow(label: &'static str, value: String) -> Element {
    rsx! {
        div {
            dt { "{label}" }
            dd { "{value}" }
        }
    }
}

#[component]
fn RuntimeEventRow(event: RuntimeEvent, language: Language) -> Element {
    let class = match event.kind {
        RuntimeEventKind::Success => "event-icon success",
        RuntimeEventKind::Info => "event-icon info",
        RuntimeEventKind::Error => "event-icon error",
    };
    rsx! {
        div { class: "event-row",
            span { class,
                if event.kind == RuntimeEventKind::Error {
                    CircleAlert { size: 14 }
                } else {
                    CircleCheck { size: 14 }
                }
            }
            span { class: "event-message", "{event.message.localized(language)}" }
            time { "{format_event_age(event.recorded_at, language)}" }
        }
    }
}

#[component]
fn RuntimeView(hosts: Vec<HostRuntimeSnapshot>, language: Language) -> Element {
    let mut selected_host = use_signal(|| None::<String>);
    if hosts.is_empty() {
        return rsx! {
            EmptyState {
                title: tr(language, "No active SSH hosts", "没有活动的 SSH 主机"),
                detail: tr(
                    language,
                    "Runtime host details will appear after a configuration starts.",
                    "配置启动后将在这里显示主机运行详情。",
                ),
            }
        };
    }

    let selected_name = selected_host.read().clone();
    let selected = selected_name
        .as_ref()
        .and_then(|name| hosts.iter().find(|host| &host.name == name))
        .unwrap_or(&hosts[0])
        .clone();
    let active_sessions = hosts
        .iter()
        .flat_map(|host| &host.sessions)
        .filter(|session| session.status != SshSessionRuntimeStatus::Offline)
        .count();
    let active_tunnels = hosts
        .iter()
        .flat_map(|host| &host.tunnels)
        .filter(|tunnel| tunnel.status == TunnelRuntimeStatus::Listening)
        .count();
    let active_connections = hosts
        .iter()
        .map(|host| host.connections_active)
        .sum::<u64>();
    let selected_name = selected.name.clone();

    rsx! {
        section { class: "runtime-page",
            div { class: "runtime-summary",
                div {
                    span { {tr(language, "Hosts", "主机")} }
                    strong { "{hosts.len()}" }
                }
                div {
                    span { {tr(language, "Active sessions", "活动会话")} }
                    strong { "{active_sessions}" }
                }
                div {
                    span { {tr(language, "Listening tunnels", "监听中的隧道")} }
                    strong { "{active_tunnels}" }
                }
                div {
                    span { {tr(language, "Active connections", "活动连接")} }
                    strong { "{active_connections}" }
                }
            }
            div { class: "runtime-explorer",
                aside { class: "runtime-hosts",
                    div { class: "runtime-hosts-header",
                        span { class: "eyebrow", {tr(language, "SSH hosts", "SSH 主机")} }
                        strong {
                            {match language {
                                Language::English => format!("{} configured", hosts.len()),
                                Language::Chinese => format!("已配置 {} 个", hosts.len()),
                            }}
                        }
                    }
                    div { class: "runtime-host-list",
                        for host in hosts {
                            HostSelector {
                                selected: host.name == selected_name,
                                host: host.clone(),
                                language,
                                onclick: move |_| selected_host.set(Some(host.name.clone())),
                            }
                        }
                    }
                }
                HostRuntimeDetail { host: selected, language }
            }
        }
    }
}

#[component]
fn HostSelector(
    host: HostRuntimeSnapshot,
    selected: bool,
    onclick: EventHandler<MouseEvent>,
    language: Language,
) -> Element {
    let active_sessions = host
        .sessions
        .iter()
        .filter(|session| session.status != SshSessionRuntimeStatus::Offline)
        .count();
    rsx! {
        button {
            class: if selected { "runtime-host selected" } else { "runtime-host" },
            onclick: move |event| onclick.call(event),
            div { class: "runtime-host-title",
                Server { size: 16 }
                strong { "{host.name}" }
                HostStateBadge { status: host.status, language }
            }
            code { "{host.address}" }
            div { class: "runtime-host-meta",
                span {
                    {match language {
                        Language::English => format!("{active_sessions}/{} sessions", host.max_sessions),
                        Language::Chinese => format!("{active_sessions}/{} 会话", host.max_sessions),
                    }}
                }
                span { class: "mini-rate upload", ArrowUp { size: 10 } "{format_rate(host.upload_bps as f64)}" }
                span { class: "mini-rate download", ArrowDown { size: 10 } "{format_rate(host.download_bps as f64)}" }
            }
        }
    }
}

#[component]
fn HostRuntimeDetail(host: HostRuntimeSnapshot, language: Language) -> Element {
    let active_sessions = host
        .sessions
        .iter()
        .filter(|session| session.status != SshSessionRuntimeStatus::Offline)
        .count();
    let healthy_sessions = host
        .sessions
        .iter()
        .filter(|session| session.status == SshSessionRuntimeStatus::Healthy)
        .count();
    let mut sessions = host.sessions.clone();
    sessions.sort_by_key(|session| (session_status_order(session.status), session.id));
    let mut tunnels = host.tunnels.clone();
    tunnels.sort_by_key(|tunnel| (tunnel_kind_order(tunnel.kind), tunnel.name.clone()));

    rsx! {
        div { class: "runtime-detail",
            section { class: "host-overview",
                div { class: "host-identity",
                    div { class: "host-icon", Server { size: 20 } }
                    div {
                        div { class: "host-name-line",
                            h2 { "{host.name}" }
                            HostStateBadge { status: host.status, language }
                        }
                        span {
                            "{host.ssh_alias} "
                            {tr(language, "at", "连接到")}
                            " "
                            code { "{host.address}" }
                        }
                    }
                    div { class: "host-live-rate",
                        span { class: "upload", ArrowUp { size: 14 } "{format_rate(host.upload_bps as f64)}" }
                        span { class: "download", ArrowDown { size: 14 } "{format_rate(host.download_bps as f64)}" }
                    }
                }
                div { class: "host-stat-grid",
                    RuntimeStat {
                        label: tr(language, "Sessions", "会话"),
                        value: format!("{active_sessions}/{}", host.max_sessions),
                        detail: match language {
                            Language::English => format!("{healthy_sessions} healthy; minimum {}", host.min_sessions),
                            Language::Chinese => format!("{healthy_sessions} 个健康；最少 {} 个", host.min_sessions),
                        },
                    }
                    RuntimeStat {
                        label: tr(language, "Connections", "连接"),
                        value: host.connections_active.to_string(),
                        detail: match language {
                            Language::English => format!("{} accepted", host.connections_total),
                            Language::Chinese => format!("累计接受 {} 个", host.connections_total),
                        },
                    }
                    RuntimeStat {
                        label: tr(language, "Best RTT", "最佳 RTT"),
                        value: host.rtt_ms.map(|value| format!("{value} ms")).unwrap_or_else(|| "-".to_string()),
                        detail: match language {
                            Language::English => format!("{} restarts", host.restart_count),
                            Language::Chinese => format!("重启 {} 次", host.restart_count),
                        },
                    }
                    TrafficRuntimeStat {
                        label: tr(language, "Traffic", "流量"),
                        upload: format_bytes(host.uploaded_bytes_total as f64),
                        download: format_bytes(host.downloaded_bytes_total as f64),
                        detail: match language {
                            Language::English => format!("{} errors", host.errors_total),
                            Language::Chinese => format!("{} 个错误", host.errors_total),
                        },
                    }
                }
            }

            section { class: "runtime-table-section",
                div { class: "runtime-section-heading",
                    div {
                        span { class: "eyebrow", {tr(language, "SSH connections", "SSH 连接")} }
                        h3 { {tr(language, "Sessions", "会话")} }
                    }
                    span {
                        {match language {
                            Language::English => format!("{} current and recent", sessions.len()),
                            Language::Chinese => format!("当前及近期共 {} 个", sessions.len()),
                        }}
                    }
                }
                if sessions.is_empty() {
                    div { class: "runtime-empty-row", Unplug { size: 18 } {tr(language, "No session attempts recorded", "尚无会话记录")} }
                } else {
                    div { class: "runtime-record-list",
                        for session in sessions {
                            SessionRecord {
                                session,
                                language,
                            }
                        }
                    }
                }
            }

            section { class: "runtime-table-section",
                div { class: "runtime-section-heading",
                    div {
                        span { class: "eyebrow", {tr(language, "Traffic entry points", "流量入口")} }
                        h3 { {tr(language, "Tunnels", "隧道")} }
                    }
                    span {
                        {match language {
                            Language::English => format!("{} configured", tunnels.len()),
                            Language::Chinese => format!("已配置 {} 个", tunnels.len()),
                        }}
                    }
                }
                if tunnels.is_empty() {
                    div { class: "runtime-empty-row", Router { size: 18 } {tr(language, "No active tunnels", "没有活动隧道")} }
                } else {
                    div { class: "runtime-record-list",
                        for tunnel in tunnels {
                            TunnelRecord {
                                tunnel,
                                language,
                            }
                        }
                    }
                }
            }

        }
    }
}

#[component]
fn ConnectionsView(status: RuntimeSnapshot, language: Language) -> Element {
    let recording = status.connection_capture.recording;
    let auto_clear_closed = status.connection_capture.auto_clear_closed;
    let mut connections = status
        .hosts
        .iter()
        .flat_map(|host| {
            host.connections
                .iter()
                .cloned()
                .map(|connection| ConnectionTableEntry {
                    host_name: host.name.clone(),
                    connection,
                })
        })
        .collect::<Vec<_>>();
    connections.sort_by_key(|entry| {
        (
            connection_status_order(entry.connection.status),
            std::cmp::Reverse(entry.connection.id),
        )
    });
    let active_connections = connections
        .iter()
        .filter(|entry| {
            matches!(
                entry.connection.status,
                ConnectionRuntimeStatus::Connecting | ConnectionRuntimeStatus::Active
            )
        })
        .count();
    let connection_count = connections.len();
    let has_connections = connection_count > 0;
    let Some(context) = super::GUI_CONTEXT.get() else {
        return rsx! {
            section { class: "inline-error", role: "alert",
                CircleAlert { size: 18 }
                "GUI runtime context is unavailable"
            }
        };
    };
    let runtime_for_recording = Arc::clone(&context.runtime);
    let runtime_for_clear = Arc::clone(&context.runtime);
    let runtime_for_auto_clear = Arc::clone(&context.runtime);

    rsx! {
        section { class: "connections-page",
            section { class: "connection-monitor",
                div { class: "connection-monitor-toolbar",
                    div { class: "connection-monitor-title",
                        div {
                            span { class: "eyebrow", {tr(language, "Network monitor", "网络监视")} }
                            h2 { {tr(language, "Connections", "连接")} }
                        }
                        span {
                            {match language {
                                Language::English => format!("{connection_count} captured / {active_connections} active"),
                                Language::Chinese => format!("已记录 {connection_count} 条 / {active_connections} 条活动"),
                            }}
                        }
                    }
                    div { class: "connection-monitor-actions",
                        label { class: "connection-auto-clear",
                            input {
                                r#type: "checkbox",
                                checked: auto_clear_closed,
                                onchange: move |event| {
                                    let runtime = Arc::clone(&runtime_for_auto_clear);
                                    let enabled = event.checked();
                                    spawn(async move {
                                        if let Err(error) = runtime
                                            .set_connection_capture_auto_clear_closed(enabled)
                                            .await
                                        {
                                            runtime.set_error(format!(
                                                "failed to update connection auto-clear: {error:#}"
                                            ));
                                        }
                                    });
                                },
                            }
                            span { {tr(language, "Auto-clear closed", "自动清理已关闭")} }
                        }
                        button {
                            class: "button secondary",
                            disabled: !has_connections,
                            title: tr(language, "Clear captured connections", "清空连接记录"),
                            onclick: move |_| {
                                let runtime = Arc::clone(&runtime_for_clear);
                                spawn(async move {
                                    if let Err(error) = runtime.clear_captured_connections().await {
                                        runtime.set_error(format!(
                                            "failed to clear captured connections: {error:#}"
                                        ));
                                    }
                                });
                            },
                            Trash2 { size: 14 }
                            {tr(language, "Clear", "清空")}
                        }
                        button {
                            class: if recording { "button secondary connection-recording" } else { "button connection-record-start" },
                            title: if recording {
                                tr(language, "Stop recording new connections", "停止记录新连接")
                            } else {
                                tr(language, "Start recording new connections", "开始记录新连接")
                            },
                            onclick: move |_| {
                                let runtime = Arc::clone(&runtime_for_recording);
                                spawn(async move {
                                    if let Err(error) = runtime
                                        .set_connection_capture_recording(!recording)
                                        .await
                                    {
                                        runtime.set_error(format!(
                                            "failed to update connection capture: {error:#}"
                                        ));
                                    }
                                });
                            },
                            if recording {
                                Square { size: 13 }
                                {tr(language, "Stop", "停止")}
                            } else {
                                CircleDot { size: 13 }
                                {tr(language, "Start", "开始")}
                            }
                        }
                    }
                }
                div { class: if recording { "connection-capture-strip recording" } else { "connection-capture-strip" },
                    span { class: "status-dot" }
                    {if recording {
                        tr(language, "Recording new connections", "正在记录新连接")
                    } else {
                        tr(language, "Recording stopped", "记录已停止")
                    }}
                }
                if !has_connections {
                    div { class: "connection-monitor-empty",
                        Network { size: 22 }
                        strong {
                            {if recording {
                                tr(language, "Waiting for new connections", "等待新的连接")
                            } else {
                                tr(language, "No captured connections", "尚无连接记录")
                            }}
                        }
                    }
                } else {
                    div { class: "connection-table-scroll connection-monitor-table",
                        table { class: "connection-table",
                            thead {
                                tr {
                                    th { class: "connection-status-column", {tr(language, "Status", "状态")} }
                                    th { class: "connection-id-column", {tr(language, "Connection", "连接")} }
                                    th { class: "connection-host-column", {tr(language, "Host", "主机")} }
                                    th { class: "connection-route-column", {tr(language, "Route", "路由")} }
                                    th { class: "connection-session-column", {tr(language, "Session", "会话")} }
                                    th { class: "connection-rate-column", {tr(language, "Live rate", "实时速率")} }
                                    th { class: "connection-traffic-column", {tr(language, "Traffic", "累计流量")} }
                                    th { class: "connection-duration-column", {tr(language, "Duration", "持续时间")} }
                                    th { class: "connection-activity-column", {tr(language, "Activity", "活动")} }
                                }
                            }
                            tbody {
                                for entry in connections {
                                    ConnectionRow {
                                        host_name: entry.host_name,
                                        connection: entry.connection,
                                        language,
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn RuntimeStat(label: &'static str, value: String, detail: String) -> Element {
    rsx! {
        div { class: "host-stat",
            span { "{label}" }
            strong { "{value}" }
            small { "{detail}" }
        }
    }
}

#[component]
fn TrafficRuntimeStat(
    label: &'static str,
    upload: String,
    download: String,
    detail: String,
) -> Element {
    rsx! {
        div { class: "host-stat",
            span { "{label}" }
            div { class: "host-traffic-values",
                span { class: "upload", ArrowUp { size: 12 } "{upload}" }
                span { class: "download", ArrowDown { size: 12 } "{download}" }
            }
            small { "{detail}" }
        }
    }
}

#[component]
fn SessionRecord(session: SshSessionRuntimeSnapshot, language: Language) -> Element {
    let established = session
        .established_at_unix_ms
        .map(format_timestamp)
        .unwrap_or_else(|| "-".to_string());
    let created = format_timestamp(session.created_at_unix_ms);
    let uptime = session
        .uptime_ms
        .map(|value| format_duration(value, language))
        .unwrap_or_else(|| "-".to_string());
    let rtt = session
        .rtt_ms
        .map(|value| format!("{value} ms"))
        .unwrap_or_else(|| "-".to_string());
    let startup = session
        .startup_ms
        .map(|value| format!("{value:.0} ms startup"))
        .unwrap_or_else(|| tr(language, "No startup sample", "暂无启动样本").to_string());
    let startup = match (language, session.startup_ms) {
        (Language::Chinese, Some(value)) => format!("启动耗时 {value:.0} ms"),
        _ => startup,
    };
    let last_activity = format_age_from_unix_ms(session.last_activity_unix_ms, language);
    let last_probe = session
        .last_probe_unix_ms
        .map(|value| format_age_from_unix_ms(value, language))
        .unwrap_or_else(|| "-".to_string());
    let last_probe_timestamp = session
        .last_probe_unix_ms
        .map(format_timestamp)
        .unwrap_or_else(|| "-".to_string());
    rsx! {
        article { class: "runtime-record",
            header { class: "record-header",
                div { class: "record-identity",
                    div { class: "record-icon", Server { size: 16 } }
                    div {
                        strong {
                            {match language {
                                Language::English => format!("Session #{}", session.id),
                                Language::Chinese => format!("会话 #{}", session.id),
                            }}
                        }
                        span { "{session.ssh_alias}" }
                        code { "{session.address}" }
                    }
                }
                div { class: "record-badges",
                    SessionStateBadge { status: session.status, language }
                    if session.remote_forward_owner {
                        span { class: "owner-badge", {tr(language, "Remote owner", "远端监听主会话")} }
                    }
                    if session.retiring {
                        span { class: "retiring-badge", {tr(language, "Retiring", "正在替换")} }
                    }
                }
            }
            div { class: "record-metrics",
                RecordMetric {
                    label: tr(language, "Established", "建立时间"),
                    value: established,
                    detail: match language {
                        Language::English => format!("Created {created}"),
                        Language::Chinese => format!("创建于 {created}"),
                    },
                }
                RecordMetric {
                    label: tr(language, "Uptime", "运行时间"),
                    value: uptime,
                    detail: session.ended_at_unix_ms.map(|value| match language {
                        Language::English => format!("Ended {}", format_timestamp(value)),
                        Language::Chinese => format!("结束于 {}", format_timestamp(value)),
                    }).unwrap_or_else(|| tr(language, "Currently connected", "当前已连接").to_string()),
                }
                RecordMetric {
                    label: tr(language, "Latency", "延迟"),
                    value: rtt,
                    detail: startup,
                }
                RecordMetric {
                    label: tr(language, "Channels", "Channel"),
                    value: match language {
                        Language::English => format!("{} active / {} total", session.active_channels, session.channels_total),
                        Language::Chinese => format!("{} 活动 / {} 累计", session.active_channels, session.channels_total),
                    },
                    detail: match language {
                        Language::English => format!("{} open errors", session.channel_open_errors_total),
                        Language::Chinese => format!("{} 个打开错误", session.channel_open_errors_total),
                    },
                }
                TrafficRecordMetric {
                    label: tr(language, "Live rate", "实时速率"),
                    upload: format_rate(session.upload_bps as f64),
                    download: format_rate(session.download_bps as f64),
                }
                TrafficRecordMetric {
                    label: tr(language, "Traffic", "累计流量"),
                    upload: format_bytes(session.uploaded_bytes_total as f64),
                    download: format_bytes(session.downloaded_bytes_total as f64),
                }
                RecordMetric {
                    label: tr(language, "Last activity", "最近活动"),
                    value: last_activity,
                    detail: format_timestamp(session.last_activity_unix_ms),
                }
                RecordMetric {
                    label: tr(language, "Health probe", "健康探针"),
                    value: last_probe,
                    detail: last_probe_timestamp,
                }
            }
        }
    }
}

#[component]
fn TunnelRecord(tunnel: TunnelRuntimeSnapshot, language: Language) -> Element {
    let target = tunnel
        .target
        .clone()
        .or_else(|| tunnel.protocol.clone())
        .unwrap_or_else(|| "-".to_string());
    let owner = tunnel
        .owner_session_id
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| "-".to_string());
    let started = format_timestamp(tunnel.started_at_unix_ms);
    let last_activity = format_age_from_unix_ms(tunnel.last_activity_unix_ms, language);
    rsx! {
        article { class: "runtime-record",
            header { class: "record-header",
                div { class: "record-identity",
                    div { class: "record-icon", Router { size: 16 } }
                    div {
                        strong { "{tunnel.name}" }
                        span { "{tunnel_kind_label(tunnel.kind, language)}" }
                    }
                }
                div { class: "record-badges",
                    TunnelStateBadge { status: tunnel.status, language }
                }
            }
            div { class: "record-route",
                code { "{tunnel.listen}" }
                span { {tr(language, "to", "到")} }
                code { "{target}" }
                if owner != "-" {
                    span { class: "route-owner",
                        {match language {
                            Language::English => format!("Owner {owner}"),
                            Language::Chinese => format!("主会话 {owner}"),
                        }}
                    }
                }
            }
            if tunnel.status == TunnelRuntimeStatus::Error {
                if let Some(error) = &tunnel.last_error {
                    div { class: "record-error",
                        CircleAlert { size: 12 }
                        span { "{error}" }
                    }
                }
            }
            div { class: "record-metrics tunnel-metrics",
                RecordMetric {
                    label: tr(language, "Connections", "连接"),
                    value: match language {
                        Language::English => format!("{} active / {} total", tunnel.connections_active, tunnel.connections_total),
                        Language::Chinese => format!("{} 活动 / {} 累计", tunnel.connections_active, tunnel.connections_total),
                    },
                    detail: match language {
                        Language::English => format!("{} errors", tunnel.errors_total),
                        Language::Chinese => format!("{} 个错误", tunnel.errors_total),
                    },
                }
                TrafficRecordMetric {
                    label: tr(language, "Live rate", "实时速率"),
                    upload: format_rate(tunnel.upload_bps as f64),
                    download: format_rate(tunnel.download_bps as f64),
                }
                TrafficRecordMetric {
                    label: tr(language, "Traffic", "累计流量"),
                    upload: format_bytes(tunnel.uploaded_bytes_total as f64),
                    download: format_bytes(tunnel.downloaded_bytes_total as f64),
                }
                RecordMetric {
                    label: tr(language, "Started", "启动时间"),
                    value: started,
                    detail: match language {
                        Language::English => format!("Active {last_activity}"),
                        Language::Chinese => format!("活动于 {last_activity}"),
                    },
                }
            }
        }
    }
}

#[component]
fn ConnectionRow(
    host_name: String,
    connection: ConnectionRuntimeSnapshot,
    language: Language,
) -> Element {
    let target = connection
        .target
        .clone()
        .unwrap_or_else(|| tr(language, "Resolving", "正在解析").to_string());
    let protocol = connection
        .protocol
        .clone()
        .unwrap_or_else(|| "-".to_string());
    let session = connection
        .session_id
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| "-".to_string());
    let uptime = format_duration(connection.uptime_ms, language);
    let last_activity = format_age_from_unix_ms(connection.last_activity_unix_ms, language);
    let route = format!("{} -> {target}", connection.peer_address);
    let duration_detail = match connection.established_at_unix_ms {
        Some(established) => match language {
            Language::English => format!(
                "Created {}; established {}",
                format_timestamp(connection.created_at_unix_ms),
                format_timestamp(established)
            ),
            Language::Chinese => format!(
                "创建于 {}；建立于 {}",
                format_timestamp(connection.created_at_unix_ms),
                format_timestamp(established)
            ),
        },
        None => match language {
            Language::English => format!(
                "Created {}",
                format_timestamp(connection.created_at_unix_ms)
            ),
            Language::Chinese => {
                format!("创建于 {}", format_timestamp(connection.created_at_unix_ms))
            }
        },
    };
    let identity_detail = match language {
        Language::English => format!(
            "{}; session {session}; {} errors",
            connection.tunnel_id, connection.errors_total
        ),
        Language::Chinese => format!(
            "{}；会话 {session}；{} 个错误",
            connection.tunnel_id, connection.errors_total
        ),
    };
    let traffic_detail = match language {
        Language::English => format!(
            "Total upload {}; download {}",
            format_bytes(connection.uploaded_bytes_total as f64),
            format_bytes(connection.downloaded_bytes_total as f64)
        ),
        Language::Chinese => format!(
            "累计上传 {}；下载 {}",
            format_bytes(connection.uploaded_bytes_total as f64),
            format_bytes(connection.downloaded_bytes_total as f64)
        ),
    };
    let row_title = connection
        .last_error
        .clone()
        .unwrap_or_else(|| route.clone());
    rsx! {
        tr { class: "connection-row", title: "{row_title}",
            td { class: "connection-status-column",
                ConnectionStateBadge { status: connection.status, language }
            }
            td { class: "connection-id-column", title: "{identity_detail}",
                div { class: "connection-identity-line",
                    strong { "#{connection.id}" }
                    span { "{protocol}" }
                    if connection.errors_total > 0 {
                        CircleAlert { size: 11 }
                    }
                }
            }
            td { class: "connection-host-column", title: "{host_name}", "{host_name}" }
            td { class: "connection-route-column", title: "{route}",
                div { class: "connection-route-line",
                    code { "{connection.peer_address}" }
                    span { "→" }
                    code { "{target}" }
                }
            }
            td { class: "connection-session-column", "{session}" }
            td { class: "connection-rate-column", title: "{traffic_detail}",
                div { class: "connection-flow-values",
                    span { class: "upload", ArrowUp { size: 10 } "{format_rate(connection.upload_bps as f64)}" }
                    span { class: "download", ArrowDown { size: 10 } "{format_rate(connection.download_bps as f64)}" }
                }
            }
            td { class: "connection-traffic-column",
                div { class: "connection-flow-values",
                    span { class: "upload", ArrowUp { size: 10 } "{format_bytes(connection.uploaded_bytes_total as f64)}" }
                    span { class: "download", ArrowDown { size: 10 } "{format_bytes(connection.downloaded_bytes_total as f64)}" }
                }
            }
            td { class: "connection-duration-column", title: "{duration_detail}", "{uptime}" }
            td { class: "connection-activity-column", title: "{format_timestamp(connection.last_activity_unix_ms)}", "{last_activity}" }
        }
    }
}

#[component]
fn RecordMetric(label: &'static str, value: String, detail: String) -> Element {
    rsx! {
        div { class: "record-metric",
            span { "{label}" }
            strong { "{value}" }
            small { "{detail}" }
        }
    }
}

#[component]
fn TrafficRecordMetric(label: &'static str, upload: String, download: String) -> Element {
    rsx! {
        div { class: "record-metric traffic-metric",
            span { "{label}" }
            div { class: "record-traffic-values",
                strong { class: "upload", ArrowUp { size: 12 } "{upload}" }
                small { class: "download", ArrowDown { size: 12 } "{download}" }
            }
        }
    }
}

#[component]
fn HostStateBadge(status: HostRuntimeStatus, language: Language) -> Element {
    let (class, label) = match status {
        HostRuntimeStatus::Connecting => (
            "state-badge connecting",
            tr(language, "Connecting", "连接中"),
        ),
        HostRuntimeStatus::Healthy => ("state-badge healthy", tr(language, "Healthy", "健康")),
        HostRuntimeStatus::Degraded => {
            ("state-badge degraded", tr(language, "Degraded", "质量下降"))
        }
        HostRuntimeStatus::Offline => ("state-badge offline", tr(language, "Offline", "离线")),
    };
    rsx! { span { class, "{label}" } }
}

#[component]
fn SessionStateBadge(status: SshSessionRuntimeStatus, language: Language) -> Element {
    let (class, label) = match status {
        SshSessionRuntimeStatus::Connecting => (
            "state-badge connecting",
            tr(language, "Connecting", "连接中"),
        ),
        SshSessionRuntimeStatus::Healthy => {
            ("state-badge healthy", tr(language, "Healthy", "健康"))
        }
        SshSessionRuntimeStatus::Suspect => {
            ("state-badge degraded", tr(language, "Suspect", "可疑"))
        }
        SshSessionRuntimeStatus::Draining => {
            ("state-badge draining", tr(language, "Draining", "排空中"))
        }
        SshSessionRuntimeStatus::Offline => {
            ("state-badge offline", tr(language, "Offline", "离线"))
        }
    };
    rsx! { span { class, "{label}" } }
}

#[component]
fn TunnelStateBadge(status: TunnelRuntimeStatus, language: Language) -> Element {
    let (class, label) = match status {
        TunnelRuntimeStatus::Starting => {
            ("state-badge connecting", tr(language, "Starting", "启动中"))
        }
        TunnelRuntimeStatus::Listening => {
            ("state-badge healthy", tr(language, "Listening", "监听中"))
        }
        TunnelRuntimeStatus::Error => (
            "state-badge degraded",
            tr(language, "Listen failed", "监听失败"),
        ),
        TunnelRuntimeStatus::Stopped => ("state-badge offline", tr(language, "Stopped", "已停止")),
    };
    rsx! { span { class, "{label}" } }
}

#[component]
fn ConnectionStateBadge(status: ConnectionRuntimeStatus, language: Language) -> Element {
    let (class, label) = match status {
        ConnectionRuntimeStatus::Connecting => (
            "state-badge connecting",
            tr(language, "Connecting", "连接中"),
        ),
        ConnectionRuntimeStatus::Active => ("state-badge healthy", tr(language, "Active", "活动")),
        ConnectionRuntimeStatus::Closed => {
            ("state-badge offline", tr(language, "Closed", "已关闭"))
        }
        ConnectionRuntimeStatus::Error => ("state-badge degraded", tr(language, "Error", "错误")),
    };
    rsx! { span { class, "{label}" } }
}

#[component]
fn EmptyState(title: &'static str, detail: &'static str) -> Element {
    rsx! {
        div { class: "empty-state",
            Network { size: 22 }
            strong { "{title}" }
            span { "{detail}" }
        }
    }
}

#[component]
fn LogsView(entries: Vec<GuiLogEntry>, language: Language) -> Element {
    let mut level_filter = use_signal(LogLevelFilter::default);
    let mut search = use_signal(String::new);
    let selected_filter = *level_filter.read();
    let query = search.read().trim().to_ascii_lowercase();
    let visible_entries = entries
        .into_iter()
        .filter(|entry| selected_filter.allows(entry.level))
        .filter(|entry| {
            query.is_empty()
                || entry.message.to_ascii_lowercase().contains(&query)
                || entry.target.to_ascii_lowercase().contains(&query)
                || entry.fields.to_ascii_lowercase().contains(&query)
        })
        .collect::<Vec<_>>();
    let log_path = super::logging::log_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());

    rsx! {
        section { class: "logs-page",
            div { class: "log-toolbar",
                div { class: "log-file-identity",
                    ScrollText { size: 18 }
                    div {
                        strong { {tr(language, "Application log", "应用日志")} }
                        span { title: "{log_path}", "{log_path}" }
                    }
                }
                div { class: "log-actions",
                    label { class: "log-search",
                        Search { size: 15 }
                        input {
                            aria_label: tr(language, "Search logs", "搜索日志"),
                            placeholder: tr(language, "Search", "搜索"),
                            value: "{search}",
                            oninput: move |event| search.set(event.value()),
                        }
                    }
                    div { class: "log-level-filter", aria_label: tr(language, "Log level", "日志级别"),
                        LogFilterButton {
                            label: tr(language, "All", "全部"),
                            active: selected_filter == LogLevelFilter::All,
                            onclick: move |_| level_filter.set(LogLevelFilter::All),
                        }
                        LogFilterButton {
                            label: "Info",
                            active: selected_filter == LogLevelFilter::Info,
                            onclick: move |_| level_filter.set(LogLevelFilter::Info),
                        }
                        LogFilterButton {
                            label: tr(language, "Warn", "警告"),
                            active: selected_filter == LogLevelFilter::Warn,
                            onclick: move |_| level_filter.set(LogLevelFilter::Warn),
                        }
                        LogFilterButton {
                            label: tr(language, "Error", "错误"),
                            active: selected_filter == LogLevelFilter::Error,
                            onclick: move |_| level_filter.set(LogLevelFilter::Error),
                        }
                    }
                    button {
                        class: "icon-button",
                        title: tr(language, "Clear logs", "清空日志"),
                        aria_label: tr(language, "Clear logs", "清空日志"),
                        onclick: move |_| super::logging::clear(),
                        Trash2 { size: 16 }
                    }
                }
            }
            if visible_entries.is_empty() {
                div { class: "log-empty",
                    ScrollText { size: 22 }
                    strong { {tr(language, "No matching logs", "没有匹配的日志")} }
                }
            } else {
                div { class: "log-list",
                    for entry in visible_entries {
                        LogEntryRow { entry }
                    }
                }
            }
        }
    }
}

#[component]
fn LogFilterButton(
    label: &'static str,
    active: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        button {
            class: if active { "active" } else { "" },
            onclick: move |event| onclick.call(event),
            "{label}"
        }
    }
}

#[component]
fn LogEntryRow(entry: GuiLogEntry) -> Element {
    let (class, level) = match entry.level {
        GuiLogLevel::Trace => ("trace", "TRACE"),
        GuiLogLevel::Debug => ("debug", "DEBUG"),
        GuiLogLevel::Info => ("info", "INFO"),
        GuiLogLevel::Warn => ("warn", "WARN"),
        GuiLogLevel::Error => ("error", "ERROR"),
    };
    rsx! {
        article { class: "log-entry {class}",
            time { "{format_timestamp(entry.timestamp_unix_ms)}" }
            span { class: "log-level", "{level}" }
            div { class: "log-content",
                strong { "{entry.message}" }
                if !entry.fields.is_empty() {
                    code { "{entry.fields}" }
                }
                span { "{entry.target}" }
            }
        }
    }
}

#[component]
fn ConfigurationView(
    config_path: PathBuf,
    mut editor: Signal<ConfigEditorState>,
    mut auto_start: Signal<AutoStartUiState>,
    language: Language,
) -> Element {
    let state = editor.read().clone();
    let auto_start_state = auto_start.read().clone();
    let dirty = state.dirty();
    let format = ConfigFormat::from_path(&config_path)
        .map(|format| format.to_string())
        .unwrap_or_else(|_| tr(language, "Unknown", "未知").to_string());
    let validate_path = config_path.clone();
    let save_path = config_path.clone();
    let reload_path = config_path.clone();
    let file_name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config")
        .to_string();

    rsx! {
        section { class: "configuration-page",
            section { class: "application-settings",
                div { class: "application-setting-copy",
                    span { class: "eyebrow", {tr(language, "Application", "应用")} }
                    strong { {tr(language, "Launch at login", "登录时自动启动")} }
                    span {
                        {tr(
                            language,
                            "User startup item",
                            "当前用户的系统启动项",
                        )}
                    }
                    if let Some(error) = auto_start_state.error.as_deref() {
                        span { class: "application-setting-error", title: "{error}",
                            {match language {
                                Language::English => format!("Update failed: {error}"),
                                Language::Chinese => format!("更新失败：{error}"),
                            }}
                        }
                    }
                }
                div { class: "application-setting-action",
                    span {
                        class: if auto_start_state.enabled { "setting-status enabled" } else { "setting-status" },
                        {if auto_start_state.enabled {
                            tr(language, "Enabled", "已开启")
                        } else {
                            tr(language, "Disabled", "已关闭")
                        }}
                    }
                    label {
                        class: if auto_start_state.supported { "toggle-switch" } else { "toggle-switch disabled" },
                        title: tr(language, "Launch at login", "登录时自动启动"),
                        input {
                            r#type: "checkbox",
                            checked: auto_start_state.enabled,
                            disabled: !auto_start_state.supported,
                            aria_label: tr(language, "Launch at login", "登录时自动启动"),
                            onchange: move |event| {
                                let requested = event.checked();
                                let previous = auto_start.read().clone();
                                match super::autostart::set_enabled(requested) {
                                    Ok(enabled) => {
                                        info!(enabled, "GUI automatic startup updated");
                                        auto_start.set(AutoStartUiState {
                                            enabled,
                                            supported: previous.supported,
                                            error: None,
                                        });
                                    }
                                    Err(error) => {
                                        warn!(
                                            enabled = requested,
                                            error = %format_args!("{error:#}"),
                                            "failed to update GUI automatic startup"
                                        );
                                        auto_start.set(AutoStartUiState {
                                            error: Some(format!("{error:#}")),
                                            ..previous
                                        });
                                    }
                                }
                            },
                        }
                        span { class: "toggle-track" }
                    }
                }
            }
            div { class: "editor-toolbar",
                div { class: "file-identity",
                    FileText { size: 18 }
                    div {
                        strong { "{file_name}" }
                        span { "{format}" }
                    }
                }
                div { class: "editor-state",
                    if dirty {
                        span { class: "dirty-indicator", {tr(language, "Unsaved changes", "有未保存的修改")} }
                    } else {
                        span { class: "saved-indicator", CircleCheck { size: 14 } {tr(language, "Saved", "已保存")} }
                    }
                }
                div { class: "editor-actions",
                    button {
                        class: "button secondary",
                        onclick: move |_| {
                            let text = editor.read().text.clone();
                            let notice = match parse_configuration(&validate_path, &text) {
                                Ok(_) => EditorNotice::success(tr(language, "Configuration is valid", "配置有效")),
                                Err(error) => EditorNotice::error(error),
                            };
                            editor.write().notice = Some(notice);
                        },
                        CircleCheck { size: 16 }
                        {tr(language, "Validate", "校验")}
                    }
                    button {
                        class: "button secondary",
                        title: tr(
                            language,
                            "Discard editor changes and reload from disk",
                            "放弃编辑器修改并从磁盘重新加载",
                        ),
                        onclick: move |_| {
                            editor.set(ConfigEditorState::load(&reload_path, language));
                            super::request_gui_reload();
                        },
                        RefreshCw { size: 16 }
                        {tr(language, "Reload", "重载")}
                    }
                    button {
                        class: "button primary",
                        disabled: !dirty,
                        onclick: move |_| {
                            let text = editor.read().text.clone();
                            match save_configuration(&save_path, &text) {
                                Ok(config) => {
                                    let mut state = editor.write();
                                    state.saved_text = text;
                                    state.disk_config = Some(config);
                                    state.notice = Some(EditorNotice::success(
                                        tr(language, "Configuration saved to disk", "配置已保存到磁盘"),
                                    ));
                                }
                                Err(error) => {
                                    editor.write().notice = Some(EditorNotice::error(error));
                                }
                            }
                        },
                        Save { size: 16 }
                        {tr(language, "Save", "保存")}
                    }
                }
            }

            if let Some(notice) = state.notice {
                div {
                    class: match notice.kind {
                        EditorNoticeKind::Success => "editor-notice success",
                        EditorNoticeKind::Info => "editor-notice info",
                        EditorNoticeKind::Error => "editor-notice error",
                    },
                    if notice.kind == EditorNoticeKind::Error {
                        CircleAlert { size: 16 }
                    } else {
                        CircleCheck { size: 16 }
                    }
                    span { "{notice.message}" }
                }
            }

            textarea {
                class: "config-editor",
                aria_label: tr(language, "Configuration editor", "配置编辑器"),
                spellcheck: "false",
                value: "{state.text}",
                oninput: move |event| {
                    let mut state = editor.write();
                    state.text = event.value();
                    state.notice = None;
                },
            }
        }
    }
}

fn append_runtime_events(
    events: &mut Signal<VecDeque<RuntimeEvent>>,
    previous: &RuntimeSnapshot,
    next: &RuntimeSnapshot,
) {
    if previous.running != next.running {
        let (kind, message) = if next.running {
            (
                RuntimeEventKind::Success,
                RuntimeEventMessage::RuntimeStarted,
            )
        } else {
            (RuntimeEventKind::Error, RuntimeEventMessage::RuntimeStopped)
        };
        push_runtime_event(events, kind, message);
    }
    if next.config_generation > previous.config_generation {
        push_runtime_event(
            events,
            RuntimeEventKind::Success,
            RuntimeEventMessage::ConfigurationApplied(next.config_generation),
        );
    }
    if next.ssh_sessions_active != previous.ssh_sessions_active {
        push_runtime_event(
            events,
            RuntimeEventKind::Info,
            RuntimeEventMessage::ActiveSessions(next.ssh_sessions_active),
        );
    }
}

fn push_runtime_event(
    events: &mut Signal<VecDeque<RuntimeEvent>>,
    kind: RuntimeEventKind,
    message: RuntimeEventMessage,
) {
    let mut events = events.write();
    events.push_front(RuntimeEvent {
        recorded_at: Instant::now(),
        kind,
        message,
    });
    while events.len() > EVENT_LIMIT {
        events.pop_back();
    }
}

fn observe_configuration_change(
    path: &Path,
    editor: &mut Signal<ConfigEditorState>,
    language: Language,
) {
    let Ok(disk_text) = fs::read_to_string(path) else {
        return;
    };
    let current = editor.read().clone();
    if disk_text == current.saved_text {
        return;
    }
    if current.dirty() {
        editor.write().notice = Some(EditorNotice::info(tr(
            language,
            "Configuration changed on disk; reload to inspect it",
            "磁盘上的配置已更改，请重载后查看",
        )));
    } else {
        editor.set(ConfigEditorState::load(path, language));
    }
}

fn parse_configuration(path: &Path, text: &str) -> Result<AppConfig, String> {
    let format = ConfigFormat::from_path(path).map_err(|error| error.to_string())?;
    let config = AppConfig::from_str(text, format).map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())?;
    Ok(config)
}

fn save_configuration(path: &Path, text: &str) -> Result<AppConfig, String> {
    let config = parse_configuration(path, text)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Failed to create configuration directory {}: {error}",
            parent.display()
        )
    })?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))
        .map_err(|error| error.to_string())?;
    temporary
        .write_all(text.as_bytes())
        .context("failed to write the temporary configuration file")
        .map_err(|error| error.to_string())?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("failed to sync the temporary configuration file")
        .map_err(|error| error.to_string())?;
    temporary
        .persist(path)
        .map_err(|error| format!("Failed to replace {}: {}", path.display(), error.error))?;
    Ok(config)
}

fn session_status_order(status: SshSessionRuntimeStatus) -> u8 {
    match status {
        SshSessionRuntimeStatus::Healthy => 0,
        SshSessionRuntimeStatus::Connecting => 1,
        SshSessionRuntimeStatus::Suspect => 2,
        SshSessionRuntimeStatus::Draining => 3,
        SshSessionRuntimeStatus::Offline => 4,
    }
}

fn connection_status_order(status: ConnectionRuntimeStatus) -> u8 {
    match status {
        ConnectionRuntimeStatus::Active => 0,
        ConnectionRuntimeStatus::Connecting => 1,
        ConnectionRuntimeStatus::Error => 2,
        ConnectionRuntimeStatus::Closed => 3,
    }
}

fn tunnel_kind_order(kind: TunnelKind) -> u8 {
    match kind {
        TunnelKind::LocalProxy => 0,
        TunnelKind::LocalForward => 1,
        TunnelKind::RemoteProxy => 2,
        TunnelKind::RemoteForward => 3,
    }
}

fn tunnel_kind_label(kind: TunnelKind, language: Language) -> &'static str {
    match kind {
        TunnelKind::LocalProxy => tr(language, "Local proxy", "本地代理"),
        TunnelKind::LocalForward => tr(language, "Local forward", "本地转发"),
        TunnelKind::RemoteProxy => tr(language, "Remote proxy", "远端代理"),
        TunnelKind::RemoteForward => tr(language, "Remote forward", "远端转发"),
    }
}

fn format_timestamp(unix_ms: u64) -> String {
    let nanos = i128::from(unix_ms).saturating_mul(1_000_000);
    let Ok(timestamp) = OffsetDateTime::from_unix_timestamp_nanos(nanos) else {
        return "-".to_string();
    };
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    timestamp
        .to_offset(offset)
        .format(format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second]"
        ))
        .unwrap_or_else(|_| "-".to_string())
}

fn format_chart_timestamp(unix_ms: u64) -> String {
    let nanos = i128::from(unix_ms).saturating_mul(1_000_000);
    let Ok(timestamp) = OffsetDateTime::from_unix_timestamp_nanos(nanos) else {
        return "-".to_string();
    };
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    timestamp
        .to_offset(offset)
        .format(format_description!("[hour]:[minute]"))
        .unwrap_or_else(|_| "-".to_string())
}

fn format_age_from_unix_ms(unix_ms: u64, language: Language) -> String {
    let age_ms = current_unix_ms().saturating_sub(unix_ms);
    if age_ms < 5_000 {
        tr(language, "now", "刚刚").to_string()
    } else if age_ms < 60_000 {
        match language {
            Language::English => format!("{}s ago", age_ms / 1_000),
            Language::Chinese => format!("{} 秒前", age_ms / 1_000),
        }
    } else if age_ms < 3_600_000 {
        match language {
            Language::English => format!("{}m ago", age_ms / 60_000),
            Language::Chinese => format!("{} 分钟前", age_ms / 60_000),
        }
    } else if age_ms < 86_400_000 {
        match language {
            Language::English => format!("{}h ago", age_ms / 3_600_000),
            Language::Chinese => format!("{} 小时前", age_ms / 3_600_000),
        }
    } else {
        match language {
            Language::English => format!("{}d ago", age_ms / 86_400_000),
            Language::Chinese => format!("{} 天前", age_ms / 86_400_000),
        }
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn snapshot_throughput(snapshot: &RuntimeSnapshot) -> ThroughputPoint {
    ThroughputPoint {
        upload_bps: snapshot.upload_bps as f64,
        download_bps: snapshot.download_bps as f64,
    }
}

fn recent_traffic_history(history: &[TrafficHistoryPoint]) -> &[TrafficHistoryPoint] {
    let start = history.len().saturating_sub(TRAFFIC_HISTORY_WINDOW_MINUTES);
    &history[start..]
}

fn build_chart_bars(history: &[TrafficHistoryPoint]) -> Vec<TrafficChartBar> {
    let aggregated = aggregate_chart_history(history);
    let max_rate = aggregated.iter().fold(1.0_f64, |maximum, sample| {
        maximum.max(sample.download_bps).max(sample.upload_bps)
    });
    let mut bars =
        vec![TrafficChartBar::default(); TRAFFIC_CHART_BAR_COUNT.saturating_sub(aggregated.len())];
    bars.extend(aggregated.into_iter().map(|sample| TrafficChartBar {
        sample: Some(sample),
        upload_height: chart_height(sample.upload_bps, max_rate),
        download_height: chart_height(sample.download_bps, max_rate),
    }));
    bars
}

fn aggregate_chart_history(history: &[TrafficHistoryPoint]) -> Vec<TrafficChartSample> {
    let mut buckets = history
        .rchunks(TRAFFIC_CHART_BUCKET_MINUTES)
        .take(TRAFFIC_CHART_BAR_COUNT)
        .map(|samples| {
            let duration_ms = samples.iter().map(|sample| sample.duration_ms).sum::<u64>();
            let uploaded_bytes = samples
                .iter()
                .map(|sample| sample.uploaded_bytes)
                .sum::<u64>();
            let downloaded_bytes = samples
                .iter()
                .map(|sample| sample.downloaded_bytes)
                .sum::<u64>();
            TrafficChartSample {
                started_at_unix_ms: samples
                    .first()
                    .map(|sample| sample.started_at_unix_ms)
                    .unwrap_or_default(),
                upload_bps: window_bytes_per_second(uploaded_bytes, duration_ms),
                download_bps: window_bytes_per_second(downloaded_bytes, duration_ms),
            }
        })
        .collect::<Vec<_>>();
    buckets.reverse();
    buckets
}

fn window_bytes_per_second(bytes: u64, duration_ms: u64) -> f64 {
    if duration_ms == 0 {
        0.0
    } else {
        bytes as f64 / (duration_ms as f64 / 1_000.0)
    }
}

fn chart_height(rate: f64, maximum: f64) -> f64 {
    if rate <= 0.0 {
        0.0
    } else {
        ((rate / maximum) * 100.0).clamp(3.0, 100.0)
    }
}

fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second))
}

fn tray_throughput_title(upload_bps: f64, download_bps: f64) -> String {
    format!(
        "{}\n{}",
        format_tray_rate(upload_bps),
        format_tray_rate(download_bps)
    )
}

fn format_tray_rate(bytes_per_second: f64) -> String {
    const UNITS: [&str; 7] = ["B/s", "K/s", "M/s", "G/s", "T/s", "P/s", "E/s"];
    const PAD_SPACE: char = ' ';

    let mut value = bytes_per_second.max(0.0);
    let mut unit = 0;
    while value >= 999.5 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == UNITS.len() - 1 {
        value = value.min(999.0);
    }

    let number = if unit > 0 && value < 9.95 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    };
    let padding = 3_usize.saturating_sub(number.chars().count());
    format!(
        "{}{number}{}",
        PAD_SPACE.to_string().repeat(padding),
        UNITS[unit]
    )
}

fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn format_duration(milliseconds: u64, language: Language) -> String {
    let seconds = milliseconds / 1000;
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        match language {
            Language::English => format!("{days}d {hours}h"),
            Language::Chinese => format!("{days}天 {hours}小时"),
        }
    } else if hours > 0 {
        match language {
            Language::English => format!("{hours}h {minutes}m"),
            Language::Chinese => format!("{hours}小时 {minutes}分钟"),
        }
    } else {
        match language {
            Language::English => format!("{minutes}m {}s", seconds % 60),
            Language::Chinese => format!("{minutes}分钟 {}秒", seconds % 60),
        }
    }
}

fn format_event_age(recorded_at: Instant, language: Language) -> String {
    let seconds = recorded_at.elapsed().as_secs();
    if seconds < 5 {
        tr(language, "now", "刚刚").to_string()
    } else if seconds < 60 {
        match language {
            Language::English => format!("{seconds}s"),
            Language::Chinese => format!("{seconds}秒"),
        }
    } else {
        match language {
            Language::English => format!("{}m", seconds / 60),
            Language::Chinese => format!("{}分钟", seconds / 60),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traffic_point(upload_bps: u64, download_bps: u64) -> TrafficHistoryPoint {
        TrafficHistoryPoint {
            started_at_unix_ms: 0,
            duration_ms: 60_000,
            upload_bps,
            download_bps,
            uploaded_bytes: upload_bps * 60,
            downloaded_bytes: download_bps * 60,
        }
    }

    #[test]
    fn byte_values_use_compact_binary_units() {
        assert_eq!(format_bytes(0.0), "0 B");
        assert_eq!(format_bytes(1024.0), "1.00 KB");
        assert_eq!(format_bytes(12.5 * 1024.0), "12.5 KB");
        assert_eq!(format_bytes(150.0 * 1024.0 * 1024.0), "150 MB");
    }

    #[test]
    fn tray_title_places_upload_above_download_without_labels() {
        assert_eq!(
            tray_throughput_title(1024.0, 12.0 * 1024.0),
            "1.0K/s\n 12K/s"
        );
    }

    #[test]
    fn tray_rate_uses_a_stable_three_character_number_field() {
        assert_eq!(format_tray_rate(0.0), "  0B/s");
        assert_eq!(format_tray_rate(1024.0), "1.0K/s");
        assert_eq!(format_tray_rate(12.0 * 1024.0), " 12K/s");
        assert_eq!(format_tray_rate(123.0 * 1024.0 * 1024.0), "123M/s");
    }

    #[test]
    fn chart_history_is_padded_to_a_stable_width() {
        let bars = build_chart_bars(&[traffic_point(50, 100)]);
        assert_eq!(bars.len(), TRAFFIC_CHART_BAR_COUNT);
        assert_eq!(
            bars.last(),
            Some(&TrafficChartBar {
                sample: Some(TrafficChartSample {
                    started_at_unix_ms: 0,
                    upload_bps: 50.0,
                    download_bps: 100.0,
                }),
                upload_height: 50.0,
                download_height: 100.0,
            })
        );
    }

    #[test]
    fn chart_history_averages_two_one_minute_buckets() {
        let history = vec![traffic_point(10, 20); TRAFFIC_CHART_BUCKET_MINUTES];

        assert_eq!(
            aggregate_chart_history(&history),
            vec![TrafficChartSample {
                started_at_unix_ms: 0,
                upload_bps: 10.0,
                download_bps: 20.0,
            }]
        );
    }

    #[test]
    fn overview_history_keeps_only_the_latest_hour() {
        let history = vec![traffic_point(10, 20); 90];
        assert_eq!(recent_traffic_history(&history).len(), 60);
        assert_eq!(build_chart_bars(recent_traffic_history(&history)).len(), 30);
    }

    #[test]
    fn invalid_configuration_is_rejected_before_save() {
        let path = Path::new("config.yaml");
        let error = parse_configuration(path, "hosts: {}\n").unwrap_err();
        assert!(error.contains("at least one SSH host is required"));
    }

    #[test]
    fn saving_configuration_preserves_original_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        fs::write(&path, "hosts: {}\n").unwrap();
        let text = "# preserved comment\nhosts:\n  test:\n    host: 127.0.0.1\n";

        save_configuration(&path, text).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), text);
    }
}
