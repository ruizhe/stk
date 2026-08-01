use anyhow::{Context as _, bail};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env,
    ffi::OsString,
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};
use stk_core::{
    AppConfig, ApplicationLauncherEntryConfig, BrowserEngine, BrowserLauncherEntryConfig,
    EnvProfileConfig, LocalProxyCandidate, ProxyEnvScheme, ProxyEnvVariable, config::ProxyProtocol,
    stats::{RuntimeSnapshot, TunnelKind, TunnelRuntimeStatus},
};

const PROXY_ENVIRONMENT_NAMES: [&str; 12] = [
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "STK_PROXY_HOST",
    "STK_PROXY_TUNNEL",
    "STK_PROXY_SCHEME",
    "STK_PROXY_URL",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchMode {
    Normal,
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LauncherKind {
    Browser(BrowserEngine),
    Application,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LauncherCatalog {
    pub items: Vec<LauncherItem>,
    pub error: Option<String>,
}

impl LauncherCatalog {
    pub fn load(path: &Path) -> Self {
        match load_launcher_catalog(path) {
            Ok(items) => Self { items, error: None },
            Err(error) => Self {
                items: Vec::new(),
                error: Some(format!("{error:#}")),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LauncherItem {
    pub id: String,
    pub name: String,
    pub icon_text: String,
    pub kind: LauncherKind,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub normal_args: Vec<String>,
    pub private_args: Vec<String>,
    pub proxy_args: Vec<String>,
    pub profile_dir: Option<String>,
    pub working_directory: Option<String>,
    pub environment: BTreeMap<String, String>,
    pub unset_environment: Vec<String>,
    pub proxy: Result<LauncherProxyPlan, String>,
    order: i32,
}

impl LauncherItem {
    pub fn is_browser(&self) -> bool {
        matches!(self.kind, LauncherKind::Browser(_))
    }

    pub fn proxy_summary(&self) -> String {
        match &self.proxy {
            Ok(LauncherProxyPlan::Stk(plan)) => format!(
                "{} · {} · {}",
                plan.candidate.host,
                proxy_scheme_name(plan.scheme).to_ascii_uppercase(),
                plan.address
            ),
            Ok(LauncherProxyPlan::Direct) => "Direct".to_string(),
            Ok(LauncherProxyPlan::Inherit) => "Inherit environment".to_string(),
            Err(error) => error.clone(),
        }
    }

    pub fn unavailable_reason(&self, snapshot: &RuntimeSnapshot) -> Option<String> {
        if !self.command.is_file() {
            return Some(format!("executable not found: {}", self.command.display()));
        }
        match &self.proxy {
            Err(error) => Some(error.clone()),
            Ok(LauncherProxyPlan::Stk(plan)) => proxy_unavailable_reason(plan, snapshot),
            Ok(LauncherProxyPlan::Direct | LauncherProxyPlan::Inherit) => None,
        }
    }

    pub fn launch(&self, mode: LaunchMode) -> anyhow::Result<()> {
        let proxy = self
            .proxy
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))?;
        let plan = self.build_launch_plan(mode, proxy)?;
        plan.spawn()
    }

    fn build_launch_plan(
        &self,
        mode: LaunchMode,
        proxy: &LauncherProxyPlan,
    ) -> anyhow::Result<LaunchPlan> {
        let mut args = self.args.iter().map(OsString::from).collect::<Vec<_>>();
        let configured_profile_dir = self.profile_dir.as_deref().map(expand_path);
        match self.kind {
            LauncherKind::Browser(BrowserEngine::Chromium) => {
                if let Some(profile_dir) = configured_profile_dir.as_ref() {
                    fs::create_dir_all(profile_dir).with_context(|| {
                        format!("failed to create browser profile {}", profile_dir.display())
                    })?;
                    args.push(format!("--user-data-dir={}", profile_dir.display()).into());
                }
                if let LauncherProxyPlan::Stk(plan) = proxy {
                    args.push(format!("--proxy-server={}", chromium_proxy_url(plan)).into());
                }
                match mode {
                    LaunchMode::Normal => {
                        args.extend(self.normal_args.iter().map(OsString::from));
                    }
                    LaunchMode::Private => {
                        args.push("--incognito".into());
                        args.extend(self.private_args.iter().map(OsString::from));
                    }
                }
            }
            LauncherKind::Browser(BrowserEngine::Firefox) => {
                let profile_dir = configured_profile_dir
                    .unwrap_or_else(|| self.default_managed_profile_directory(proxy));
                prepare_firefox_profile(&profile_dir, proxy)?;
                args.push("-no-remote".into());
                args.push("-profile".into());
                args.push(profile_dir.as_os_str().to_os_string());
                match mode {
                    LaunchMode::Normal => {
                        args.extend(self.normal_args.iter().map(OsString::from));
                    }
                    LaunchMode::Private => {
                        args.push("-private-window".into());
                        args.extend(self.private_args.iter().map(OsString::from));
                    }
                }
            }
            LauncherKind::Browser(BrowserEngine::Custom) => {
                let profile_dir = configured_profile_dir.as_deref();
                if let Some(profile_dir) = profile_dir {
                    fs::create_dir_all(profile_dir).with_context(|| {
                        format!(
                            "failed to create browser profile {}",
                            profile_dir.display()
                        )
                    })?;
                }
                args.extend(expand_proxy_arguments(&self.proxy_args, proxy, profile_dir)?);
                args.extend(match mode {
                    LaunchMode::Normal => self.normal_args.iter().map(OsString::from),
                    LaunchMode::Private => self.private_args.iter().map(OsString::from),
                });
            }
            LauncherKind::Application => {
                args.extend(expand_proxy_arguments(&self.proxy_args, proxy, None)?);
            }
        }

        let (mut environment_set, mut environment_remove) = proxy.environment();
        for name in &self.unset_environment {
            environment_set.remove(name);
            if !environment_remove.contains(name) {
                environment_remove.push(name.clone());
            }
        }
        for (name, value) in &self.environment {
            environment_remove.retain(|removed| removed != name);
            environment_set.insert(name.clone(), value.clone());
        }

        Ok(LaunchPlan {
            command: self.command.clone(),
            args,
            working_directory: self
                .working_directory
                .as_deref()
                .map(expand_path),
            environment_set,
            environment_remove,
        })
    }

    fn default_managed_profile_directory(&self, proxy: &LauncherProxyPlan) -> PathBuf {
        stk_core::default_config_directory(stk_core::ConfigScope::User)
            .join("browser-data")
            .join(sanitize_path_segment(&self.id))
            .join(proxy.profile_key())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LauncherProxyPlan {
    Stk(ProxyEnvironmentPlan),
    Direct,
    Inherit,
}

impl LauncherProxyPlan {
    fn profile_key(&self) -> String {
        match self {
            Self::Stk(plan) => sanitize_path_segment(&format!(
                "{}-{}-{}",
                plan.candidate.host,
                plan.candidate.tunnel,
                proxy_scheme_name(plan.scheme)
            )),
            Self::Direct => "direct".to_string(),
            Self::Inherit => "inherit".to_string(),
        }
    }

    fn environment(&self) -> (BTreeMap<String, String>, Vec<String>) {
        match self {
            Self::Stk(plan) => (plan.set.clone(), plan.remove.clone()),
            Self::Direct => (
                BTreeMap::new(),
                PROXY_ENVIRONMENT_NAMES
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ),
            Self::Inherit => (BTreeMap::new(), Vec::new()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProxyEnvironmentPlan {
    pub candidate: LocalProxyCandidate,
    pub scheme: ProxyEnvScheme,
    pub address: SocketAddr,
    pub url: String,
    pub set: BTreeMap<String, String>,
    pub remove: Vec<String>,
}

struct LaunchPlan {
    command: PathBuf,
    args: Vec<OsString>,
    working_directory: Option<PathBuf>,
    environment_set: BTreeMap<String, String>,
    environment_remove: Vec<String>,
}

impl LaunchPlan {
    fn spawn(self) -> anyhow::Result<()> {
        let mut command = Command::new(&self.command);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(directory) = &self.working_directory {
            command.current_dir(directory);
        }
        for name in &self.environment_remove {
            command.env_remove(name);
        }
        command.envs(&self.environment_set);
        let mut child = command.spawn().with_context(|| {
            format!("failed to start launcher command {}", self.command.display())
        })?;
        let name = self.command.display().to_string();
        if let Err(error) = thread::Builder::new()
            .name("stk-launcher-reaper".to_string())
            .spawn(move || {
                if let Err(error) = child.wait() {
                    tracing::debug!(%error, command = %name, "launcher child wait failed");
                }
            })
        {
            tracing::debug!(%error, "failed to start launcher child reaper");
        }
        Ok(())
    }
}

#[derive(Clone)]
struct DetectedBrowser {
    id: &'static str,
    name: &'static str,
    engine: BrowserEngine,
    command: PathBuf,
    order: i32,
}

fn load_launcher_catalog(path: &Path) -> anyhow::Result<Vec<LauncherItem>> {
    let config = AppConfig::from_path(path)
        .with_context(|| format!("failed to load launcher configuration from {}", path.display()))?;
    config.validate()?;
    let detected = detect_browsers();
    let detected_by_id = detected
        .iter()
        .map(|browser| (browser.id, browser))
        .collect::<BTreeMap<_, _>>();
    let mut items = Vec::new();
    let mut configured_ids = HashSet::new();

    for (id, browser) in &config.launchers.browsers.entries {
        configured_ids.insert(id.as_str());
        if let Some(detect) = browser.detect.as_deref() {
            configured_ids.insert(detect);
        }
        if !browser.enabled || !browser.show_in_overview {
            continue;
        }
        let detected_browser = browser
            .detect
            .as_deref()
            .and_then(|detected_id| detected_by_id.get(detected_id).copied())
            .or_else(|| detected_by_id.get(id.as_str()).copied());
        items.push(resolve_browser_item(&config, id, browser, detected_browser));
    }

    if config.launchers.browsers.auto_discover {
        for browser in detected {
            if configured_ids.contains(browser.id) {
                continue;
            }
            let entry = BrowserLauncherEntryConfig::default();
            items.push(resolve_browser_item(
                &config,
                browser.id,
                &entry,
                Some(&browser),
            ));
        }
    }

    for (id, application) in &config.launchers.applications.entries {
        if !application.enabled || !application.show_in_overview {
            continue;
        }
        items.push(resolve_application_item(&config, id, application));
    }

    items.sort_by(|left, right| {
        left.order
            .cmp(&right.order)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(items)
}

fn resolve_browser_item(
    config: &AppConfig,
    id: &str,
    entry: &BrowserLauncherEntryConfig,
    detected: Option<&DetectedBrowser>,
) -> LauncherItem {
    let name = entry
        .name
        .clone()
        .or_else(|| detected.map(|browser| browser.name.to_string()))
        .unwrap_or_else(|| id.to_string());
    let command = entry
        .command
        .as_deref()
        .and_then(find_executable)
        .or_else(|| detected.map(|browser| browser.command.clone()))
        .unwrap_or_else(|| {
            entry
                .command
                .as_deref()
                .map(expand_path)
                .unwrap_or_else(|| PathBuf::from(entry.detect.as_deref().unwrap_or(id)))
        });
    let engine = entry
        .engine
        .or_else(|| detected.map(|browser| browser.engine))
        .unwrap_or(BrowserEngine::Custom);
    let proxy_source = effective_proxy_source(
        entry.proxy.as_deref(),
        config.launchers.browsers.default_proxy.as_deref(),
        config.launchers.default_proxy.as_deref(),
        config.env.default.as_deref(),
    );
    LauncherItem {
        id: format!("browser:{id}"),
        icon_text: launcher_icon_text(entry.icon.as_deref(), &name),
        name,
        kind: LauncherKind::Browser(engine),
        command,
        args: entry.args.clone(),
        normal_args: entry.normal_args.clone(),
        private_args: entry.private_args.clone(),
        proxy_args: entry.proxy_args.clone(),
        profile_dir: entry.profile_dir.clone(),
        working_directory: None,
        environment: BTreeMap::new(),
        unset_environment: Vec::new(),
        proxy: resolve_launcher_proxy(config, proxy_source).map_err(|error| format!("{error:#}")),
        order: entry
            .order
            .or_else(|| detected.map(|browser| browser.order))
            .unwrap_or(500),
    }
}

fn resolve_application_item(
    config: &AppConfig,
    id: &str,
    entry: &ApplicationLauncherEntryConfig,
) -> LauncherItem {
    let name = entry.name.clone().unwrap_or_else(|| id.to_string());
    let proxy_source = effective_proxy_source(
        entry.proxy.as_deref(),
        config.launchers.applications.default_proxy.as_deref(),
        config.launchers.default_proxy.as_deref(),
        config.env.default.as_deref(),
    );
    LauncherItem {
        id: format!("application:{id}"),
        icon_text: launcher_icon_text(entry.icon.as_deref(), &name),
        name,
        kind: LauncherKind::Application,
        command: find_executable(&entry.command).unwrap_or_else(|| expand_path(&entry.command)),
        args: entry.args.clone(),
        normal_args: Vec::new(),
        private_args: Vec::new(),
        proxy_args: entry.proxy_args.clone(),
        profile_dir: None,
        working_directory: entry.working_directory.clone(),
        environment: entry.env.clone(),
        unset_environment: entry.unset_env.clone(),
        proxy: resolve_launcher_proxy(config, proxy_source).map_err(|error| format!("{error:#}")),
        order: entry.order.unwrap_or(1_000),
    }
}

fn effective_proxy_source<'a>(
    entry: Option<&'a str>,
    section: Option<&'a str>,
    launcher: Option<&'a str>,
    environment: Option<&'a str>,
) -> Option<&'a str> {
    [entry, section, launcher, environment]
        .into_iter()
        .flatten()
        .find(|value| *value != "default")
}

fn resolve_launcher_proxy(
    config: &AppConfig,
    source: Option<&str>,
) -> anyhow::Result<LauncherProxyPlan> {
    match source {
        Some("direct") => return Ok(LauncherProxyPlan::Direct),
        Some("inherit") => return Ok(LauncherProxyPlan::Inherit),
        _ => {}
    }
    let selection = source
        .map(|source| selection_from_profile_or_selector(config, source))
        .transpose()?
        .unwrap_or_default();
    build_proxy_environment_plan(config, &selection).map(LauncherProxyPlan::Stk)
}

#[derive(Default)]
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

fn selection_from_profile_or_selector(
    config: &AppConfig,
    source: &str,
) -> anyhow::Result<ProxySelection> {
    if let Some(profile) = config.env.profiles.get(source) {
        return Ok(profile.clone().into());
    }
    parse_proxy_selector(source)
}

fn parse_proxy_selector(source: &str) -> anyhow::Result<ProxySelection> {
    let source = source.trim();
    if source.is_empty() {
        bail!("proxy profile or selector must not be empty");
    }
    let (location, scheme) = match source.rsplit_once('@') {
        Some((location, scheme)) if !location.contains('@') && !scheme.is_empty() => {
            (location, Some(parse_proxy_scheme(scheme)?))
        }
        Some(_) => bail!("invalid proxy selector {source}; expected HOST/TUNNEL@SCHEME"),
        None => (source, None),
    };
    let (host, tunnel) = if let Some((host, tunnel)) = location.split_once('/') {
        if host.is_empty() || tunnel.is_empty() || tunnel.contains('/') {
            bail!("invalid proxy selector {source}; expected HOST/TUNNEL@SCHEME");
        }
        (Some(host.to_string()), Some(tunnel.to_string()))
    } else {
        (Some(location.to_string()), None)
    };
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
        "http" => Ok(ProxyEnvScheme::Http),
        "socks5" => Ok(ProxyEnvScheme::Socks5),
        "socks5h" => Ok(ProxyEnvScheme::Socks5h),
        _ => bail!("unsupported proxy scheme {value}"),
    }
}

fn build_proxy_environment_plan(
    config: &AppConfig,
    selection: &ProxySelection,
) -> anyhow::Result<ProxyEnvironmentPlan> {
    let candidates = config.resolved_local_proxies()?;
    if candidates.is_empty() {
        bail!("configuration has no enabled local proxy tunnels");
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
        bail!("no enabled local proxy matches the launcher selection");
    }
    let requested_scheme = selection.scheme.unwrap_or(ProxyEnvScheme::Auto);
    let compatible = location_matches
        .iter()
        .filter(|candidate| proxy_scheme_is_compatible(candidate.protocol, requested_scheme))
        .collect::<Vec<_>>();
    if compatible.is_empty() {
        bail!("no selected local proxy supports the requested scheme");
    }
    if selection.host.is_none() && selection.tunnel.is_some() && compatible.len() > 1 {
        bail!("selected local proxy tunnel is ambiguous across hosts");
    }

    let candidate = (*compatible[0]).clone();
    let scheme = resolve_proxy_scheme(candidate.protocol, requested_scheme);
    let address = proxy_connect_address(candidate.listen);
    let url = format!("{}://{address}", proxy_scheme_name(scheme));
    let inject = selection.inject.as_ref().unwrap_or(&config.env.inject);
    let inherit = selection.inherit.as_ref().unwrap_or(&config.env.inherit);
    let mut set = BTreeMap::new();
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
            remove.extend(names.into_iter().map(str::to_string));
        }
    }
    set.insert("STK_PROXY_HOST".to_string(), candidate.host.clone());
    set.insert("STK_PROXY_TUNNEL".to_string(), candidate.tunnel.clone());
    set.insert(
        "STK_PROXY_SCHEME".to_string(),
        proxy_scheme_name(scheme).to_string(),
    );
    set.insert("STK_PROXY_URL".to_string(), url.clone());
    Ok(ProxyEnvironmentPlan {
        candidate,
        scheme,
        address,
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

fn proxy_unavailable_reason(
    plan: &ProxyEnvironmentPlan,
    snapshot: &RuntimeSnapshot,
) -> Option<String> {
    let Some(host) = snapshot
        .hosts
        .iter()
        .find(|host| host.name == plan.candidate.host)
    else {
        return Some(format!(
            "proxy host {} is not present in the running configuration",
            plan.candidate.host
        ));
    };
    let Some(tunnel) = host.tunnels.iter().find(|tunnel| {
        tunnel.kind == TunnelKind::LocalProxy && tunnel.name == plan.candidate.tunnel
    }) else {
        return Some(format!(
            "proxy {} is not present on host {}",
            plan.candidate.tunnel, plan.candidate.host
        ));
    };
    if tunnel.status == TunnelRuntimeStatus::Listening {
        return None;
    }
    Some(match tunnel.status {
        TunnelRuntimeStatus::Starting => "proxy is starting".to_string(),
        TunnelRuntimeStatus::Listening => unreachable!(),
        TunnelRuntimeStatus::Error => tunnel
            .last_error
            .clone()
            .unwrap_or_else(|| "proxy listener failed".to_string()),
        TunnelRuntimeStatus::Stopped => "proxy is stopped".to_string(),
    })
}

fn chromium_proxy_url(plan: &ProxyEnvironmentPlan) -> String {
    match plan.scheme {
        ProxyEnvScheme::Socks5 | ProxyEnvScheme::Socks5h => {
            format!("socks5://{}", plan.address)
        }
        ProxyEnvScheme::Http | ProxyEnvScheme::Auto => format!("http://{}", plan.address),
    }
}

fn prepare_firefox_profile(profile_dir: &Path, proxy: &LauncherProxyPlan) -> anyhow::Result<()> {
    fs::create_dir_all(profile_dir).with_context(|| {
        format!("failed to create Firefox profile {}", profile_dir.display())
    })?;
    let preferences = match proxy {
        LauncherProxyPlan::Stk(plan) => match plan.scheme {
            ProxyEnvScheme::Http | ProxyEnvScheme::Auto => format!(
                "user_pref(\"network.proxy.type\", 1);\n\
                 user_pref(\"network.proxy.http\", \"{}\");\n\
                 user_pref(\"network.proxy.http_port\", {});\n\
                 user_pref(\"network.proxy.ssl\", \"{}\");\n\
                 user_pref(\"network.proxy.ssl_port\", {});\n\
                 user_pref(\"network.proxy.no_proxies_on\", \"\");\n",
                plan.address.ip(),
                plan.address.port(),
                plan.address.ip(),
                plan.address.port()
            ),
            ProxyEnvScheme::Socks5 | ProxyEnvScheme::Socks5h => format!(
                "user_pref(\"network.proxy.type\", 1);\n\
                 user_pref(\"network.proxy.socks\", \"{}\");\n\
                 user_pref(\"network.proxy.socks_port\", {});\n\
                 user_pref(\"network.proxy.socks_version\", 5);\n\
                 user_pref(\"network.proxy.socks_remote_dns\", {});\n\
                 user_pref(\"network.proxy.no_proxies_on\", \"\");\n",
                plan.address.ip(),
                plan.address.port(),
                plan.scheme == ProxyEnvScheme::Socks5h
            ),
        },
        LauncherProxyPlan::Direct => {
            "user_pref(\"network.proxy.type\", 0);\n".to_string()
        }
        LauncherProxyPlan::Inherit => String::new(),
    };
    fs::write(profile_dir.join("user.js"), preferences).with_context(|| {
        format!("failed to update Firefox profile {}", profile_dir.display())
    })
}

fn expand_proxy_arguments(
    arguments: &[String],
    proxy: &LauncherProxyPlan,
    profile_dir: Option<&Path>,
) -> anyhow::Result<Vec<OsString>> {
    let mut expanded = Vec::with_capacity(arguments.len());
    for argument in arguments {
        let mut value = argument.clone();
        if let LauncherProxyPlan::Stk(plan) = proxy {
            value = value
                .replace("{proxy-url}", &plan.url)
                .replace("{proxy-scheme}", proxy_scheme_name(plan.scheme))
                .replace("{proxy-host}", &plan.address.ip().to_string())
                .replace("{proxy-port}", &plan.address.port().to_string());
        } else if value.contains("{proxy-") {
            bail!("proxy argument template requires an STK proxy");
        }
        if let Some(profile_dir) = profile_dir {
            value = value.replace("{profile-dir}", &profile_dir.to_string_lossy());
        } else if value.contains("{profile-dir}") {
            bail!("profile-dir template is only available to browsers");
        }
        expanded.push(value.into());
    }
    Ok(expanded)
}

fn detect_browsers() -> Vec<DetectedBrowser> {
    browser_candidates()
        .into_iter()
        .filter_map(|candidate| {
            candidate.commands.iter().find_map(|command| find_executable(command)).map(|command| DetectedBrowser {
                id: candidate.id,
                name: candidate.name,
                engine: candidate.engine,
                command,
                order: candidate.order,
            })
        })
        .collect()
}

struct BrowserCandidate {
    id: &'static str,
    name: &'static str,
    engine: BrowserEngine,
    commands: Vec<String>,
    order: i32,
}

#[cfg(target_os = "macos")]
fn browser_candidates() -> Vec<BrowserCandidate> {
    vec![
        browser_candidate("chrome", "Google Chrome", BrowserEngine::Chromium, &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "~/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"], 10),
        browser_candidate("firefox", "Firefox", BrowserEngine::Firefox, &["/Applications/Firefox.app/Contents/MacOS/firefox", "~/Applications/Firefox.app/Contents/MacOS/firefox"], 20),
        browser_candidate("edge", "Microsoft Edge", BrowserEngine::Chromium, &["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge", "~/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"], 30),
        browser_candidate("brave", "Brave Browser", BrowserEngine::Chromium, &["/Applications/Brave Browser.app/Contents/MacOS/Brave Browser", "~/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"], 40),
        browser_candidate("chromium", "Chromium", BrowserEngine::Chromium, &["/Applications/Chromium.app/Contents/MacOS/Chromium", "~/Applications/Chromium.app/Contents/MacOS/Chromium"], 50),
        browser_candidate("vivaldi", "Vivaldi", BrowserEngine::Chromium, &["/Applications/Vivaldi.app/Contents/MacOS/Vivaldi", "~/Applications/Vivaldi.app/Contents/MacOS/Vivaldi"], 60),
        browser_candidate("opera", "Opera", BrowserEngine::Chromium, &["/Applications/Opera.app/Contents/MacOS/Opera", "~/Applications/Opera.app/Contents/MacOS/Opera"], 70),
    ]
}

#[cfg(target_os = "linux")]
fn browser_candidates() -> Vec<BrowserCandidate> {
    vec![
        browser_candidate("chrome", "Google Chrome", BrowserEngine::Chromium, &["google-chrome", "google-chrome-stable"], 10),
        browser_candidate("firefox", "Firefox", BrowserEngine::Firefox, &["firefox"], 20),
        browser_candidate("edge", "Microsoft Edge", BrowserEngine::Chromium, &["microsoft-edge", "microsoft-edge-stable"], 30),
        browser_candidate("brave", "Brave Browser", BrowserEngine::Chromium, &["brave-browser", "brave"], 40),
        browser_candidate("chromium", "Chromium", BrowserEngine::Chromium, &["chromium", "chromium-browser"], 50),
        browser_candidate("vivaldi", "Vivaldi", BrowserEngine::Chromium, &["vivaldi", "vivaldi-stable"], 60),
        browser_candidate("opera", "Opera", BrowserEngine::Chromium, &["opera"], 70),
    ]
}

#[cfg(target_os = "windows")]
fn browser_candidates() -> Vec<BrowserCandidate> {
    let local = env::var("LOCALAPPDATA").unwrap_or_default();
    let program_files = env::var("PROGRAMFILES").unwrap_or_default();
    let program_files_x86 = env::var("PROGRAMFILES(X86)").unwrap_or_default();
    vec![
        browser_candidate_owned("chrome", "Google Chrome", BrowserEngine::Chromium, vec![format!(r"{local}\Google\Chrome\Application\chrome.exe"), format!(r"{program_files}\Google\Chrome\Application\chrome.exe"), "chrome.exe".to_string()], 10),
        browser_candidate_owned("firefox", "Firefox", BrowserEngine::Firefox, vec![format!(r"{program_files}\Mozilla Firefox\firefox.exe"), format!(r"{program_files_x86}\Mozilla Firefox\firefox.exe"), "firefox.exe".to_string()], 20),
        browser_candidate_owned("edge", "Microsoft Edge", BrowserEngine::Chromium, vec![format!(r"{program_files_x86}\Microsoft\Edge\Application\msedge.exe"), format!(r"{program_files}\Microsoft\Edge\Application\msedge.exe"), "msedge.exe".to_string()], 30),
        browser_candidate_owned("brave", "Brave Browser", BrowserEngine::Chromium, vec![format!(r"{program_files}\BraveSoftware\Brave-Browser\Application\brave.exe"), format!(r"{local}\BraveSoftware\Brave-Browser\Application\brave.exe"), "brave.exe".to_string()], 40),
        browser_candidate("chromium", "Chromium", BrowserEngine::Chromium, &["chromium.exe"], 50),
        browser_candidate_owned("vivaldi", "Vivaldi", BrowserEngine::Chromium, vec![format!(r"{local}\Vivaldi\Application\vivaldi.exe"), "vivaldi.exe".to_string()], 60),
        browser_candidate_owned("opera", "Opera", BrowserEngine::Chromium, vec![format!(r"{local}\Programs\Opera\opera.exe"), "opera.exe".to_string()], 70),
    ]
}

fn browser_candidate(
    id: &'static str,
    name: &'static str,
    engine: BrowserEngine,
    commands: &[&str],
    order: i32,
) -> BrowserCandidate {
    browser_candidate_owned(
        id,
        name,
        engine,
        commands.iter().map(|command| (*command).to_string()).collect(),
        order,
    )
}

fn browser_candidate_owned(
    id: &'static str,
    name: &'static str,
    engine: BrowserEngine,
    commands: Vec<String>,
    order: i32,
) -> BrowserCandidate {
    BrowserCandidate {
        id,
        name,
        engine,
        commands,
        order,
    }
}

fn find_executable(command: &str) -> Option<PathBuf> {
    let path = expand_path(command);
    if command.contains('/') || command.contains('\\') || path.is_absolute() {
        return path.is_file().then_some(path);
    }
    let path_variable = env::var_os("PATH")?;
    let extensions = executable_extensions();
    for directory in env::split_paths(&path_variable) {
        for extension in &extensions {
            let candidate = directory.join(format!("{command}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn executable_extensions() -> Vec<String> {
    let mut extensions = vec![String::new()];
    extensions.extend(
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string())
            .split(';')
            .map(str::to_ascii_lowercase),
    );
    extensions
}

#[cfg(not(target_os = "windows"))]
fn executable_extensions() -> Vec<String> {
    vec![String::new()]
}

fn expand_path(value: &str) -> PathBuf {
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));
        if let Some(home) = home {
            let suffix = value
                .strip_prefix("~/")
                .or_else(|| value.strip_prefix("~\\"))
                .unwrap_or_default();
            return Path::new(&home).join(suffix);
        }
    }
    PathBuf::from(value)
}

fn sanitize_path_segment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    sanitized.trim_matches('-').to_string()
}

fn launcher_icon_text(configured: Option<&str>, name: &str) -> String {
    configured
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.chars().take(2).collect())
        .unwrap_or_else(|| {
            name.split_whitespace()
                .filter_map(|part| part.chars().next())
                .take(2)
                .collect::<String>()
                .to_ascii_uppercase()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_browser_item(engine: BrowserEngine, profile_dir: Option<String>) -> LauncherItem {
        LauncherItem {
            id: "browser:test".to_string(),
            name: "Test Browser".to_string(),
            icon_text: "TB".to_string(),
            kind: LauncherKind::Browser(engine),
            command: PathBuf::from("/bin/echo"),
            args: vec!["--user-argument".to_string()],
            normal_args: vec!["--normal-argument".to_string()],
            private_args: vec!["--private-argument".to_string()],
            proxy_args: Vec::new(),
            profile_dir,
            working_directory: None,
            environment: BTreeMap::new(),
            unset_environment: Vec::new(),
            proxy: Ok(LauncherProxyPlan::Inherit),
            order: 0,
        }
    }

    fn launcher_test_config() -> AppConfig {
        AppConfig::from_yaml_str(
            r#"
env:
  default: web
  profiles:
    web:
      host: alpha
      tunnel: proxy
      scheme: http
launchers:
  browsers:
    auto-discover: false
    entries:
      custom:
        engine: custom
        command: /bin/echo
        private-args: [--private]
  applications:
    entries:
      tool:
        command: /bin/echo
        proxy-args: ["--proxy={proxy-url}"]
hosts:
  alpha:
    host: alpha.example
    inherit-ssh-config-forwards: false
    local-proxies:
      - name: proxy
        listen: 0.0.0.0:7890
        mixed: true
"#,
        )
        .unwrap()
    }

    #[test]
    fn proxy_selector_and_wildcard_listener_resolve_for_launchers() {
        let config = launcher_test_config();
        let plan = resolve_launcher_proxy(&config, Some("web")).unwrap();
        let LauncherProxyPlan::Stk(plan) = plan else {
            panic!("expected an STK proxy plan");
        };
        assert_eq!(plan.url, "http://127.0.0.1:7890");
        assert_eq!(plan.set["HTTP_PROXY"], plan.url);
    }

    #[test]
    fn browser_and_application_configs_build_separate_catalog_entries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        fs::write(path.clone(), launcher_test_config().to_yaml_string().unwrap()).unwrap();
        let catalog = LauncherCatalog::load(&path);
        assert!(catalog.error.is_none());
        assert_eq!(catalog.items.len(), 2);
        assert!(catalog.items.iter().any(LauncherItem::is_browser));
        assert!(catalog.items.iter().any(|item| !item.is_browser()));
    }

    #[test]
    fn custom_application_proxy_arguments_are_expanded() {
        let config = launcher_test_config();
        let proxy = resolve_launcher_proxy(&config, Some("web")).unwrap();
        let arguments = expand_proxy_arguments(
            &["--proxy={proxy-url}".to_string()],
            &proxy,
            None,
        )
        .unwrap();
        assert_eq!(arguments, [OsString::from("--proxy=http://127.0.0.1:7890")]);
    }

    #[test]
    fn chromium_reuses_default_profile_and_only_appends_mode_and_proxy_arguments() {
        let config = launcher_test_config();
        let proxy = resolve_launcher_proxy(&config, Some("web")).unwrap();
        let item = test_browser_item(BrowserEngine::Chromium, None);

        let normal = item
            .build_launch_plan(LaunchMode::Normal, &proxy)
            .unwrap();
        assert_eq!(
            normal.args,
            [
                OsString::from("--user-argument"),
                OsString::from("--proxy-server=http://127.0.0.1:7890"),
                OsString::from("--normal-argument"),
            ]
        );

        let private = item
            .build_launch_plan(LaunchMode::Private, &proxy)
            .unwrap();
        assert_eq!(
            private.args,
            [
                OsString::from("--user-argument"),
                OsString::from("--proxy-server=http://127.0.0.1:7890"),
                OsString::from("--incognito"),
                OsString::from("--private-argument"),
            ]
        );
    }

    #[test]
    fn chromium_uses_profile_directory_only_when_explicitly_configured() {
        let directory = tempfile::tempdir().unwrap();
        let profile_dir = directory.path().join("profile");
        let item = test_browser_item(
            BrowserEngine::Chromium,
            Some(profile_dir.to_string_lossy().into_owned()),
        );

        let plan = item
            .build_launch_plan(LaunchMode::Normal, &LauncherProxyPlan::Inherit)
            .unwrap();
        assert_eq!(
            plan.args,
            [
                OsString::from("--user-argument"),
                OsString::from(format!("--user-data-dir={}", profile_dir.display())),
                OsString::from("--normal-argument"),
            ]
        );
        assert!(profile_dir.is_dir());
    }
}
