use crate::config::{
    HostPort, ProxyProtocol, ResolvedForwardConfig, ResolvedHostConfig, ResolvedProxyConfig,
    SshAuthConfig, SshHostConfig, SshHostKeyPolicy, SshPoolConfig,
};
use anyhow::{Context, bail};
use glob::glob;
use ssh2_config::{DefaultAlgorithms, HostClause, HostParams, ParseRule, SshConfig};
use std::{
    collections::HashSet,
    env,
    fs::{self, File},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

const DEFAULT_SSH_PORT: u16 = 22;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_KEEP_ALIVE_SECS: u64 = 30;
const DEFAULT_KEEP_ALIVE_MAX: usize = 3;
const MAX_PROXY_JUMPS: usize = 8;
const MAX_INCLUDE_DEPTH: usize = 16;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedSshPlan {
    pub target: ResolvedSshEndpoint,
    pub jumps: Vec<ResolvedSshEndpoint>,
    pub config_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedSshEndpoint {
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: ResolvedSshAuth,
    pub host_key_policy: ResolvedHostKeyPolicy,
    pub host_key_name: String,
    pub known_hosts_paths: Vec<PathBuf>,
    pub connect_timeout: Duration,
    pub keep_alive: Duration,
    pub keep_alive_max: usize,
    pub tcp_keep_alive: bool,
    pub proxy_command: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedSshAuth {
    pub explicit: Option<SshAuthConfig>,
    pub identity_files: Vec<PathBuf>,
    pub use_agent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedHostKeyPolicy {
    KnownHosts,
    AcceptNew,
    InsecureAcceptAny,
}

struct LoadedSshConfig {
    config: SshConfig,
    path: Option<PathBuf>,
}

struct ResolvedDestination {
    endpoint: ResolvedSshEndpoint,
    proxy_jump: Vec<String>,
}

#[derive(Debug)]
struct DestinationSpec {
    alias: String,
    username: Option<String>,
    port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedSshForward {
    LocalProxy {
        listen: SocketAddr,
    },
    LocalForward {
        listen: SocketAddr,
        target: HostPort,
    },
    RemoteProxy {
        listen: SocketAddr,
    },
    RemoteForward {
        listen: SocketAddr,
        target: HostPort,
    },
}

struct ForwardScanState {
    active: bool,
    clear_all_forwardings: Option<bool>,
    forwards: Vec<ParsedSshForward>,
}

pub(crate) fn inherit_ssh_config_forwards(host: &mut ResolvedHostConfig) -> anyhow::Result<()> {
    if !host.inherit_ssh_config_forwards {
        return Ok(());
    }
    let Some(alias) = host.ssh_config_host.as_deref() else {
        return Ok(());
    };
    let forwards = load_ssh_config_forwards(alias, host.ssh_config_path.as_deref())?;
    merge_ssh_config_forwards(host, forwards);
    Ok(())
}

pub(crate) fn resolve_ssh_plan(
    upstream: &SshHostConfig,
    pool: &SshPoolConfig,
) -> anyhow::Result<ResolvedSshPlan> {
    let loaded = load_ssh_config(upstream)?;
    let config = upstream.ssh_config_host.as_ref().map(|_| &loaded.config);
    let target_spec = DestinationSpec {
        alias: upstream
            .ssh_config_host
            .clone()
            .or_else(|| upstream.host.clone())
            .context("SSH host has neither host nor ssh-config-host")?,
        username: upstream.username.clone(),
        port: upstream.port,
    };
    let target = resolve_destination(&target_spec, config, Some(upstream), pool)?;
    if !target.proxy_jump.is_empty() && target.endpoint.proxy_command.is_some() {
        bail!(
            "SSH host {} configures both ProxyJump and ProxyCommand",
            target.endpoint.alias
        );
    }

    let mut jumps = Vec::new();
    let mut stack = vec![target.endpoint.alias.clone()];
    for jump in &target.proxy_jump {
        resolve_jump(jump, config, pool, &mut stack, &mut jumps)?;
    }
    if jumps.len() > MAX_PROXY_JUMPS {
        bail!("SSH ProxyJump chain exceeds {MAX_PROXY_JUMPS} hops");
    }
    for endpoint in jumps.iter().skip(1) {
        if endpoint.proxy_command.is_some() {
            bail!(
                "SSH ProxyCommand for jump host {} cannot run after another ProxyJump hop",
                endpoint.alias
            );
        }
    }

    Ok(ResolvedSshPlan {
        target: target.endpoint,
        jumps,
        config_path: loaded.path,
    })
}

fn resolve_jump(
    jump: &str,
    config: Option<&SshConfig>,
    pool: &SshPoolConfig,
    stack: &mut Vec<String>,
    resolved: &mut Vec<ResolvedSshEndpoint>,
) -> anyhow::Result<()> {
    if jump.eq_ignore_ascii_case("none") {
        return Ok(());
    }
    let spec = parse_destination(jump)?;
    if stack
        .iter()
        .any(|alias| alias.eq_ignore_ascii_case(&spec.alias))
    {
        stack.push(spec.alias);
        bail!("SSH ProxyJump cycle detected: {}", stack.join(" -> "));
    }
    stack.push(spec.alias.clone());
    let destination = resolve_destination(&spec, config, None, pool)?;
    if !destination.proxy_jump.is_empty() && destination.endpoint.proxy_command.is_some() {
        bail!(
            "SSH jump host {} configures both ProxyJump and ProxyCommand",
            destination.endpoint.alias
        );
    }
    for nested in &destination.proxy_jump {
        resolve_jump(nested, config, pool, stack, resolved)?;
    }
    stack.pop();
    resolved.push(destination.endpoint);
    if resolved.len() > MAX_PROXY_JUMPS {
        bail!("SSH ProxyJump chain exceeds {MAX_PROXY_JUMPS} hops");
    }
    Ok(())
}

fn resolve_destination(
    spec: &DestinationSpec,
    config: Option<&SshConfig>,
    upstream: Option<&SshHostConfig>,
    pool: &SshPoolConfig,
) -> anyhow::Result<ResolvedDestination> {
    let params = config
        .map(|config| config.query(&spec.alias))
        .unwrap_or_else(|| HostParams::new(&DefaultAlgorithms::default()));
    let host = upstream
        .and_then(|upstream| upstream.host.clone())
        .or_else(|| params.host_name.clone())
        .unwrap_or_else(|| spec.alias.clone());
    let username = spec
        .username
        .clone()
        .or_else(|| params.user.clone())
        .or_else(local_username)
        .with_context(|| format!("no SSH username configured for host {}", spec.alias))?;
    let port = spec.port.or(params.port).unwrap_or(DEFAULT_SSH_PORT);
    let connect_timeout = pool
        .connect_timeout_secs
        .map(Duration::from_secs)
        .or(params.connect_timeout)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS));
    let keep_alive = pool
        .keep_alive_secs
        .map(Duration::from_secs)
        .or(params.server_alive_interval)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_KEEP_ALIVE_SECS));
    let keep_alive_max = pool
        .server_alive_count_max
        .map(|count| count as usize)
        .or_else(|| unsupported_usize(&params, "serveralivecountmax"))
        .unwrap_or(DEFAULT_KEEP_ALIVE_MAX);
    let host_key_policy = resolve_host_key_policy(upstream, &params);
    let host_key_name = unsupported_value(&params, "hostkeyalias").unwrap_or_else(|| host.clone());
    let mut endpoint = ResolvedSshEndpoint {
        alias: spec.alias.clone(),
        host,
        port,
        username,
        auth: resolve_auth(upstream, &params),
        host_key_policy,
        host_key_name,
        known_hosts_paths: resolve_known_hosts_paths(upstream, &params),
        connect_timeout,
        keep_alive,
        keep_alive_max,
        tcp_keep_alive: params.tcp_keep_alive.unwrap_or(true),
        proxy_command: unsupported_command(&params, "proxycommand")
            .filter(|command| !command.eq_ignore_ascii_case("none")),
    };
    endpoint.auth.identity_files = endpoint
        .auth
        .identity_files
        .iter()
        .map(|path| expand_path_tokens(path, &endpoint))
        .collect();
    endpoint.known_hosts_paths = endpoint
        .known_hosts_paths
        .iter()
        .map(|path| expand_path_tokens(path, &endpoint))
        .collect();
    let proxy_jump = params
        .proxy_jump
        .unwrap_or_default()
        .into_iter()
        .filter(|jump| !jump.eq_ignore_ascii_case("none"))
        .collect();
    Ok(ResolvedDestination {
        endpoint,
        proxy_jump,
    })
}

fn resolve_auth(upstream: Option<&SshHostConfig>, params: &HostParams) -> ResolvedSshAuth {
    if let Some(auth) = upstream.and_then(|upstream| upstream.auth.clone()) {
        return ResolvedSshAuth {
            explicit: Some(auth),
            identity_files: Vec::new(),
            use_agent: false,
        };
    }
    let public_key_enabled = params.pubkey_authentication.unwrap_or(true);
    let identities_only = unsupported_bool(params, "identitiesonly").unwrap_or(false);
    let identity_files = params
        .identity_file
        .clone()
        .unwrap_or_else(default_identity_files)
        .into_iter()
        .filter(|path| !path.to_string_lossy().eq_ignore_ascii_case("none"))
        .collect();
    ResolvedSshAuth {
        explicit: None,
        identity_files: if public_key_enabled {
            identity_files
        } else {
            Vec::new()
        },
        use_agent: public_key_enabled && !identities_only,
    }
}

fn resolve_host_key_policy(
    upstream: Option<&SshHostConfig>,
    params: &HostParams,
) -> ResolvedHostKeyPolicy {
    if let Some(policy) = upstream.and_then(|upstream| upstream.host_key_policy) {
        return match policy {
            SshHostKeyPolicy::KnownHosts => ResolvedHostKeyPolicy::KnownHosts,
            SshHostKeyPolicy::InsecureAcceptAny => ResolvedHostKeyPolicy::InsecureAcceptAny,
        };
    }
    match unsupported_value(params, "stricthostkeychecking")
        .unwrap_or_else(|| "yes".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "no" | "off" => ResolvedHostKeyPolicy::InsecureAcceptAny,
        "accept-new" => ResolvedHostKeyPolicy::AcceptNew,
        _ => ResolvedHostKeyPolicy::KnownHosts,
    }
}

fn resolve_known_hosts_paths(
    upstream: Option<&SshHostConfig>,
    params: &HostParams,
) -> Vec<PathBuf> {
    if let Some(path) = upstream.and_then(|upstream| upstream.known_hosts_path.as_ref()) {
        return vec![PathBuf::from(path)];
    }
    let has_configured_paths = params.unsupported_fields.contains_key("userknownhostsfile")
        || params
            .unsupported_fields
            .contains_key("globalknownhostsfile");
    let mut paths = unsupported_values(params, "userknownhostsfile")
        .into_iter()
        .chain(unsupported_values(params, "globalknownhostsfile"))
        .filter(|path| !path.eq_ignore_ascii_case("none"))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() && !has_configured_paths {
        paths.push(default_ssh_dir().join("known_hosts"));
    }
    paths
}

fn load_ssh_config(upstream: &SshHostConfig) -> anyhow::Result<LoadedSshConfig> {
    if upstream.ssh_config_host.is_none() {
        return Ok(LoadedSshConfig {
            config: SshConfig::default(),
            path: None,
        });
    }
    let configured_path = upstream.ssh_config_path.as_ref();
    let path = configured_path
        .map(|path| expand_tilde(Path::new(path)))
        .unwrap_or_else(|| default_ssh_dir().join("config"));
    match File::open(&path) {
        Ok(_) => {}
        Err(error) if configured_path.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedSshConfig {
                config: SshConfig::default(),
                path: Some(path),
            });
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open SSH config {}", path.display()));
        }
    };
    reject_match_directives(&path)?;
    let source = connection_ssh_config_source(&path)?;
    let mut reader = source.as_bytes();
    let rules = ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS;
    let config = SshConfig::default()
        .parse(&mut reader, rules)
        .with_context(|| format!("failed to parse SSH config {}", path.display()))?;
    Ok(LoadedSshConfig {
        config,
        path: Some(path),
    })
}

fn load_ssh_config_forwards(
    alias: &str,
    configured_path: Option<&str>,
) -> anyhow::Result<Vec<ParsedSshForward>> {
    let path = configured_path
        .map(|path| expand_tilde(Path::new(path)))
        .unwrap_or_else(|| default_ssh_dir().join("config"));
    match File::open(&path) {
        Ok(_) => {}
        Err(error) if configured_path.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open SSH config {}", path.display()));
        }
    }

    reject_match_directives(&path)?;
    let mut state = ForwardScanState {
        active: true,
        clear_all_forwardings: None,
        forwards: Vec::new(),
    };
    scan_ssh_config_forwards(&path, alias, &mut state, &mut Vec::new())?;
    if state.clear_all_forwardings == Some(true) {
        Ok(Vec::new())
    } else {
        Ok(state.forwards)
    }
}

fn scan_ssh_config_forwards(
    path: &Path,
    alias: &str,
    state: &mut ForwardScanState,
    include_stack: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    if include_stack.len() >= MAX_INCLUDE_DEPTH {
        bail!("SSH Include nesting exceeds {MAX_INCLUDE_DEPTH} files");
    }
    let identity = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if include_stack.contains(&identity) {
        include_stack.push(identity);
        bail!(
            "SSH Include cycle detected: {}",
            include_stack
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ")
        );
    }
    include_stack.push(identity);

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to inspect SSH config {}", path.display()))?;
    for (line_index, line) in contents.lines().enumerate() {
        let cleaned = strip_config_comment(line);
        let line = cleaned.trim();
        let Some((directive, arguments)) = split_config_directive(line) else {
            continue;
        };
        let line_number = line_index + 1;
        if directive.eq_ignore_ascii_case("host") {
            state.active = host_patterns_match(arguments, alias).with_context(|| {
                format!(
                    "invalid Host directive in SSH config {}:{}",
                    path.display(),
                    line_number
                )
            })?;
            continue;
        }
        if directive.eq_ignore_ascii_case("match") {
            bail!(
                "SSH config Match blocks are not supported ({}:{})",
                path.display(),
                line_number
            );
        }
        if directive.eq_ignore_ascii_case("include") {
            if state.active {
                for pattern in split_config_arguments(arguments) {
                    let pattern = expand_include_path(&pattern);
                    let display_pattern = pattern.to_string_lossy();
                    for entry in glob(&display_pattern)
                        .with_context(|| format!("invalid SSH Include pattern {display_pattern}"))?
                    {
                        let included = entry.with_context(|| {
                            format!("failed to resolve SSH Include pattern {display_pattern}")
                        })?;
                        scan_ssh_config_forwards(&included, alias, state, include_stack)?;
                    }
                }
            }
            continue;
        }
        if !state.active {
            continue;
        }
        if directive.eq_ignore_ascii_case("clearallforwardings") {
            if state.clear_all_forwardings.is_none() {
                state.clear_all_forwardings =
                    Some(parse_ssh_bool(arguments).with_context(|| {
                        format!(
                            "invalid ClearAllForwardings directive in SSH config {}:{}",
                            path.display(),
                            line_number
                        )
                    })?);
            }
            continue;
        }

        let parsed = if directive.eq_ignore_ascii_case("dynamicforward") {
            Some(parse_dynamic_forward(arguments))
        } else if directive.eq_ignore_ascii_case("localforward") {
            Some(parse_local_forward(arguments))
        } else if directive.eq_ignore_ascii_case("remoteforward") {
            Some(parse_remote_forward(arguments))
        } else {
            None
        };
        if let Some(parsed) = parsed {
            state.forwards.push(parsed.with_context(|| {
                format!(
                    "invalid {directive} directive in SSH config {}:{}",
                    path.display(),
                    line_number
                )
            })?);
        }
    }
    include_stack.pop();
    Ok(())
}

fn host_patterns_match(arguments: &str, alias: &str) -> anyhow::Result<bool> {
    let patterns = split_config_arguments(arguments);
    if patterns.is_empty() {
        bail!("Host requires at least one pattern");
    }
    let mut matched = false;
    let mut has_positive = false;
    for pattern in patterns {
        let (negated, pattern) = pattern
            .strip_prefix('!')
            .map(|pattern| (true, pattern))
            .unwrap_or((false, pattern.as_str()));
        if pattern.is_empty() {
            bail!("Host pattern must not be empty");
        }
        let intersects = HostClause::new(pattern.to_string(), negated).intersects(alias);
        if negated && intersects {
            return Ok(false);
        }
        if !negated {
            has_positive = true;
            matched |= intersects;
        }
    }
    Ok(has_positive && matched)
}

fn parse_dynamic_forward(arguments: &str) -> anyhow::Result<ParsedSshForward> {
    let arguments = split_config_arguments(arguments);
    if arguments.len() != 1 {
        bail!("DynamicForward requires one listen address");
    }
    Ok(ParsedSshForward::LocalProxy {
        listen: parse_forward_listen(&arguments[0])?,
    })
}

fn parse_local_forward(arguments: &str) -> anyhow::Result<ParsedSshForward> {
    let arguments = split_config_arguments(arguments);
    if arguments.len() != 2 {
        bail!("LocalForward requires a listen address and target");
    }
    Ok(ParsedSshForward::LocalForward {
        listen: parse_forward_listen(&arguments[0])?,
        target: HostPort::from_str(&arguments[1]).map_err(anyhow::Error::msg)?,
    })
}

fn parse_remote_forward(arguments: &str) -> anyhow::Result<ParsedSshForward> {
    let arguments = split_config_arguments(arguments);
    match arguments.as_slice() {
        [listen] => Ok(ParsedSshForward::RemoteProxy {
            listen: parse_forward_listen(listen)?,
        }),
        [listen, target] => Ok(ParsedSshForward::RemoteForward {
            listen: parse_forward_listen(listen)?,
            target: HostPort::from_str(target).map_err(anyhow::Error::msg)?,
        }),
        _ => bail!("RemoteForward requires a listen address and optional target"),
    }
}

fn parse_forward_listen(value: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(port) = value.parse::<u16>() {
        if port == 0 {
            bail!("forward listen port must be non-zero");
        }
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    if let Ok(listen) = value.parse::<SocketAddr>() {
        if listen.port() == 0 {
            bail!("forward listen port must be non-zero");
        }
        return Ok(listen);
    }

    let (host, port) = value
        .rsplit_once(':')
        .with_context(|| format!("forward listen must be [bind-address:]port: {value}"))?;
    let port = port
        .parse::<u16>()
        .with_context(|| format!("invalid forward listen port: {value}"))?;
    if port == 0 {
        bail!("forward listen port must be non-zero");
    }
    let host = host.trim_matches(['[', ']']);
    let ip = match host.to_ascii_lowercase().as_str() {
        "" | "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
        "*" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        _ => host
            .parse::<IpAddr>()
            .with_context(|| format!("forward bind address must be an IP address: {host}"))?,
    };
    Ok(SocketAddr::new(ip, port))
}

fn parse_ssh_bool(arguments: &str) -> anyhow::Result<bool> {
    let arguments = split_config_arguments(arguments);
    let [value] = arguments.as_slice() else {
        bail!("expected one yes/no value");
    };
    match value.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" | "1" => Ok(true),
        "no" | "false" | "off" | "0" => Ok(false),
        _ => bail!("expected yes or no"),
    }
}

fn connection_ssh_config_source(path: &Path) -> anyhow::Result<String> {
    // ssh2-config 0.7.1 treats RemoteForward as a bare port. STK parses all
    // forwarding directives separately, so omit them from connection parsing.
    let mut source = String::new();
    append_connection_ssh_config(path, &mut source, &mut Vec::new(), None)?;
    Ok(source)
}

fn append_connection_ssh_config(
    path: &Path,
    source: &mut String,
    include_stack: &mut Vec<PathBuf>,
    inherited_host: Option<&str>,
) -> anyhow::Result<()> {
    if include_stack.len() >= MAX_INCLUDE_DEPTH {
        bail!("SSH Include nesting exceeds {MAX_INCLUDE_DEPTH} files");
    }
    let identity = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if include_stack.contains(&identity) {
        include_stack.push(identity);
        bail!(
            "SSH Include cycle detected: {}",
            include_stack
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ")
        );
    }
    include_stack.push(identity);

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to inspect SSH config {}", path.display()))?;
    let mut current_host = inherited_host.map(str::to_string);
    for (line_index, original_line) in contents.lines().enumerate() {
        let cleaned = strip_config_comment(original_line);
        let line = cleaned.trim();
        let Some((directive, arguments)) = split_config_directive(line) else {
            source.push_str(original_line);
            source.push('\n');
            continue;
        };

        if directive.eq_ignore_ascii_case("host") {
            current_host = Some(original_line.to_string());
        }

        if directive.eq_ignore_ascii_case("include") {
            let patterns = split_config_arguments(arguments);
            if patterns.is_empty() {
                bail!(
                    "invalid Include directive in SSH config {}:{}: missing path",
                    path.display(),
                    line_index + 1
                );
            }
            for pattern in patterns {
                let pattern = expand_include_path(&pattern);
                let display_pattern = pattern.to_string_lossy();
                let mut included_paths = glob(&display_pattern)
                    .with_context(|| format!("invalid SSH Include pattern {display_pattern}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .with_context(|| {
                        format!("failed to resolve SSH Include pattern {display_pattern}")
                    })?;
                included_paths.sort();
                for included_path in included_paths {
                    append_connection_ssh_config(
                        &included_path,
                        source,
                        include_stack,
                        current_host.as_deref(),
                    )?;
                    match &current_host {
                        Some(host) => source.push_str(host),
                        None => source.push_str("Host *"),
                    }
                    source.push('\n');
                }
            }
            continue;
        }

        if is_forwarding_directive(directive) {
            source.push('\n');
            continue;
        }

        source.push_str(original_line);
        source.push('\n');
    }
    include_stack.pop();
    Ok(())
}

fn is_forwarding_directive(directive: &str) -> bool {
    directive.eq_ignore_ascii_case("dynamicforward")
        || directive.eq_ignore_ascii_case("localforward")
        || directive.eq_ignore_ascii_case("remoteforward")
        || directive.eq_ignore_ascii_case("clearallforwardings")
}

fn merge_ssh_config_forwards(host: &mut ResolvedHostConfig, forwards: Vec<ParsedSshForward>) {
    let mut local_ports = host
        .local_proxies
        .iter()
        .map(|proxy| proxy.listen.port())
        .chain(
            host.local_forwards
                .iter()
                .map(|forward| forward.listen.port()),
        )
        .collect::<HashSet<_>>();
    let mut remote_ports = host
        .remote_proxies
        .iter()
        .map(|proxy| proxy.listen.port())
        .chain(
            host.remote_forwards
                .iter()
                .map(|forward| forward.listen.port()),
        )
        .collect::<HashSet<_>>();
    let mut names = host
        .local_proxies
        .iter()
        .filter_map(|proxy| proxy.name.clone())
        .chain(
            host.local_forwards
                .iter()
                .filter_map(|forward| forward.name.clone()),
        )
        .chain(
            host.remote_proxies
                .iter()
                .filter_map(|proxy| proxy.name.clone()),
        )
        .chain(
            host.remote_forwards
                .iter()
                .filter_map(|forward| forward.name.clone()),
        )
        .collect::<HashSet<_>>();

    for forward in forwards {
        match forward {
            ParsedSshForward::LocalProxy { listen } if local_ports.insert(listen.port()) => {
                host.local_proxies.push(ResolvedProxyConfig {
                    auto: true,
                    name: Some(unique_forward_name(
                        &mut names,
                        "ssh-config-dynamic",
                        listen.port(),
                    )),
                    listen,
                    mixed: true,
                    protocol: Some(ProxyProtocol::Mixed),
                });
            }
            ParsedSshForward::LocalForward { listen, target }
                if local_ports.insert(listen.port()) =>
            {
                host.local_forwards.push(ResolvedForwardConfig {
                    auto: true,
                    name: Some(unique_forward_name(
                        &mut names,
                        "ssh-config-local",
                        listen.port(),
                    )),
                    listen,
                    target,
                });
            }
            ParsedSshForward::RemoteProxy { listen } if remote_ports.insert(listen.port()) => {
                host.remote_proxies.push(ResolvedProxyConfig {
                    auto: true,
                    name: Some(unique_forward_name(
                        &mut names,
                        "ssh-config-remote-dynamic",
                        listen.port(),
                    )),
                    listen,
                    mixed: true,
                    protocol: Some(ProxyProtocol::Mixed),
                });
            }
            ParsedSshForward::RemoteForward { listen, target }
                if remote_ports.insert(listen.port()) =>
            {
                host.remote_forwards.push(ResolvedForwardConfig {
                    auto: true,
                    name: Some(unique_forward_name(
                        &mut names,
                        "ssh-config-remote",
                        listen.port(),
                    )),
                    listen,
                    target,
                });
            }
            _ => {}
        }
    }
}

fn unique_forward_name(names: &mut HashSet<String>, prefix: &str, port: u16) -> String {
    let base = format!("{prefix}-{port}");
    if names.insert(base.clone()) {
        return base;
    }
    for suffix in 2.. {
        let name = format!("{base}-{suffix}");
        if names.insert(name.clone()) {
            return name;
        }
    }
    unreachable!("forward name suffix space is unbounded")
}

fn reject_match_directives(path: &Path) -> anyhow::Result<()> {
    scan_ssh_config(path, &mut HashSet::new())
}

fn scan_ssh_config(path: &Path, visited: &mut HashSet<PathBuf>) -> anyhow::Result<()> {
    let identity = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(identity) {
        return Ok(());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to inspect SSH config {}", path.display()))?;
    for (line_index, line) in contents.lines().enumerate() {
        let cleaned = strip_config_comment(line);
        let line = cleaned.trim();
        let Some((directive, arguments)) = split_config_directive(line) else {
            continue;
        };
        if directive.eq_ignore_ascii_case("match") {
            bail!(
                "SSH config Match blocks are not supported ({}:{})",
                path.display(),
                line_index + 1
            );
        }
        if !directive.eq_ignore_ascii_case("include") {
            continue;
        }
        for pattern in split_config_arguments(arguments) {
            let pattern = expand_include_path(&pattern);
            let pattern = pattern.to_string_lossy();
            for entry in
                glob(&pattern).with_context(|| format!("invalid SSH Include pattern {pattern}"))?
            {
                let included = entry
                    .with_context(|| format!("failed to resolve SSH Include pattern {pattern}"))?;
                scan_ssh_config(&included, visited)?;
            }
        }
    }
    Ok(())
}

fn strip_config_comment(line: &str) -> String {
    let mut quoted = false;
    let mut escaped = false;
    let mut output = String::with_capacity(line.len());
    for character in line.chars() {
        if escaped {
            output.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => {
                output.push(character);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                output.push(character);
            }
            '#' if !quoted => break,
            _ => output.push(character),
        }
    }
    output
}

fn split_config_directive(line: &str) -> Option<(&str, &str)> {
    if line.is_empty() {
        return None;
    }
    let delimiter = line
        .char_indices()
        .find(|(_, character)| character.is_whitespace() || *character == '=')
        .map(|(index, _)| index)
        .unwrap_or(line.len());
    let directive = &line[..delimiter];
    let arguments = line[delimiter..]
        .trim_start_matches(|character: char| character.is_whitespace() || character == '=')
        .trim();
    Some((directive, arguments))
}

fn split_config_arguments(arguments: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut value = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in arguments.chars() {
        if escaped {
            value.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted => {
                if !value.is_empty() {
                    values.push(std::mem::take(&mut value));
                }
            }
            _ => value.push(character),
        }
    }
    if !value.is_empty() {
        values.push(value);
    }
    values
}

fn expand_include_path(path: &str) -> PathBuf {
    let path = expand_tilde(Path::new(path));
    if path.is_absolute() {
        path
    } else {
        default_ssh_dir().join(path)
    }
}

fn parse_destination(value: &str) -> anyhow::Result<DestinationSpec> {
    let mut value = value.trim();
    if let Some(uri) = value.strip_prefix("ssh://") {
        value = uri;
    }
    let (username, host_port) = value
        .rsplit_once('@')
        .map(|(username, host)| (Some(username.to_string()), host))
        .unwrap_or((None, value));
    let (alias, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let (host, suffix) = rest
            .split_once(']')
            .with_context(|| format!("invalid SSH destination {value}"))?;
        let port = suffix
            .strip_prefix(':')
            .map(str::parse)
            .transpose()
            .with_context(|| format!("invalid SSH port in destination {value}"))?;
        (host.to_string(), port)
    } else if host_port.matches(':').count() == 1 {
        let (host, port) = host_port
            .rsplit_once(':')
            .expect("one colon must have a split point");
        let port = port
            .parse()
            .with_context(|| format!("invalid SSH port in destination {value}"))?;
        (host.to_string(), Some(port))
    } else {
        (host_port.to_string(), None)
    };
    if alias.is_empty() || username.as_ref().is_some_and(String::is_empty) || port == Some(0) {
        bail!("invalid SSH destination {value}");
    }
    Ok(DestinationSpec {
        alias,
        username,
        port,
    })
}

fn unsupported_value(params: &HostParams, name: &str) -> Option<String> {
    unsupported_values(params, name)
        .into_iter()
        .next()
        .map(|value| unquote(&value))
}

fn unsupported_command(params: &HostParams, name: &str) -> Option<String> {
    let command = unsupported_values(params, name).join(" ");
    (!command.is_empty()).then(|| unquote(&command))
}

fn unsupported_values(params: &HostParams, name: &str) -> Vec<String> {
    params
        .unsupported_fields
        .get(name)
        .cloned()
        .unwrap_or_default()
}

fn unsupported_bool(params: &HostParams, name: &str) -> Option<bool> {
    match unsupported_value(params, name)?
        .to_ascii_lowercase()
        .as_str()
    {
        "yes" | "true" | "on" | "1" => Some(true),
        "no" | "false" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn unsupported_usize(params: &HostParams, name: &str) -> Option<usize> {
    unsupported_value(params, name)?.parse().ok()
}

fn expand_path_tokens(path: &Path, endpoint: &ResolvedSshEndpoint) -> PathBuf {
    let raw = unquote(&path.to_string_lossy());
    let expanded = expand_percent_tokens(&raw, endpoint);
    expand_tilde(Path::new(&expanded))
}

pub(crate) fn expand_proxy_command(command: &str, endpoint: &ResolvedSshEndpoint) -> String {
    expand_percent_tokens(command, endpoint)
}

fn expand_percent_tokens(value: &str, endpoint: &ResolvedSshEndpoint) -> String {
    let local_user = local_username().unwrap_or_default();
    let local_home = home_dir().to_string_lossy().into_owned();
    let local_host = env::var("HOSTNAME")
        .or_else(|_| env::var("COMPUTERNAME"))
        .unwrap_or_default();
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        match chars.next() {
            Some('%') => output.push('%'),
            Some('d') => output.push_str(&local_home),
            Some('h') => output.push_str(&endpoint.host),
            Some('l') => output.push_str(&local_host),
            Some('n') => output.push_str(&endpoint.alias),
            Some('p') => output.push_str(&endpoint.port.to_string()),
            Some('r') => output.push_str(&endpoint.username),
            Some('u') => output.push_str(&local_user),
            Some(other) => {
                output.push('%');
                output.push(other);
            }
            None => output.push('%'),
        }
    }
    output
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

fn local_username() -> Option<String> {
    env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .ok()
        .filter(|username| !username.is_empty())
}

fn default_ssh_dir() -> PathBuf {
    home_dir().join(".ssh")
}

fn default_identity_files() -> Vec<PathBuf> {
    let ssh_dir = default_ssh_dir();
    [
        "id_rsa",
        "id_ecdsa",
        "id_ecdsa_sk",
        "id_ed25519",
        "id_ed25519_sk",
        "id_xmss",
        "id_dsa",
    ]
    .into_iter()
    .map(|name| ssh_dir.join(name))
    .collect()
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"))
}

fn expand_tilde(path: &Path) -> PathBuf {
    let path = path.to_string_lossy();
    if path == "~" {
        return home_dir();
    }
    if let Some(suffix) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home_dir().join(suffix);
    }
    PathBuf::from(path.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, ForwardConfig, LoadBalancePolicy, ProxyConfig};
    use std::{fs, sync::Mutex};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn pool() -> SshPoolConfig {
        SshPoolConfig {
            policy: LoadBalancePolicy::RoundRobin,
            keep_alive_secs: None,
            min_sessions_per_host: 1,
            max_sessions_per_host: 1,
            session_rotation_enabled: false,
            session_rotation_interval_secs: 3_600,
            max_channels_per_session: 8,
            server_alive_count_max: None,
            connect_timeout_secs: None,
            restart_initial_millis: 100,
            restart_max_secs: 1,
            session_spawn_cooldown_millis: 100,
            session_drain_timeout_secs: 1,
            hosts: Vec::new(),
        }
    }

    fn upstream(alias: &str) -> SshHostConfig {
        SshHostConfig {
            name: "test".to_string(),
            host: None,
            ssh_config_host: Some(alias.to_string()),
            port: None,
            username: None,
            auth: None,
            ssh_config_path: None,
            host_key_policy: None,
            known_hosts_path: None,
            remote_forwards: Vec::new(),
        }
    }

    fn upstream_with_path(alias: &str, path: impl Into<String>) -> SshHostConfig {
        SshHostConfig {
            ssh_config_path: Some(path.into()),
            ..upstream(alias)
        }
    }

    fn direct_upstream(host: &str) -> SshHostConfig {
        SshHostConfig {
            name: "test".to_string(),
            host: Some(host.to_string()),
            ssh_config_host: None,
            port: None,
            username: Some("test".to_string()),
            auth: None,
            ssh_config_path: None,
            host_key_policy: None,
            known_hosts_path: None,
            remote_forwards: Vec::new(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "stk-ssh-config-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn resolved_host(alias: &str, config_path: &Path) -> ResolvedHostConfig {
        let mut config = AppConfig::default();
        let defaults = config.override_default.clone();
        let host = config.hosts.get_mut("default").unwrap();
        host.ssh_config_host = Some(alias.to_string());
        host.ssh_config_path = Some(config_path.to_string_lossy().into_owned());
        host.local_proxies.clear();
        host.local_forwards.clear();
        host.remote_proxies.clear();
        host.remote_forwards.clear();
        host.resolve(&defaults)
    }

    #[test]
    fn inherits_all_forward_kinds_with_additive_host_and_include_rules() {
        let directory = temp_dir("forwards");
        let included_path = directory.join("included.conf");
        fs::write(
            &included_path,
            "Host target\n  DynamicForward 1080\n  DynamicForward [::1]:1081\n  LocalForward 127.0.0.1:8080 app.internal:80\n",
        )
        .unwrap();
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            format!(
                "Include {}\nHost ignored\n  DynamicForward 9999\nHost target !target\n  DynamicForward 9998\nHost target\n  RemoteForward *:9000 localhost:9000\n  RemoteForward 9001\nHost *\n  LocalForward 8082 fallback.internal:82\n",
                included_path.display()
            ),
        )
        .unwrap();

        let mut host = resolved_host("target", &config_path);
        inherit_ssh_config_forwards(&mut host).unwrap();

        assert_eq!(host.local_proxies.len(), 2);
        assert_eq!(
            host.local_proxies[0].listen,
            "127.0.0.1:1080".parse().unwrap()
        );
        assert_eq!(host.local_proxies[1].listen, "[::1]:1081".parse().unwrap());
        assert!(host.local_proxies.iter().all(|proxy| proxy.mixed));
        assert!(
            host.local_proxies
                .iter()
                .all(|proxy| proxy.resolved_protocol() == ProxyProtocol::Mixed)
        );
        assert_eq!(host.local_forwards.len(), 2);
        assert_eq!(host.local_forwards[0].target.to_string(), "app.internal:80");
        assert_eq!(
            host.local_forwards[1].listen,
            "127.0.0.1:8082".parse().unwrap()
        );
        assert_eq!(host.remote_forwards.len(), 1);
        assert_eq!(
            host.remote_forwards[0].listen,
            "0.0.0.0:9000".parse().unwrap()
        );
        assert_eq!(host.remote_proxies.len(), 1);
        assert_eq!(
            host.remote_proxies[0].listen,
            "127.0.0.1:9001".parse().unwrap()
        );
        assert_eq!(
            host.remote_proxies[0].resolved_protocol(),
            ProxyProtocol::Mixed
        );
    }

    #[test]
    fn explicit_ports_override_inherited_forwards_even_when_disabled() {
        let directory = temp_dir("forward-overrides");
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            "Host target\n  DynamicForward 1080\n  LocalForward 1081 app.internal:80\n  RemoteForward 9000\n  RemoteForward 9001 localhost:9001\n",
        )
        .unwrap();

        let mut config = AppConfig::default();
        let defaults = config.override_default.clone();
        let source = config.hosts.get_mut("default").unwrap();
        source.ssh_config_host = Some("target".to_string());
        source.ssh_config_path = Some(config_path.to_string_lossy().into_owned());
        source.local_proxies = vec![ProxyConfig {
            auto: Some(false),
            name: Some("disabled-explicit-local".to_string()),
            listen: "[::1]:1080".parse().unwrap(),
            mixed: Some(false),
            protocol: None,
        }];
        source.local_forwards.clear();
        source.remote_proxies.clear();
        source.remote_forwards = vec![ForwardConfig {
            auto: Some(false),
            name: Some("disabled-explicit-remote".to_string()),
            listen: "0.0.0.0:9000".parse().unwrap(),
            target: "localhost:9000".parse().unwrap(),
        }];
        let mut host = source.resolve(&defaults);

        inherit_ssh_config_forwards(&mut host).unwrap();

        assert_eq!(host.local_proxies.len(), 1);
        assert_eq!(host.local_forwards.len(), 1);
        assert_eq!(host.local_forwards[0].listen.port(), 1081);
        assert_eq!(host.remote_proxies.len(), 0);
        assert_eq!(host.remote_forwards.len(), 2);
        assert!(
            host.remote_forwards
                .iter()
                .any(|forward| forward.listen.port() == 9001)
        );
    }

    #[test]
    fn forward_inheritance_can_be_disabled_without_reading_ssh_config() {
        let directory = temp_dir("forwards-disabled");
        let mut host = resolved_host("target", &directory.join("missing"));
        host.inherit_ssh_config_forwards = false;

        inherit_ssh_config_forwards(&mut host).unwrap();

        assert!(host.local_proxies.is_empty());
        assert!(host.local_forwards.is_empty());
        assert!(host.remote_proxies.is_empty());
        assert!(host.remote_forwards.is_empty());
    }

    #[test]
    fn clear_all_forwardings_disables_inherited_forwards() {
        let directory = temp_dir("clear-forwards");
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            "Host target\n  DynamicForward 1080\n  ClearAllForwardings yes\n  LocalForward 8080 app.internal:80\n",
        )
        .unwrap();
        let mut host = resolved_host("target", &config_path);

        inherit_ssh_config_forwards(&mut host).unwrap();

        assert!(host.local_proxies.is_empty());
        assert!(host.local_forwards.is_empty());
        assert!(host.remote_proxies.is_empty());
        assert!(host.remote_forwards.is_empty());
    }

    #[test]
    fn resolves_alias_and_common_options_from_custom_config() {
        let directory = temp_dir("alias");
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            format!(
                "Host target\n  HostName 192.0.2.10\n  User alice\n  Port 2222\n  IdentityFile {}/id_test\n  IdentitiesOnly yes\n  ConnectTimeout 7\n  ServerAliveInterval 11\n  ServerAliveCountMax 5\n  StrictHostKeyChecking accept-new\n  UserKnownHostsFile {}/known_hosts\n",
                directory.display(),
                directory.display()
            ),
        )
        .unwrap();

        let plan = resolve_ssh_plan(
            &upstream_with_path("target", config_path.to_string_lossy().into_owned()),
            &pool(),
        )
        .unwrap();
        assert_eq!(plan.target.host, "192.0.2.10");
        assert_eq!(plan.target.username, "alice");
        assert_eq!(plan.target.port, 2222);
        assert_eq!(plan.target.connect_timeout, Duration::from_secs(7));
        assert_eq!(plan.target.keep_alive, Duration::from_secs(11));
        assert_eq!(plan.target.keep_alive_max, 5);
        assert_eq!(
            plan.target.host_key_policy,
            ResolvedHostKeyPolicy::AcceptNew
        );
        assert!(!plan.target.auth.use_agent);
        assert_eq!(
            plan.target.auth.identity_files,
            vec![directory.join("id_test")]
        );
        assert_eq!(
            plan.target.known_hosts_paths,
            vec![directory.join("known_hosts")]
        );
    }

    #[test]
    fn connection_options_ignore_forwarding_directives() {
        let directory = temp_dir("connection-forwarding");
        let included_path = directory.join("forwards.conf");
        fs::write(
            &included_path,
            "Host target\n  DynamicForward 7990\n  LocalForward 2222 localhost:22\n  RemoteForward 7990\n  RemoteForward 3126 localhost:3126\n",
        )
        .unwrap();
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            format!(
                "Include {}\nHost unrelated\n  RemoteForward 7890 localhost:7890\nHost target\n  HostName target.internal\n  User alice\n  Port 2222\n",
                included_path.display()
            ),
        )
        .unwrap();

        let plan = resolve_ssh_plan(
            &upstream_with_path("target", config_path.to_string_lossy().into_owned()),
            &pool(),
        )
        .unwrap();

        assert_eq!(plan.target.host, "target.internal");
        assert_eq!(plan.target.username, "alice");
        assert_eq!(plan.target.port, 2222);
    }

    #[test]
    fn included_host_scope_does_not_leak_into_the_parent_config() {
        let directory = temp_dir("include-scope");
        let included_path = directory.join("included.conf");
        fs::write(&included_path, "Host other\n  HostName other.internal\n").unwrap();
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            format!(
                "Host target\n  User alice\n  Include {}\n  Port 2222\n",
                included_path.display()
            ),
        )
        .unwrap();

        let target = resolve_ssh_plan(
            &upstream_with_path("target", config_path.to_string_lossy().into_owned()),
            &pool(),
        )
        .unwrap();
        let other = resolve_ssh_plan(
            &upstream_with_path("other", config_path.to_string_lossy().into_owned()),
            &pool(),
        )
        .unwrap();

        assert_eq!(target.target.port, 2222);
        assert_eq!(other.target.host, "other.internal");
        assert_eq!(other.target.port, DEFAULT_SSH_PORT);
    }

    #[test]
    fn explicit_upstream_values_override_ssh_config() {
        let directory = temp_dir("override");
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            "Host target\n  HostName config.example\n  User config-user\n  Port 2200\n  StrictHostKeyChecking no\n",
        )
        .unwrap();
        let mut target = upstream_with_path("target", config_path.to_string_lossy().into_owned());
        target.host = Some("explicit.example".to_string());
        target.username = Some("explicit-user".to_string());
        target.port = Some(2022);
        target.auth = Some(SshAuthConfig::Agent);
        target.host_key_policy = Some(SshHostKeyPolicy::KnownHosts);

        let plan = resolve_ssh_plan(&target, &pool()).unwrap();
        assert_eq!(plan.target.host, "explicit.example");
        assert_eq!(plan.target.username, "explicit-user");
        assert_eq!(plan.target.port, 2022);
        assert!(matches!(
            plan.target.auth.explicit,
            Some(SshAuthConfig::Agent)
        ));
        assert_eq!(
            plan.target.host_key_policy,
            ResolvedHostKeyPolicy::KnownHosts
        );
    }

    #[test]
    fn resolves_proxy_jump_aliases_and_detects_cycles() {
        let directory = temp_dir("jump");
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            "Host target\n  HostName target.internal\n  ProxyJump jump-a,jump-b\nHost jump-a\n  HostName 192.0.2.1\n  User first\nHost jump-b\n  HostName 192.0.2.2\n  User second\n",
        )
        .unwrap();
        let config_path = config_path.to_string_lossy().into_owned();
        let plan =
            resolve_ssh_plan(&upstream_with_path("target", config_path.clone()), &pool()).unwrap();
        assert_eq!(
            plan.jumps
                .iter()
                .map(|jump| jump.host.as_str())
                .collect::<Vec<_>>(),
            vec!["192.0.2.1", "192.0.2.2"]
        );

        fs::write(
            directory.join("cycle"),
            "Host target\n  ProxyJump jump\nHost jump\n  ProxyJump target\n",
        )
        .unwrap();
        let error = resolve_ssh_plan(
            &upstream_with_path(
                "target",
                directory.join("cycle").to_string_lossy().into_owned(),
            ),
            &pool(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cycle detected"));
    }

    #[test]
    fn resolves_included_hosts_and_preserves_first_value_wins() {
        let directory = temp_dir("include");
        let included_path = directory.join("included.conf");
        fs::write(
            &included_path,
            "Host target\n  HostName included.example\n  User included-user\nHost *\n  Port 2200\n",
        )
        .unwrap();
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            format!(
                "Host target\n  User first-user\nInclude {}\nHost *\n  User fallback-user\n  Port 22\n",
                included_path.display()
            ),
        )
        .unwrap();

        let plan = resolve_ssh_plan(
            &upstream_with_path("target", config_path.to_string_lossy().into_owned()),
            &pool(),
        )
        .unwrap();
        assert_eq!(plan.target.host, "included.example");
        assert_eq!(plan.target.username, "first-user");
        assert_eq!(plan.target.port, 2200);
    }

    #[test]
    fn rejects_match_blocks_in_included_configs() {
        let directory = temp_dir("match");
        let included_path = directory.join("included.conf");
        fs::write(&included_path, "Match host target\n  User conditional\n").unwrap();
        let config_path = directory.join("config");
        fs::write(
            &config_path,
            format!(
                "Include {}\nHost target\n  User alice\n",
                included_path.display()
            ),
        )
        .unwrap();

        let error = resolve_ssh_plan(
            &upstream_with_path("target", config_path.to_string_lossy().into_owned()),
            &pool(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Match blocks are not supported"));
    }

    #[test]
    fn default_config_path_is_optional_and_can_expand_tilde() {
        let _guard = ENV_LOCK.lock().unwrap();
        let directory = temp_dir("home");
        let old_home = env::var_os("HOME");
        unsafe { env::set_var("HOME", &directory) };
        let plan = resolve_ssh_plan(&upstream("example.test"), &pool()).unwrap();
        assert_eq!(plan.config_path, Some(directory.join(".ssh/config")));

        fs::create_dir_all(directory.join(".ssh")).unwrap();
        fs::write(
            directory.join(".ssh/custom"),
            "Host alias\n  HostName custom.example\n  User alice\n",
        )
        .unwrap();
        let custom =
            resolve_ssh_plan(&upstream_with_path("alias", "~/.ssh/custom"), &pool()).unwrap();
        assert_eq!(custom.target.host, "custom.example");
        match old_home {
            Some(home) => unsafe { env::set_var("HOME", home) },
            None => unsafe { env::remove_var("HOME") },
        }
    }

    #[test]
    fn direct_host_does_not_load_ssh_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let directory = temp_dir("direct");
        let old_home = env::var_os("HOME");
        unsafe { env::set_var("HOME", &directory) };
        fs::create_dir_all(directory.join(".ssh")).unwrap();
        fs::write(
            directory.join(".ssh/config"),
            "Host *\n  HostName config.example\n",
        )
        .unwrap();

        let plan = resolve_ssh_plan(&direct_upstream("direct.example"), &pool()).unwrap();
        assert_eq!(plan.target.host, "direct.example");
        assert_eq!(plan.config_path, None);

        match old_home {
            Some(home) => unsafe { env::set_var("HOME", home) },
            None => unsafe { env::remove_var("HOME") },
        }
    }

    #[test]
    fn expands_proxy_command_tokens() {
        let endpoint = ResolvedSshEndpoint {
            alias: "target".to_string(),
            host: "target.internal".to_string(),
            port: 2222,
            username: "alice".to_string(),
            auth: ResolvedSshAuth {
                explicit: None,
                identity_files: Vec::new(),
                use_agent: true,
            },
            host_key_policy: ResolvedHostKeyPolicy::KnownHosts,
            host_key_name: "target.internal".to_string(),
            known_hosts_paths: Vec::new(),
            connect_timeout: Duration::from_secs(1),
            keep_alive: Duration::from_secs(1),
            keep_alive_max: 1,
            tcp_keep_alive: true,
            proxy_command: None,
        };
        assert_eq!(
            expand_proxy_command("proxy %h %p %r %n %%", &endpoint),
            "proxy target.internal 2222 alice target %"
        );
    }
}
