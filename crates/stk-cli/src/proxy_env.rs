use anyhow::Context as _;
use clap::{Args, ValueEnum};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
};
use stk_core::{
    AppConfig, ConfigScope, ControlEndpoint, LocalProxyCandidate, ProxyEnvScheme, ProxyEnvVariable,
    config::{EnvProfileConfig, ProxyProtocol},
    fetch_runtime_snapshot, resolve_config_path,
    stats::{TunnelKind, TunnelRuntimeStatus},
};

#[derive(Debug, Args)]
pub(super) struct EnvArgs {
    #[arg(
        short = 'p',
        long = "proxy",
        value_name = "PROFILE_OR_SELECTOR",
        help = "Use an env profile or HOST/TUNNEL@SCHEME selector"
    )]
    pub(super) proxy: Option<String>,
    #[arg(short = 'H', long, help = "Override the selected SSH host")]
    host: Option<String>,
    #[arg(short, long, help = "Override the selected local proxy tunnel")]
    tunnel: Option<String>,
    #[arg(
        short,
        long,
        value_enum,
        help = "Override the proxy scheme used by the child command"
    )]
    scheme: Option<EnvSchemeArg>,
    #[arg(
        short,
        long,
        help = "Config file or directory; defaults to the user config directory"
    )]
    config: Option<PathBuf>,
    #[arg(long, help = "Use the system config and control endpoint")]
    system: bool,
    #[arg(
        long,
        help = "Require the selected local proxy to be listening in the running runtime"
    )]
    live: bool,
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        help = "Command and arguments to execute; omit to print the computed variables"
    )]
    pub(super) command: Vec<OsString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EnvSchemeArg {
    Auto,
    Socks5h,
    Socks5,
    Http,
}

impl From<EnvSchemeArg> for ProxyEnvScheme {
    fn from(value: EnvSchemeArg) -> Self {
        match value {
            EnvSchemeArg::Auto => Self::Auto,
            EnvSchemeArg::Socks5h => Self::Socks5h,
            EnvSchemeArg::Socks5 => Self::Socks5,
            EnvSchemeArg::Http => Self::Http,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProxySelection {
    host: Option<String>,
    tunnel: Option<String>,
    scheme: Option<ProxyEnvScheme>,
    inject: Option<BTreeSet<ProxyEnvVariable>>,
    inherit: Option<BTreeSet<ProxyEnvVariable>>,
}

impl From<EnvProfileConfig> for ProxySelection {
    fn from(profile: EnvProfileConfig) -> Self {
        Self {
            host: profile.host,
            tunnel: profile.tunnel,
            scheme: profile.scheme,
            inject: profile.inject,
            inherit: profile.inherit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProxyEnvironmentPlan {
    candidate: LocalProxyCandidate,
    scheme: ProxyEnvScheme,
    url: String,
    set: BTreeMap<String, String>,
    remove: Vec<&'static str>,
}

pub(super) async fn run(args: EnvArgs) -> anyhow::Result<()> {
    let scope = if args.system {
        ConfigScope::System
    } else {
        ConfigScope::User
    };
    let path = resolve_config_path(args.config.as_deref(), scope);
    let config = AppConfig::from_path(&path)
        .with_context(|| format!("failed to load {}", path.display()))?;
    config.validate()?;
    let selection = requested_proxy_selection(&config, &args)?;
    let plan = build_proxy_environment_plan(&config, &selection)?;

    if args.live {
        require_live_proxy(&config, scope, &plan.candidate).await?;
    }

    if args.command.is_empty() {
        for (name, value) in &plan.set {
            println!("{name}={value}");
        }
        return Ok(());
    }

    let mut command = tokio::process::Command::new(&args.command[0]);
    command
        .args(&args.command[1..])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    for name in &plan.remove {
        command.env_remove(name);
    }
    command.envs(&plan.set);
    let status = command
        .status()
        .await
        .with_context(|| format!("failed to execute {}", args.command[0].to_string_lossy()))?;
    if status.success() {
        return Ok(());
    }
    exit_with_child_status(status)
}

fn requested_proxy_selection(config: &AppConfig, args: &EnvArgs) -> anyhow::Result<ProxySelection> {
    let environment_profile = env::var_os("STK_PROXY_PROFILE")
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("STK_PROXY_PROFILE is not valid UTF-8"))
        })
        .transpose()?;
    requested_proxy_selection_with_environment(config, args, environment_profile.as_deref())
}

fn requested_proxy_selection_with_environment(
    config: &AppConfig,
    args: &EnvArgs,
    environment_profile: Option<&str>,
) -> anyhow::Result<ProxySelection> {
    let source = args
        .proxy
        .as_deref()
        .or(environment_profile)
        .or(config.env.default.as_deref());
    let mut selection = source
        .map(|source| selection_from_profile_or_selector(config, source))
        .transpose()?
        .unwrap_or_default();

    if let Some(host) = &args.host {
        selection.host = Some(non_empty_value("host", host)?);
    }
    if let Some(tunnel) = &args.tunnel {
        selection.tunnel = Some(non_empty_value("tunnel", tunnel)?);
    }
    if let Some(scheme) = args.scheme {
        selection.scheme = Some(scheme.into());
    }
    Ok(selection)
}

fn selection_from_profile_or_selector(
    config: &AppConfig,
    value: &str,
) -> anyhow::Result<ProxySelection> {
    if let Some(profile) = config.env.profiles.get(value) {
        return Ok(profile.clone().into());
    }
    parse_proxy_selector(value)
}

fn parse_proxy_selector(value: &str) -> anyhow::Result<ProxySelection> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("proxy profile or selector must not be empty");
    }
    let (location, scheme) = match value.rsplit_once('@') {
        Some((location, scheme)) => {
            if location.contains('@') || scheme.is_empty() {
                anyhow::bail!("invalid proxy selector {value}; expected HOST/TUNNEL@SCHEME");
            }
            (location, Some(parse_proxy_scheme(scheme)?))
        }
        None => (value, None),
    };
    let (host, tunnel) = if location.is_empty() {
        (None, None)
    } else if let Some((host, tunnel)) = location.split_once('/') {
        if host.is_empty() || tunnel.is_empty() || tunnel.contains('/') {
            anyhow::bail!("invalid proxy selector {value}; expected HOST/TUNNEL@SCHEME");
        }
        (Some(host.to_string()), Some(tunnel.to_string()))
    } else {
        (Some(location.to_string()), None)
    };
    if host.is_none() && tunnel.is_none() && scheme.is_none() {
        anyhow::bail!("proxy selector must select a host, tunnel, or scheme");
    }
    Ok(ProxySelection {
        host,
        tunnel,
        scheme,
        ..ProxySelection::default()
    })
}

fn parse_proxy_scheme(value: &str) -> anyhow::Result<ProxyEnvScheme> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(ProxyEnvScheme::Auto),
        "socks5h" => Ok(ProxyEnvScheme::Socks5h),
        "socks5" => Ok(ProxyEnvScheme::Socks5),
        "http" => Ok(ProxyEnvScheme::Http),
        _ => anyhow::bail!(
            "unsupported proxy scheme {value}; expected auto, socks5h, socks5, or http"
        ),
    }
}

fn non_empty_value(field: &str, value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    Ok(value.to_string())
}

fn build_proxy_environment_plan(
    config: &AppConfig,
    selection: &ProxySelection,
) -> anyhow::Result<ProxyEnvironmentPlan> {
    let candidates = config.resolved_local_proxies()?;
    if candidates.is_empty() {
        anyhow::bail!("configuration has no enabled local proxy tunnels");
    }
    let location_matches = candidates
        .into_iter()
        .filter(|candidate| {
            selection
                .host
                .as_deref()
                .is_none_or(|host| candidate.host == host)
                && selection
                    .tunnel
                    .as_deref()
                    .is_none_or(|tunnel| candidate.tunnel == tunnel)
        })
        .collect::<Vec<_>>();
    if location_matches.is_empty() {
        anyhow::bail!(
            "no enabled local proxy matches {}",
            format_proxy_selection(selection)
        );
    }

    let requested_scheme = selection.scheme.unwrap_or(ProxyEnvScheme::Auto);
    let compatible = location_matches
        .iter()
        .filter(|candidate| proxy_scheme_is_compatible(candidate.protocol, requested_scheme))
        .collect::<Vec<_>>();
    if compatible.is_empty() {
        anyhow::bail!(
            "no local proxy matching {} supports scheme {}",
            format_proxy_selection(selection),
            proxy_scheme_name(requested_scheme)
        );
    }
    if selection.host.is_none() && selection.tunnel.is_some() && compatible.len() > 1 {
        anyhow::bail!(
            "local proxy tunnel {} is ambiguous across hosts; specify --host",
            selection.tunnel.as_deref().unwrap_or_default()
        );
    }

    let candidate = (*compatible[0]).clone();
    let scheme = resolve_proxy_scheme(candidate.protocol, requested_scheme);
    let listen = proxy_connect_address(candidate.listen);
    let url = format!("{}://{listen}", proxy_scheme_name(scheme));
    let mut set = BTreeMap::new();
    let inject = selection.inject.as_ref().unwrap_or(&config.env.inject);
    let inherit = selection.inherit.as_ref().unwrap_or(&config.env.inherit);
    if inject.contains(&ProxyEnvVariable::NoProxy) {
        anyhow::bail!("no-proxy cannot be injected; add it to inherit instead");
    }
    let mut remove = Vec::new();
    for variable in [
        ProxyEnvVariable::AllProxy,
        ProxyEnvVariable::HttpProxy,
        ProxyEnvVariable::HttpsProxy,
        ProxyEnvVariable::NoProxy,
    ] {
        let names = proxy_environment_variable_names(variable);
        if inject.contains(&variable) {
            for name in names {
                set.insert(name.to_string(), url.clone());
            }
        } else if !inherit.contains(&variable) {
            remove.extend(names);
        }
    }
    set.insert("STK_PROXY_HOST".to_string(), candidate.host.clone());
    set.insert(
        "STK_PROXY_SCHEME".to_string(),
        proxy_scheme_name(scheme).to_string(),
    );
    set.insert("STK_PROXY_TUNNEL".to_string(), candidate.tunnel.clone());
    set.insert("STK_PROXY_URL".to_string(), url.clone());

    Ok(ProxyEnvironmentPlan {
        candidate,
        scheme,
        url,
        set,
        remove,
    })
}

fn proxy_environment_variable_names(variable: ProxyEnvVariable) -> [&'static str; 2] {
    match variable {
        ProxyEnvVariable::AllProxy => ["ALL_PROXY", "all_proxy"],
        ProxyEnvVariable::HttpProxy => ["HTTP_PROXY", "http_proxy"],
        ProxyEnvVariable::HttpsProxy => ["HTTPS_PROXY", "https_proxy"],
        ProxyEnvVariable::NoProxy => ["NO_PROXY", "no_proxy"],
    }
}

fn proxy_scheme_is_compatible(protocol: ProxyProtocol, scheme: ProxyEnvScheme) -> bool {
    matches!(
        (protocol, scheme),
        (_, ProxyEnvScheme::Auto)
            | (ProxyProtocol::Mixed, _)
            | (
                ProxyProtocol::Socks5h,
                ProxyEnvScheme::Socks5h | ProxyEnvScheme::Socks5
            )
            | (ProxyProtocol::Http, ProxyEnvScheme::Http)
    )
}

fn resolve_proxy_scheme(protocol: ProxyProtocol, requested: ProxyEnvScheme) -> ProxyEnvScheme {
    if requested != ProxyEnvScheme::Auto {
        return requested;
    }
    match protocol {
        ProxyProtocol::Socks5h => ProxyEnvScheme::Socks5h,
        ProxyProtocol::Mixed | ProxyProtocol::Http => ProxyEnvScheme::Http,
    }
}

fn proxy_scheme_name(scheme: ProxyEnvScheme) -> &'static str {
    match scheme {
        ProxyEnvScheme::Auto => "auto",
        ProxyEnvScheme::Socks5h => "socks5h",
        ProxyEnvScheme::Socks5 => "socks5",
        ProxyEnvScheme::Http => "http",
    }
}

fn proxy_connect_address(address: SocketAddr) -> SocketAddr {
    if !address.ip().is_unspecified() {
        return address;
    }
    let ip = match address.ip() {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
    };
    SocketAddr::new(ip, address.port())
}

fn format_proxy_selection(selection: &ProxySelection) -> String {
    let host = selection.host.as_deref().unwrap_or("any host");
    let tunnel = selection.tunnel.as_deref().unwrap_or("any tunnel");
    format!("host {host}, tunnel {tunnel}")
}

async fn require_live_proxy(
    config: &AppConfig,
    scope: ConfigScope,
    candidate: &LocalProxyCandidate,
) -> anyhow::Result<()> {
    let endpoint = ControlEndpoint::from_config(&config.control, scope)?;
    let snapshot = fetch_runtime_snapshot(&endpoint)
        .await
        .with_context(|| format!("failed to query runtime at {endpoint}"))?;
    let tunnel = snapshot
        .hosts
        .iter()
        .find(|host| host.name == candidate.host)
        .and_then(|host| {
            host.tunnels.iter().find(|tunnel| {
                tunnel.kind == TunnelKind::LocalProxy && tunnel.name == candidate.tunnel
            })
        })
        .with_context(|| {
            format!(
                "runtime does not contain local proxy {}/{}",
                candidate.host, candidate.tunnel
            )
        })?;
    if tunnel.status != TunnelRuntimeStatus::Listening {
        let detail = tunnel
            .last_error
            .as_deref()
            .map(|error| format!(": {error}"))
            .unwrap_or_default();
        anyhow::bail!(
            "local proxy {}/{} is {}{detail}",
            candidate.host,
            candidate.tunnel,
            tunnel_status_label(tunnel.status)
        );
    }
    Ok(())
}

fn tunnel_status_label(status: TunnelRuntimeStatus) -> &'static str {
    match status {
        TunnelRuntimeStatus::Starting => "starting",
        TunnelRuntimeStatus::Listening => "listening",
        TunnelRuntimeStatus::Error => "listen-failed",
        TunnelRuntimeStatus::Stopped => "stopped",
    }
}

fn exit_with_child_status(status: std::process::ExitStatus) -> ! {
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            std::process::exit(128 + signal);
        }
    }
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_args() -> EnvArgs {
        EnvArgs {
            proxy: None,
            host: None,
            tunnel: None,
            scheme: None,
            config: None,
            system: false,
            live: false,
            command: Vec::new(),
        }
    }

    fn env_test_config() -> AppConfig {
        AppConfig::from_yaml_str(
            r#"
env:
  default: corp
  profiles:
    corp:
      host: alpha
      tunnel: socks
    web:
      host: alpha
      tunnel: web
      scheme: http
      inject: [https-proxy]
      inherit: [no-proxy]
hosts:
  alpha:
    host: alpha.example
    inherit-ssh-config-forwards: false
    local-proxies:
      - name: socks
        listen: 0.0.0.0:1080
        mixed: true
      - name: web
        listen: "[::]:8080"
        protocol: http
  beta:
    host: beta.example
    inherit-ssh-config-forwards: false
    local-proxies:
      - name: socks
        listen: 127.0.0.1:2080
"#,
        )
        .unwrap()
    }

    #[test]
    fn compact_proxy_selector_parses_host_tunnel_and_scheme() {
        assert_eq!(
            parse_proxy_selector("alpha/socks@socks5h").unwrap(),
            ProxySelection {
                host: Some("alpha".to_string()),
                tunnel: Some("socks".to_string()),
                scheme: Some(ProxyEnvScheme::Socks5h),
                ..ProxySelection::default()
            }
        );
        assert_eq!(
            parse_proxy_selector("alpha@http").unwrap(),
            ProxySelection {
                host: Some("alpha".to_string()),
                tunnel: None,
                scheme: Some(ProxyEnvScheme::Http),
                ..ProxySelection::default()
            }
        );
        assert!(parse_proxy_selector("alpha/socks/extra").is_err());
    }

    #[test]
    fn profile_source_precedence_and_independent_overrides_are_applied() {
        let config = env_test_config();
        let mut args = env_args();
        let from_default =
            requested_proxy_selection_with_environment(&config, &args, None).unwrap();
        assert_eq!(from_default.tunnel.as_deref(), Some("socks"));

        let from_environment =
            requested_proxy_selection_with_environment(&config, &args, Some("web")).unwrap();
        assert_eq!(from_environment.tunnel.as_deref(), Some("web"));
        assert_eq!(from_environment.scheme, Some(ProxyEnvScheme::Http));

        args.proxy = Some("corp".to_string());
        args.tunnel = Some("web".to_string());
        args.scheme = Some(EnvSchemeArg::Http);
        let from_arguments =
            requested_proxy_selection_with_environment(&config, &args, Some("web")).unwrap();
        assert_eq!(from_arguments.host.as_deref(), Some("alpha"));
        assert_eq!(from_arguments.tunnel.as_deref(), Some("web"));
        assert_eq!(from_arguments.scheme, Some(ProxyEnvScheme::Http));
    }

    #[test]
    fn automatic_selection_uses_first_compatible_proxy_and_normalizes_wildcards() {
        let config = env_test_config();
        let socks = build_proxy_environment_plan(&config, &ProxySelection::default()).unwrap();
        assert_eq!(socks.candidate.host, "alpha");
        assert_eq!(socks.candidate.tunnel, "socks");
        assert_eq!(socks.url, "http://127.0.0.1:1080");
        for name in [
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
        ] {
            assert_eq!(socks.set[name], socks.url);
        }
        assert_eq!(socks.remove, ["NO_PROXY", "no_proxy"]);

        let http = build_proxy_environment_plan(
            &config,
            &ProxySelection {
                scheme: Some(ProxyEnvScheme::Http),
                ..ProxySelection::default()
            },
        )
        .unwrap();
        assert_eq!(http.candidate.tunnel, "socks");
        assert_eq!(http.url, "http://127.0.0.1:1080");
        assert_eq!(http.set["HTTPS_PROXY"], http.url);
        assert_eq!(http.set["ALL_PROXY"], http.url);
        assert_eq!(http.remove, ["NO_PROXY", "no_proxy"]);

        let socks_only = build_proxy_environment_plan(
            &config,
            &ProxySelection {
                host: Some("beta".to_string()),
                tunnel: Some("socks".to_string()),
                ..ProxySelection::default()
            },
        )
        .unwrap();
        assert_eq!(socks_only.url, "socks5h://127.0.0.1:2080");
    }

    #[test]
    fn global_and_profile_environment_policies_are_applied() {
        let mut config = env_test_config();
        config.env.inject = [ProxyEnvVariable::AllProxy].into_iter().collect();
        config.env.inherit = [ProxyEnvVariable::HttpProxy, ProxyEnvVariable::NoProxy]
            .into_iter()
            .collect();

        let global = build_proxy_environment_plan(&config, &ProxySelection::default()).unwrap();
        assert_eq!(global.set["ALL_PROXY"], global.url);
        assert!(!global.set.contains_key("HTTP_PROXY"));
        assert!(!global.remove.contains(&"HTTP_PROXY"));
        assert!(!global.remove.contains(&"NO_PROXY"));
        assert!(global.remove.contains(&"HTTPS_PROXY"));

        let inherited_profile =
            requested_proxy_selection_with_environment(&config, &env_args(), None).unwrap();
        let inherited_profile = build_proxy_environment_plan(&config, &inherited_profile).unwrap();
        assert_eq!(inherited_profile.set["ALL_PROXY"], inherited_profile.url);
        assert!(!inherited_profile.set.contains_key("HTTP_PROXY"));
        assert!(!inherited_profile.remove.contains(&"HTTP_PROXY"));
        assert!(!inherited_profile.remove.contains(&"NO_PROXY"));

        let profile =
            requested_proxy_selection_with_environment(&config, &env_args(), Some("web")).unwrap();
        let profile = build_proxy_environment_plan(&config, &profile).unwrap();
        assert_eq!(profile.set["HTTPS_PROXY"], profile.url);
        assert!(!profile.set.contains_key("ALL_PROXY"));
        assert!(profile.remove.contains(&"ALL_PROXY"));
        assert!(profile.remove.contains(&"HTTP_PROXY"));
        assert!(!profile.remove.contains(&"NO_PROXY"));

        let empty_profile = build_proxy_environment_plan(
            &config,
            &ProxySelection {
                inject: Some(BTreeSet::new()),
                inherit: Some(BTreeSet::new()),
                ..ProxySelection::default()
            },
        )
        .unwrap();
        for name in [
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "NO_PROXY",
            "no_proxy",
        ] {
            assert!(!empty_profile.set.contains_key(name));
            assert!(empty_profile.remove.contains(&name));
        }
    }

    #[test]
    fn http_listener_uses_ipv6_loopback_and_rejects_socks_scheme() {
        let config = env_test_config();
        let http = build_proxy_environment_plan(
            &config,
            &ProxySelection {
                host: Some("alpha".to_string()),
                tunnel: Some("web".to_string()),
                scheme: Some(ProxyEnvScheme::Auto),
                ..ProxySelection::default()
            },
        )
        .unwrap();
        assert_eq!(http.url, "http://[::1]:8080");
        assert!(
            build_proxy_environment_plan(
                &config,
                &ProxySelection {
                    host: Some("alpha".to_string()),
                    tunnel: Some("web".to_string()),
                    scheme: Some(ProxyEnvScheme::Socks5h),
                    ..ProxySelection::default()
                },
            )
            .is_err()
        );
    }

    #[test]
    fn tunnel_only_selection_requires_an_unambiguous_host() {
        let config = env_test_config();
        let error = build_proxy_environment_plan(
            &config,
            &ProxySelection {
                tunnel: Some("socks".to_string()),
                ..ProxySelection::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("ambiguous across hosts"));
    }
}
