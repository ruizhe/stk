use anyhow::Context;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use std::{
    io::{self, IsTerminal as _, Write as _},
    path::PathBuf,
    time::Duration,
};
use stk_core::{
    AppConfig, ConfigScope, ControlConfig, ControlEndpoint, RuntimeProfile,
    config::ConfigFormat,
    fetch_runtime_snapshot,
    reload::run_config_file_until_shutdown,
    request_runtime_reload, resolve_config_path,
    stats::{
        ConnectionRuntimeSnapshot, ConnectionRuntimeStatus, HostRuntimeSnapshot, HostRuntimeStatus,
        RuntimeSnapshot, SshSessionRuntimeSnapshot, SshSessionRuntimeStatus, TunnelKind,
        TunnelRuntimeSnapshot, TunnelRuntimeStatus,
    },
    subscribe_runtime_snapshots,
};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(
    name = "stk",
    version,
    about = "Reliable SSH proxies and tunnel management"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Run as a service with the control API; defaults to the system config directory"
    )]
    Serve(ConfigArgs),
    #[command(about = "Run in the foreground; defaults to the user config directory")]
    Run(ConfigArgs),
    #[command(about = "Validate a config; defaults to the user config directory")]
    Check(ConfigArgs),
    #[command(about = "Show the running GUI, foreground, or service runtime status")]
    Status(StatusArgs),
    #[command(about = "Continuously display pushed runtime status like top")]
    Top(ControlArgs),
    #[command(about = "Request the running GUI, foreground, or service runtime to reload")]
    Reload(ControlArgs),
    PrintDefaultConfig(PrintDefaultConfigArgs),
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[arg(
        short,
        long,
        help = "Config file or directory; defaults depend on the command"
    )]
    config: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ControlArgs {
    #[arg(
        short,
        long,
        help = "Config file or directory used to discover the control endpoint"
    )]
    config: Option<PathBuf>,
    #[arg(
        long,
        help = "Use the system config and system control endpoint instead of the user endpoint"
    )]
    system: bool,
    #[arg(
        long,
        value_name = "ENDPOINT",
        help = "Override the configured endpoint: unix:/path, pipe:name, or tcp:address"
    )]
    endpoint: Option<ControlEndpoint>,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[command(flatten)]
    control: ControlArgs,
    #[arg(long, help = "Print the complete status snapshot as JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct PrintDefaultConfigArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Yaml)]
    format: OutputFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Yaml,
    Json,
    Toml,
}

impl From<OutputFormat> for ConfigFormat {
    fn from(value: OutputFormat) -> Self {
        match value {
            OutputFormat::Yaml => Self::Yaml,
            OutputFormat::Json => Self::Json,
            OutputFormat::Toml => Self::Toml,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let Some(command) = Cli::parse().command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    match command {
        Command::Serve(args) => run(args, RuntimeProfile::Service, ConfigScope::System).await,
        Command::Run(args) => run(args, RuntimeProfile::Foreground, ConfigScope::User).await,
        Command::Check(args) => check(args, ConfigScope::User),
        Command::Status(args) => status(args).await,
        Command::Top(args) => top(args).await,
        Command::Reload(args) => reload(args).await,
        Command::PrintDefaultConfig(args) => {
            println!(
                "{}",
                AppConfig::default()
                    .to_string(args.format.into())?
                    .trim_end()
            );
            Ok(())
        }
    }
}

async fn status(args: StatusArgs) -> anyhow::Result<()> {
    let endpoint = resolve_control_endpoint(&args.control)?;
    let snapshot = fetch_runtime_snapshot(&endpoint).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        println!("{}", format_runtime_status(&snapshot));
    }
    Ok(())
}

async fn top(args: ControlArgs) -> anyhow::Result<()> {
    let endpoint = resolve_control_endpoint(&args)?;
    let interactive = io::stdout().is_terminal();
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());
    loop {
        let subscription = subscribe_runtime_snapshots(&endpoint).await;
        let mut subscription = match subscription {
            Ok(subscription) => subscription,
            Err(error) => {
                eprintln!("failed to subscribe to {endpoint}: {error:#}; retrying");
                tokio::select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                }
            }
        };
        loop {
            let snapshot = tokio::select! {
                _ = &mut shutdown => return Ok(()),
                snapshot = subscription.recv() => snapshot,
            };
            match snapshot {
                Ok(Some(snapshot)) => render_top_snapshot(&snapshot, interactive)?,
                Ok(None) => break,
                Err(error) => {
                    eprintln!("status stream from {endpoint} failed: {error:#}; reconnecting");
                    break;
                }
            }
        }
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

fn render_top_snapshot(snapshot: &RuntimeSnapshot, interactive: bool) -> anyhow::Result<()> {
    let mut output = io::stdout().lock();
    if interactive {
        write!(output, "\x1b[2J\x1b[H")?;
    } else {
        writeln!(output, "---")?;
    }
    writeln!(output, "{}", format_runtime_status(snapshot))?;
    output.flush()?;
    Ok(())
}

fn format_runtime_status(snapshot: &RuntimeSnapshot) -> String {
    let mut output = Vec::new();
    output.push("SSH Tunnel Keeper".to_string());
    output.push(format!(
        "  State: {}    Uptime: {}",
        if snapshot.running {
            "running"
        } else {
            "stopped"
        },
        format_optional_duration(snapshot.uptime_ms)
    ));
    output.push(format!(
        "  Config: generation {}    reloads {}    reload errors {}",
        snapshot.config_generation,
        snapshot.config_reloads_total,
        snapshot.config_reload_errors_total
    ));
    output.push(format!(
        "  Hosts: {}    SSH sessions: {} active / {} created",
        snapshot.configured_hosts, snapshot.ssh_sessions_active, snapshot.ssh_sessions_total
    ));
    output.push(format!(
        "  Connections: {} active / {} accepted",
        snapshot.local_connections_active, snapshot.local_connections_total
    ));
    let captured_connections = snapshot
        .hosts
        .iter()
        .map(|host| host.connections.len())
        .sum::<usize>();
    output.push(format!(
        "  Connection capture: {}    auto-clear {}    {} captured",
        if snapshot.connection_capture.recording {
            "recording"
        } else {
            "stopped"
        },
        if snapshot.connection_capture.auto_clear_closed {
            "on"
        } else {
            "off"
        },
        captured_connections
    ));
    output.push(format!(
        "  Speed: {}",
        format_speed(snapshot.upload_bps, snapshot.download_bps)
    ));
    output.push(format!(
        "  Traffic: upload {}    download {}    total {}",
        format_bytes(snapshot.uploaded_bytes_total),
        format_bytes(snapshot.downloaded_bytes_total),
        format_bytes(snapshot.transferred_bytes_total)
    ));
    output.push(format!(
        "  SSH channel open: {}",
        format_channel_latency(snapshot)
    ));
    output.push(format!("  Errors: {}", snapshot.errors_total));

    if snapshot.hosts.is_empty() {
        output.push(String::new());
        output.push("Hosts: none".to_string());
        return output.join("\n");
    }

    output.push(String::new());
    output.push(format!("Hosts ({})", snapshot.hosts.len()));
    for host in &snapshot.hosts {
        append_host_status(&mut output, host);
    }
    output.join("\n")
}

fn append_host_status(output: &mut Vec<String>, host: &HostRuntimeSnapshot) {
    output.push(format!(
        "  {} [{}]",
        host.name,
        host_status_label(host.status)
    ));
    output.push(format!(
        "    SSH: {} -> {}    RTT: {}",
        host.ssh_alias,
        host.address,
        format_optional_millis(host.rtt_ms)
    ));
    output.push(format!(
        "    Pool: {}-{} sessions    active {} / {} retained",
        host.min_sessions,
        host.max_sessions,
        host.sessions
            .iter()
            .filter(|session| session.status != SshSessionRuntimeStatus::Offline)
            .count(),
        host.sessions.len()
    ));
    output.push(format!(
        "    Connections: {} active / {} total    restarts {}    errors {}",
        host.connections_active, host.connections_total, host.restart_count, host.errors_total
    ));
    output.push(format!(
        "    Speed: {}",
        format_speed(host.upload_bps, host.download_bps)
    ));
    output.push(format!(
        "    Traffic: upload {}    download {}",
        format_bytes(host.uploaded_bytes_total),
        format_bytes(host.downloaded_bytes_total)
    ));
    if let Some(error) = &host.last_error {
        output.push(format!("    Last error: {error}"));
    }

    if !host.sessions.is_empty() {
        output.push("    Sessions".to_string());
        for session in &host.sessions {
            append_session_status(output, session);
        }
    }
    if !host.tunnels.is_empty() {
        output.push("    Tunnels".to_string());
        for tunnel in &host.tunnels {
            append_tunnel_status(output, tunnel);
        }
    }
    if !host.connections.is_empty() {
        output.push("    Connections".to_string());
        for connection in &host.connections {
            append_connection_status(output, connection);
        }
    }
}

fn append_session_status(output: &mut Vec<String>, session: &SshSessionRuntimeSnapshot) {
    let mut flags = Vec::new();
    if session.remote_forward_owner {
        flags.push("remote-forward owner");
    }
    if session.retiring {
        flags.push("retiring");
    }
    let flags = if flags.is_empty() {
        String::new()
    } else {
        format!("    {}", flags.join(", "))
    };
    output.push(format!(
        "      #{} [{}]    uptime {}    RTT {}{}",
        session.id,
        session_status_label(session.status),
        format_optional_duration(session.uptime_ms),
        format_optional_millis(session.rtt_ms),
        flags
    ));
    output.push(format!(
        "        Startup: {}    channels {} active / {} total    channel errors {}",
        format_optional_decimal_millis(session.startup_ms),
        session.active_channels,
        session.channels_total,
        session.channel_open_errors_total
    ));
    output.push(format!(
        "        Speed: {}",
        format_speed(session.upload_bps, session.download_bps)
    ));
    output.push(format!(
        "        Traffic: upload {}    download {}",
        format_bytes(session.uploaded_bytes_total),
        format_bytes(session.downloaded_bytes_total)
    ));
    if let Some(error) = &session.last_error {
        output.push(format!("        Last error: {error}"));
    }
}

fn append_tunnel_status(output: &mut Vec<String>, tunnel: &TunnelRuntimeSnapshot) {
    output.push(format!(
        "      {} {} [{}]",
        tunnel_kind_label(tunnel.kind),
        tunnel.name,
        tunnel_status_label(tunnel.status)
    ));
    let destination = tunnel
        .target
        .as_deref()
        .map(|target| format!("target {target}"))
        .or_else(|| {
            tunnel
                .protocol
                .as_deref()
                .map(|protocol| format!("protocol {protocol}"))
        })
        .unwrap_or_else(|| "dynamic target".to_string());
    output.push(format!(
        "        Listen: {}    {}",
        tunnel.listen, destination
    ));
    output.push(format!(
        "        Connections: {} active / {} total    errors {}",
        tunnel.connections_active, tunnel.connections_total, tunnel.errors_total
    ));
    output.push(format!(
        "        Speed: {}",
        format_speed(tunnel.upload_bps, tunnel.download_bps)
    ));
    output.push(format!(
        "        Traffic: upload {}    download {}",
        format_bytes(tunnel.uploaded_bytes_total),
        format_bytes(tunnel.downloaded_bytes_total)
    ));
    if let Some(owner) = tunnel.owner_session_id {
        output.push(format!("        Owner session: #{owner}"));
    }
    if let Some(error) = &tunnel.last_error {
        output.push(format!("        Last error: {error}"));
    }
}

fn append_connection_status(output: &mut Vec<String>, connection: &ConnectionRuntimeSnapshot) {
    let target = connection.target.as_deref().unwrap_or("-");
    let protocol = connection.protocol.as_deref().unwrap_or("-");
    let session = connection
        .session_id
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| "-".to_string());
    output.push(format!(
        "      #{} [{}]    {} -> {}",
        connection.id,
        connection_status_label(connection.status),
        connection.peer_address,
        target
    ));
    output.push(format!(
        "        Tunnel: {}    protocol {}    session {}    uptime {}",
        connection.tunnel_id,
        protocol,
        session,
        format_duration(connection.uptime_ms)
    ));
    output.push(format!(
        "        Speed: {}",
        format_speed(connection.upload_bps, connection.download_bps)
    ));
    output.push(format!(
        "        Traffic: upload {}    download {}    errors {}",
        format_bytes(connection.uploaded_bytes_total),
        format_bytes(connection.downloaded_bytes_total),
        connection.errors_total
    ));
    if let Some(error) = &connection.last_error {
        output.push(format!("        Last error: {error}"));
    }
}

fn format_channel_latency(snapshot: &RuntimeSnapshot) -> String {
    let latency = &snapshot.ssh_channel_open;
    if latency.samples == 0 {
        return "no samples".to_string();
    }
    format!(
        "average {}    max {}    {} samples",
        format_optional_decimal_millis(latency.average_ms),
        format_optional_decimal_millis(latency.max_ms),
        latency.samples
    )
}

fn format_optional_duration(milliseconds: Option<u64>) -> String {
    milliseconds.map_or_else(|| "-".to_string(), format_duration)
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds} ms");
    }
    let total_seconds = milliseconds / 1_000;
    let days = total_seconds / 86_400;
    let hours = total_seconds % 86_400 / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_optional_millis(milliseconds: Option<u64>) -> String {
    milliseconds.map_or_else(|| "-".to_string(), |value| format!("{value} ms"))
}

fn format_optional_decimal_millis(milliseconds: Option<f64>) -> String {
    milliseconds.map_or_else(|| "-".to_string(), |value| format!("{value:.1} ms"))
}

fn format_speed(upload_bps: u64, download_bps: u64) -> String {
    format!(
        "upload {}    download {}",
        format_bytes_per_second(upload_bps),
        format_bytes_per_second(download_bps)
    )
}

fn format_bytes_per_second(bytes_per_second: u64) -> String {
    const UNITS: [&str; 6] = ["B/s", "KiB/s", "MiB/s", "GiB/s", "TiB/s", "PiB/s"];
    let mut value = bytes_per_second as f64;
    let mut unit = 0_usize;
    while value >= 1_024.0 && unit < UNITS.len() - 1 {
        value /= 1_024.0;
        unit += 1;
    }
    if unit == 0 || value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1_024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0_usize;
    while value >= 1_024.0 && unit < UNITS.len() - 1 {
        value /= 1_024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn host_status_label(status: HostRuntimeStatus) -> &'static str {
    match status {
        HostRuntimeStatus::Connecting => "connecting",
        HostRuntimeStatus::Healthy => "healthy",
        HostRuntimeStatus::Degraded => "degraded",
        HostRuntimeStatus::Offline => "offline",
    }
}

fn session_status_label(status: SshSessionRuntimeStatus) -> &'static str {
    match status {
        SshSessionRuntimeStatus::Connecting => "connecting",
        SshSessionRuntimeStatus::Healthy => "healthy",
        SshSessionRuntimeStatus::Suspect => "suspect",
        SshSessionRuntimeStatus::Draining => "draining",
        SshSessionRuntimeStatus::Offline => "offline",
    }
}

fn tunnel_status_label(status: TunnelRuntimeStatus) -> &'static str {
    match status {
        TunnelRuntimeStatus::Starting => "starting",
        TunnelRuntimeStatus::Listening => "listening",
        TunnelRuntimeStatus::Error => "listen-failed",
        TunnelRuntimeStatus::Stopped => "stopped",
    }
}

fn tunnel_kind_label(kind: TunnelKind) -> &'static str {
    match kind {
        TunnelKind::LocalProxy => "local-proxy",
        TunnelKind::LocalForward => "local-forward",
        TunnelKind::RemoteProxy => "remote-proxy",
        TunnelKind::RemoteForward => "remote-forward",
    }
}

fn connection_status_label(status: ConnectionRuntimeStatus) -> &'static str {
    match status {
        ConnectionRuntimeStatus::Connecting => "connecting",
        ConnectionRuntimeStatus::Active => "active",
        ConnectionRuntimeStatus::Closed => "closed",
        ConnectionRuntimeStatus::Error => "error",
    }
}

async fn reload(args: ControlArgs) -> anyhow::Result<()> {
    let endpoint = resolve_control_endpoint(&args)?;
    request_runtime_reload(&endpoint).await?;
    println!("reload requested");
    Ok(())
}

fn resolve_control_endpoint(args: &ControlArgs) -> anyhow::Result<ControlEndpoint> {
    if let Some(endpoint) = &args.endpoint {
        return Ok(endpoint.clone());
    }
    let scope = if args.system {
        ConfigScope::System
    } else {
        ConfigScope::User
    };
    let path = resolve_config_path(args.config.as_deref(), scope);
    let control = if path.is_file() {
        load_config(path)?.control
    } else {
        ControlConfig::default()
    };
    ControlEndpoint::from_config(&control, scope)
}

async fn run(args: ConfigArgs, profile: RuntimeProfile, scope: ConfigScope) -> anyhow::Result<()> {
    let config = resolve_config_path(args.config.as_deref(), scope);
    info!(?profile, config = %config.display(), "starting SSH Tunnel Keeper runtime with config reload");
    run_config_file_until_shutdown(config, profile, shutdown_signal()).await
}

fn check(args: ConfigArgs, scope: ConfigScope) -> anyhow::Result<()> {
    let path = resolve_config_path(args.config.as_deref(), scope);
    let config = load_config(path)?;
    config.validate()?;
    println!("config ok");
    Ok(())
}

fn load_config(path: PathBuf) -> anyhow::Result<AppConfig> {
    AppConfig::from_path(&path).with_context(|| format!("failed to load {}", path.display()))
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for shutdown signal: {error}");
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_selects_help_instead_of_run() {
        let cli = Cli::try_parse_from(["stk"]).unwrap();
        assert!(cli.command.is_none());
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("Commands:"));
        assert!(help.contains("status"));
        assert!(help.contains("top"));
        assert!(help.contains("run"));
    }

    #[test]
    fn top_subcommand_uses_control_arguments() {
        let cli = Cli::try_parse_from(["stk", "top", "--endpoint", "tcp:127.0.0.1:19090"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Top(_))));
    }

    #[test]
    fn human_status_is_not_raw_json() {
        let output = format_runtime_status(&stk_core::stats::runtime_snapshot());
        assert!(output.starts_with("SSH Tunnel Keeper\n"));
        assert!(output.contains("State: stopped"));
        assert!(output.contains("Speed: upload 0 B/s    download 0 B/s"));
        assert!(output.contains("Hosts: none"));
        assert!(!output.trim_start().starts_with('{'));
    }

    #[test]
    fn current_speed_comes_from_the_runtime_snapshot() {
        let mut current = stk_core::stats::runtime_snapshot();
        current.upload_bps = 1_024;
        current.download_bps = 2_048;

        let output = format_runtime_status(&current);
        assert!(output.contains("Speed: upload 1.0 KiB/s    download 2.0 KiB/s"));
    }

    #[test]
    fn sizes_and_durations_are_compact() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        assert_eq!(format_bytes_per_second(1_536), "1.5 KiB/s");
        assert_eq!(format_duration(999), "999 ms");
        assert_eq!(format_duration(65_000), "1m 5s");
        assert_eq!(format_duration(3_660_000), "1h 1m");
    }
}
