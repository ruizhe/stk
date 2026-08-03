use anyhow::{Context as _, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fmt, fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
};
use stk_core::{
    AppConfig, ApplicationLauncherEntryConfig, BrowserEngine, BrowserLauncherEntryConfig,
    EnvProfileConfig, LocalProxyCandidate, ProxyEnvScheme, ProxyEnvVariable,
    config::ProxyProtocol,
    stats::{RuntimeSnapshot, TunnelKind, TunnelRuntimeStatus},
};
use sysinfo::System;

#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSArray, NSDictionary, NSError, NSString, NSURL};
#[cfg(target_os = "macos")]
use std::sync::mpsc;

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
    DefaultProfile,
}

#[derive(Debug)]
pub(super) enum LauncherLaunchError {
    DefaultProfileAlreadyRunning,
    Other(anyhow::Error),
}

impl fmt::Display for LauncherLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultProfileAlreadyRunning => {
                formatter.write_str("the browser default profile is already running")
            }
            Self::Other(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for LauncherLaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DefaultProfileAlreadyRunning => None,
            Self::Other(error) => Some(error.as_ref()),
        }
    }
}

impl From<anyhow::Error> for LauncherLaunchError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
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
    pub icon_source: Option<String>,
    pub private_icon_text: String,
    pub private_icon_source: Option<String>,
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

    pub fn supports_default_profile(&self) -> bool {
        matches!(self.kind, LauncherKind::Browser(BrowserEngine::Chromium))
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

    pub fn launch(&self, mode: LaunchMode) -> Result<(), LauncherLaunchError> {
        let proxy = self
            .proxy
            .as_ref()
            .map_err(|error| LauncherLaunchError::Other(anyhow::anyhow!(error.clone())))?;
        if mode == LaunchMode::DefaultProfile {
            if !self.supports_default_profile() {
                return Err(LauncherLaunchError::Other(anyhow::anyhow!(
                    "the browser does not support proxy-only default-profile launching"
                )));
            }
            if default_profile_browser_is_running(&self.command) {
                return Err(LauncherLaunchError::DefaultProfileAlreadyRunning);
            }
        }
        let plan = self.build_launch_plan(mode, proxy)?;
        plan.spawn()?;
        Ok(())
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
                if mode != LaunchMode::DefaultProfile
                    && let Some(profile_dir) = configured_profile_dir.as_ref()
                {
                    fs::create_dir_all(profile_dir).with_context(|| {
                        format!("failed to create browser profile {}", profile_dir.display())
                    })?;
                    args.push(format!("--user-data-dir={}", profile_dir.display()).into());
                }
                if let LauncherProxyPlan::Stk(plan) = proxy {
                    args.push(format!("--proxy-server={}", chromium_proxy_url(plan)).into());
                }
                match mode {
                    LaunchMode::Normal | LaunchMode::DefaultProfile => {
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
                    .unwrap_or_else(|| self.default_managed_profile_directory());
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
                    LaunchMode::DefaultProfile => {
                        bail!("Firefox does not support proxy-only default-profile launching")
                    }
                }
            }
            LauncherKind::Browser(BrowserEngine::Custom) => {
                let profile_dir = configured_profile_dir.as_deref();
                if let Some(profile_dir) = profile_dir {
                    fs::create_dir_all(profile_dir).with_context(|| {
                        format!("failed to create browser profile {}", profile_dir.display())
                    })?;
                }
                args.extend(expand_proxy_arguments(
                    &self.proxy_args,
                    proxy,
                    profile_dir,
                )?);
                args.extend(match mode {
                    LaunchMode::Normal | LaunchMode::DefaultProfile => {
                        self.normal_args.iter().map(OsString::from)
                    }
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
            working_directory: self.working_directory.as_deref().map(expand_path),
            environment_set,
            environment_remove,
            #[cfg(any(target_os = "macos", test))]
            launch_as_browser_application: self.is_browser(),
        })
    }

    fn default_managed_profile_directory(&self) -> PathBuf {
        let launcher_id = self.id.strip_prefix("browser:").unwrap_or(&self.id);
        managed_browser_profile_directory("firefox", launcher_id)
    }
}

fn default_profile_browser_is_running(command: &Path) -> bool {
    let system = System::new_all();
    system.processes().values().any(|process| {
        is_default_profile_browser_process(command, process.exe(), process.name(), process.cmd())
    })
}

fn is_default_profile_browser_process(
    command: &Path,
    process_executable: Option<&Path>,
    process_name: &OsStr,
    command_line: &[OsString],
) -> bool {
    if !process_matches_executable(command, process_executable, process_name) {
        return false;
    }
    if command_line.iter().any(|argument| {
        let argument = argument.to_string_lossy();
        argument == "--type" || argument.starts_with("--type=")
    }) {
        return false;
    }
    !command_line.iter().any(|argument| {
        let argument = argument.to_string_lossy();
        argument == "--user-data-dir" || argument.starts_with("--user-data-dir=")
    })
}

fn process_matches_executable(
    command: &Path,
    process_executable: Option<&Path>,
    process_name: &OsStr,
) -> bool {
    if let Some(process_executable) = process_executable {
        return paths_match(command, process_executable);
    }
    command
        .file_name()
        .is_some_and(|command_name| os_strings_match(command_name, process_name))
}

fn paths_match(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let Ok(left) = fs::canonicalize(left) else {
        return false;
    };
    let Ok(right) = fs::canonicalize(right) else {
        return false;
    };
    os_strings_match(left.as_os_str(), right.as_os_str())
}

#[cfg(windows)]
fn os_strings_match(left: &OsStr, right: &OsStr) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn os_strings_match(left: &OsStr, right: &OsStr) -> bool {
    left == right
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LauncherProxyPlan {
    Stk(ProxyEnvironmentPlan),
    Direct,
    Inherit,
}

impl LauncherProxyPlan {
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
    #[cfg(any(target_os = "macos", test))]
    launch_as_browser_application: bool,
}

impl LaunchPlan {
    fn spawn(self) -> anyhow::Result<()> {
        #[cfg(target_os = "macos")]
        match self.macos_application_launch_request() {
            Ok(Some(request)) => return request.spawn(),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(
                    %error,
                    command = %self.command.display(),
                    "browser launch is not representable by LaunchServices; using direct execution"
                );
            }
        }
        self.spawn_direct()
    }

    fn spawn_direct(self) -> anyhow::Result<()> {
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
            format!(
                "failed to start launcher command {}",
                self.command.display()
            )
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

    #[cfg(any(target_os = "macos", test))]
    fn macos_application_launch_request(
        &self,
    ) -> anyhow::Result<Option<MacosApplicationLaunchRequest>> {
        if !self.launch_as_browser_application || self.working_directory.is_some() {
            return Ok(None);
        }
        let Some(application_bundle) = macos_application_bundle_path(&self.command) else {
            return Ok(None);
        };
        let arguments = self
            .args
            .iter()
            .map(|argument| {
                argument.to_str().map(str::to_string).with_context(|| {
                    format!(
                        "launcher argument for {} is not valid UTF-8",
                        self.command.display()
                    )
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let environment = resolved_launch_environment(
            &self.environment_set,
            &self.environment_remove,
            &self.command,
        )?;
        Ok(Some(MacosApplicationLaunchRequest {
            application_bundle,
            arguments,
            environment,
        }))
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct MacosApplicationLaunchRequest {
    application_bundle: PathBuf,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
}

#[cfg(target_os = "macos")]
impl MacosApplicationLaunchRequest {
    fn spawn(self) -> anyhow::Result<()> {
        let application_path = self
            .application_bundle
            .to_str()
            .with_context(|| {
                format!(
                    "macOS application path is not valid UTF-8: {}",
                    self.application_bundle.display()
                )
            })?
            .to_string();

        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        let application_name = self.application_bundle.display().to_string();
        let completion_application_name = application_name.clone();
        dispatch2::DispatchQueue::main().exec_async(move || {
            let application_url =
                NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&application_path), true);
            let configuration = NSWorkspaceOpenConfiguration::configuration();
            configuration.setCreatesNewApplicationInstance(true);
            configuration.setAllowsRunningApplicationSubstitution(false);
            configuration.setAddsToRecentItems(false);

            let arguments = self
                .arguments
                .iter()
                .map(|argument| NSString::from_str(argument))
                .collect::<Vec<_>>();
            configuration.setArguments(&NSArray::from_retained_slice(&arguments));

            let environment_keys = self
                .environment
                .keys()
                .map(|name| NSString::from_str(name))
                .collect::<Vec<_>>();
            let environment_values = self
                .environment
                .values()
                .map(|value| NSString::from_str(value))
                .collect::<Vec<_>>();
            let environment_key_refs = environment_keys
                .iter()
                .map(|value| &**value)
                .collect::<Vec<_>>();
            let environment_value_refs = environment_values
                .iter()
                .map(|value| &**value)
                .collect::<Vec<_>>();
            let environment =
                NSDictionary::from_slices(&environment_key_refs, &environment_value_refs);
            configuration.setEnvironment(&environment);

            let completion: RcBlock<dyn Fn(*mut NSRunningApplication, *mut NSError)> = RcBlock::new(
                move |application: *mut NSRunningApplication, error: *mut NSError| {
                    let result = if let Some(error) = unsafe { error.as_ref() } {
                        Err(error.localizedDescription().to_string())
                    } else if application.is_null() {
                        Err(
                            "LaunchServices returned neither an application nor an error"
                                .to_string(),
                        )
                    } else {
                        Ok(())
                    };
                    if completion_tx.send(result).is_err() {
                        tracing::debug!(
                            application = %completion_application_name,
                            "LaunchServices completion arrived after the launcher stopped waiting"
                        );
                    }
                },
            );
            NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
                &application_url,
                &configuration,
                Some(&completion),
            );
        });
        match completion_rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => bail!("LaunchServices failed to open {application_name}: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("timed out waiting for LaunchServices to open {application_name}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("LaunchServices completion channel closed while opening {application_name}")
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn macos_application_bundle_path(command: &Path) -> Option<PathBuf> {
    let expanded = command
        .to_str()
        .map(expand_path)
        .unwrap_or_else(|| command.to_path_buf());
    expanded
        .ancestors()
        .find(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        })
        .map(Path::to_path_buf)
}

#[cfg(any(target_os = "macos", test))]
fn resolved_launch_environment(
    environment_set: &BTreeMap<String, String>,
    environment_remove: &[String],
    command: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut environment = env::vars_os().collect::<BTreeMap<OsString, OsString>>();
    for name in environment_remove {
        environment.remove(OsStr::new(name));
    }
    for (name, value) in environment_set {
        environment.insert(OsString::from(name), OsString::from(value));
    }
    environment
        .into_iter()
        .map(|(name, value)| {
            let name = name.into_string().map_err(|name| {
                anyhow::anyhow!(
                    "environment name {:?} for {} is not valid UTF-8",
                    name,
                    command.display()
                )
            })?;
            let value = value.into_string().map_err(|value| {
                anyhow::anyhow!("environment value for {name} ({value:?}) is not valid UTF-8")
            })?;
            Ok((name, value))
        })
        .collect()
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
    let config = AppConfig::from_path(path).with_context(|| {
        format!(
            "failed to load launcher configuration from {}",
            path.display()
        )
    })?;
    config.validate()?;
    let config_directory = path.parent().unwrap_or_else(|| Path::new("."));
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
        items.push(resolve_browser_item(
            &config,
            config_directory,
            id,
            browser,
            detected_browser,
        ));
    }

    if config.launchers.browsers.auto_discover {
        for browser in detected {
            if configured_ids.contains(browser.id) {
                continue;
            }
            let entry = BrowserLauncherEntryConfig::default();
            items.push(resolve_browser_item(
                &config,
                config_directory,
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
        items.push(resolve_application_item(
            &config,
            config_directory,
            id,
            application,
        ));
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
    config_directory: &Path,
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
    let browser_icon_id = entry
        .detect
        .as_deref()
        .filter(|candidate| is_builtin_browser_id(candidate))
        .or_else(|| detected.map(|browser| browser.id))
        .or_else(|| is_builtin_browser_id(id).then_some(id));
    let icon_source = configured_icon_source(entry.icon_path.as_deref(), config_directory)
        .or_else(|| {
            browser_icon_id.and_then(|browser_id| system_browser_icon(browser_id, &command))
        })
        .or_else(|| browser_icon_id.and_then(|browser_id| bundled_browser_icon(browser_id, false)));
    let private_icon_source =
        configured_icon_source(entry.private_icon_path.as_deref(), config_directory)
            .or_else(|| {
                browser_icon_id.and_then(|browser_id| bundled_browser_icon(browser_id, true))
            })
            .or_else(|| bundled_engine_private_icon(engine));
    let profile_dir = entry.profile_dir.clone().or_else(|| {
        let browser_family = browser_icon_id.or(match engine {
            BrowserEngine::Chromium => Some("chromium"),
            BrowserEngine::Firefox => Some("firefox"),
            BrowserEngine::Custom => None,
        })?;
        Some(
            managed_browser_profile_directory(browser_family, id)
                .to_string_lossy()
                .into_owned(),
        )
    });
    let proxy_source = effective_proxy_source(
        entry.proxy.as_deref(),
        config.launchers.browsers.default_proxy.as_deref(),
        config.launchers.default_proxy.as_deref(),
        config.env.default.as_deref(),
    );
    LauncherItem {
        id: format!("browser:{id}"),
        icon_text: launcher_icon_text(entry.icon.as_deref(), &name),
        icon_source,
        private_icon_text: launcher_icon_text(entry.private_icon.as_deref(), "Private"),
        private_icon_source,
        name,
        kind: LauncherKind::Browser(engine),
        command,
        args: entry.args.clone(),
        normal_args: entry.normal_args.clone(),
        private_args: entry.private_args.clone(),
        proxy_args: entry.proxy_args.clone(),
        profile_dir,
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
    config_directory: &Path,
    id: &str,
    entry: &ApplicationLauncherEntryConfig,
) -> LauncherItem {
    let name = entry.name.clone().unwrap_or_else(|| id.to_string());
    let command = find_executable(&entry.command).unwrap_or_else(|| expand_path(&entry.command));
    let icon_source = configured_icon_source(entry.icon_path.as_deref(), config_directory)
        .or_else(|| system_application_icon(&command));
    let proxy_source = effective_proxy_source(
        entry.proxy.as_deref(),
        config.launchers.applications.default_proxy.as_deref(),
        config.launchers.default_proxy.as_deref(),
        config.env.default.as_deref(),
    );
    LauncherItem {
        id: format!("application:{id}"),
        icon_text: launcher_icon_text(entry.icon.as_deref(), &name),
        icon_source,
        private_icon_text: String::new(),
        private_icon_source: None,
        name,
        kind: LauncherKind::Application,
        command,
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
    fs::create_dir_all(profile_dir)
        .with_context(|| format!("failed to create Firefox profile {}", profile_dir.display()))?;
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
        LauncherProxyPlan::Direct => "user_pref(\"network.proxy.type\", 0);\n".to_string(),
        LauncherProxyPlan::Inherit => String::new(),
    };
    fs::write(profile_dir.join("user.js"), preferences)
        .with_context(|| format!("failed to update Firefox profile {}", profile_dir.display()))
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
            candidate
                .commands
                .iter()
                .find_map(|command| find_executable(command))
                .map(|command| DetectedBrowser {
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
        browser_candidate(
            "chrome",
            "Google Chrome",
            BrowserEngine::Chromium,
            &[
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "~/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            ],
            10,
        ),
        browser_candidate(
            "firefox",
            "Firefox",
            BrowserEngine::Firefox,
            &[
                "/Applications/Firefox.app/Contents/MacOS/firefox",
                "~/Applications/Firefox.app/Contents/MacOS/firefox",
            ],
            20,
        ),
        browser_candidate(
            "edge",
            "Microsoft Edge",
            BrowserEngine::Chromium,
            &[
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
                "~/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            ],
            30,
        ),
        browser_candidate(
            "brave",
            "Brave Browser",
            BrowserEngine::Chromium,
            &[
                "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
                "~/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            ],
            40,
        ),
        browser_candidate(
            "chromium",
            "Chromium",
            BrowserEngine::Chromium,
            &[
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
                "~/Applications/Chromium.app/Contents/MacOS/Chromium",
            ],
            50,
        ),
        browser_candidate(
            "vivaldi",
            "Vivaldi",
            BrowserEngine::Chromium,
            &[
                "/Applications/Vivaldi.app/Contents/MacOS/Vivaldi",
                "~/Applications/Vivaldi.app/Contents/MacOS/Vivaldi",
            ],
            60,
        ),
        browser_candidate(
            "opera",
            "Opera",
            BrowserEngine::Chromium,
            &[
                "/Applications/Opera.app/Contents/MacOS/Opera",
                "~/Applications/Opera.app/Contents/MacOS/Opera",
            ],
            70,
        ),
    ]
}

#[cfg(target_os = "linux")]
fn browser_candidates() -> Vec<BrowserCandidate> {
    vec![
        browser_candidate(
            "chrome",
            "Google Chrome",
            BrowserEngine::Chromium,
            &["google-chrome", "google-chrome-stable"],
            10,
        ),
        browser_candidate(
            "firefox",
            "Firefox",
            BrowserEngine::Firefox,
            &["firefox"],
            20,
        ),
        browser_candidate(
            "edge",
            "Microsoft Edge",
            BrowserEngine::Chromium,
            &["microsoft-edge", "microsoft-edge-stable"],
            30,
        ),
        browser_candidate(
            "brave",
            "Brave Browser",
            BrowserEngine::Chromium,
            &["brave-browser", "brave"],
            40,
        ),
        browser_candidate(
            "chromium",
            "Chromium",
            BrowserEngine::Chromium,
            &["chromium", "chromium-browser"],
            50,
        ),
        browser_candidate(
            "vivaldi",
            "Vivaldi",
            BrowserEngine::Chromium,
            &["vivaldi", "vivaldi-stable"],
            60,
        ),
        browser_candidate("opera", "Opera", BrowserEngine::Chromium, &["opera"], 70),
    ]
}

#[cfg(target_os = "windows")]
fn browser_candidates() -> Vec<BrowserCandidate> {
    let local = env::var("LOCALAPPDATA").unwrap_or_default();
    let program_files = env::var("PROGRAMFILES").unwrap_or_default();
    let program_files_x86 = env::var("PROGRAMFILES(X86)").unwrap_or_default();
    vec![
        browser_candidate_owned(
            "chrome",
            "Google Chrome",
            BrowserEngine::Chromium,
            vec![
                format!(r"{local}\Google\Chrome\Application\chrome.exe"),
                format!(r"{program_files}\Google\Chrome\Application\chrome.exe"),
                "chrome.exe".to_string(),
            ],
            10,
        ),
        browser_candidate_owned(
            "firefox",
            "Firefox",
            BrowserEngine::Firefox,
            vec![
                format!(r"{program_files}\Mozilla Firefox\firefox.exe"),
                format!(r"{program_files_x86}\Mozilla Firefox\firefox.exe"),
                "firefox.exe".to_string(),
            ],
            20,
        ),
        browser_candidate_owned(
            "edge",
            "Microsoft Edge",
            BrowserEngine::Chromium,
            vec![
                format!(r"{program_files_x86}\Microsoft\Edge\Application\msedge.exe"),
                format!(r"{program_files}\Microsoft\Edge\Application\msedge.exe"),
                "msedge.exe".to_string(),
            ],
            30,
        ),
        browser_candidate_owned(
            "brave",
            "Brave Browser",
            BrowserEngine::Chromium,
            vec![
                format!(r"{program_files}\BraveSoftware\Brave-Browser\Application\brave.exe"),
                format!(r"{local}\BraveSoftware\Brave-Browser\Application\brave.exe"),
                "brave.exe".to_string(),
            ],
            40,
        ),
        browser_candidate(
            "chromium",
            "Chromium",
            BrowserEngine::Chromium,
            &["chromium.exe"],
            50,
        ),
        browser_candidate_owned(
            "vivaldi",
            "Vivaldi",
            BrowserEngine::Chromium,
            vec![
                format!(r"{local}\Vivaldi\Application\vivaldi.exe"),
                "vivaldi.exe".to_string(),
            ],
            60,
        ),
        browser_candidate_owned(
            "opera",
            "Opera",
            BrowserEngine::Chromium,
            vec![
                format!(r"{local}\Programs\Opera\opera.exe"),
                "opera.exe".to_string(),
            ],
            70,
        ),
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
        commands
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
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

fn managed_browser_profile_directory(browser_family: &str, launcher_id: &str) -> PathBuf {
    let browser_family = sanitize_path_segment(browser_family);
    let launcher_id = sanitize_path_segment(launcher_id);
    let profile_name = if launcher_id.is_empty() || launcher_id == browser_family {
        "default".to_string()
    } else {
        launcher_id
    };
    browser_profile_data_root()
        .join(if browser_family.is_empty() {
            "browser"
        } else {
            &browser_family
        })
        .join(profile_name)
}

#[cfg(target_os = "macos")]
fn browser_profile_data_root() -> PathBuf {
    user_home_directory()
        .join("Library/Application Support/STK")
        .join("browser-profiles")
}

#[cfg(target_os = "linux")]
fn browser_profile_data_root() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home_directory().join(".local/share"))
        .join("stk/browser-profiles")
}

#[cfg(target_os = "windows")]
fn browser_profile_data_root() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home_directory().join("AppData/Local"))
        .join("STK/browser-profiles")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn browser_profile_data_root() -> PathBuf {
    user_home_directory().join(".local/share/stk/browser-profiles")
}

fn user_home_directory() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"))
}

#[derive(Clone)]
struct CachedIcon {
    modified: Option<std::time::SystemTime>,
    length: u64,
    source: Option<String>,
}

static ICON_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedIcon>>> = OnceLock::new();
static BUNDLED_ICON_CACHE: OnceLock<HashMap<&'static str, String>> = OnceLock::new();

fn configured_icon_source(configured: Option<&str>, config_directory: &Path) -> Option<String> {
    let configured = configured?.trim();
    if configured.is_empty() {
        return None;
    }
    let expanded = expand_path(configured);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        config_directory.join(expanded)
    };
    let source = cached_icon_data_url(&path);
    if source.is_none() {
        tracing::debug!(path = %path.display(), "launcher icon could not be loaded");
    }
    source
}

fn cached_icon_data_url(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok();
    let length = metadata.len();
    let cache = ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.get(path)
            && cached.modified == modified
            && cached.length == length
        {
            return cached.source.clone();
        }
    }

    let source = load_icon_data_url(path);
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            path.to_path_buf(),
            CachedIcon {
                modified,
                length,
                source: source.clone(),
            },
        );
    source
}

fn load_icon_data_url(path: &Path) -> Option<String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();

    #[cfg(target_os = "macos")]
    if extension.eq_ignore_ascii_case("icns") {
        return convert_macos_icon(path).map(|bytes| icon_data_url("image/png", &bytes));
    }

    #[cfg(target_os = "windows")]
    if extension.eq_ignore_ascii_case("exe") {
        return extract_windows_icon(path).map(|bytes| icon_data_url("image/png", &bytes));
    }

    let mime = match extension.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => return None,
    };
    fs::read(path).ok().map(|bytes| icon_data_url(mime, &bytes))
}

fn icon_data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

#[cfg(target_os = "macos")]
fn convert_macos_icon(path: &Path) -> Option<Vec<u8>> {
    let directory = tempfile::tempdir().ok()?;
    let output = directory.path().join("launcher-icon.png");
    let status = Command::new("/usr/bin/sips")
        .args(["-s", "format", "png", "-z", "64", "64"])
        .arg(path)
        .arg("--out")
        .arg(&output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    status.success().then(|| fs::read(output).ok()).flatten()
}

#[cfg(target_os = "windows")]
fn extract_windows_icon(path: &Path) -> Option<Vec<u8>> {
    super::windows::extract_executable_icon(path)
}

#[cfg(target_os = "macos")]
fn system_browser_icon(_browser_id: &str, command: &Path) -> Option<String> {
    macos_bundle_icon_path(command).and_then(|path| cached_icon_data_url(&path))
}

#[cfg(target_os = "linux")]
fn system_browser_icon(browser_id: &str, command: &Path) -> Option<String> {
    linux_browser_icon_paths(browser_id, command)
        .into_iter()
        .find_map(|path| cached_icon_data_url(&path))
}

#[cfg(target_os = "windows")]
fn system_browser_icon(_browser_id: &str, command: &Path) -> Option<String> {
    cached_icon_data_url(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn system_browser_icon(_browser_id: &str, _command: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn system_application_icon(command: &Path) -> Option<String> {
    macos_bundle_icon_path(command).and_then(|path| cached_icon_data_url(&path))
}

#[cfg(target_os = "windows")]
fn system_application_icon(command: &Path) -> Option<String> {
    cached_icon_data_url(command)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn system_application_icon(_command: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn macos_bundle_icon_path(command: &Path) -> Option<PathBuf> {
    let bundle = macos_application_bundle_path(command)?;
    let bundle_name = bundle
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let resources = bundle.join("Contents/Resources");
    let mut candidates = fs::read_dir(resources)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("icns"))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let file_stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let rank = if file_name == "app.icns" {
            0
        } else if file_stem == bundle_name {
            1
        } else if file_stem.contains(&bundle_name) || bundle_name.contains(&file_stem) {
            2
        } else {
            3
        };
        (rank, file_name)
    });
    candidates.into_iter().next()
}

#[cfg(target_os = "linux")]
fn linux_browser_icon_paths(browser_id: &str, command: &Path) -> Vec<PathBuf> {
    let icon_names: &[&str] = match browser_id {
        "chrome" => &["google-chrome", "google-chrome-stable", "chrome"],
        "firefox" => &["firefox"],
        "edge" => &["microsoft-edge", "microsoft-edge-stable", "msedge"],
        "brave" => &["brave-browser", "brave"],
        "chromium" => &["chromium", "chromium-browser"],
        "vivaldi" => &["vivaldi", "vivaldi-stable"],
        "opera" => &["opera"],
        _ => &[],
    };
    let mut candidates = Vec::new();
    if let Some(directory) = command.parent() {
        for file_name in [
            "product_logo_128.png",
            "product_logo_64.png",
            "default128.png",
            "default64.png",
        ] {
            candidates.push(directory.join(file_name));
        }
    }

    let mut data_directories = Vec::new();
    if let Some(directory) = env::var_os("XDG_DATA_HOME") {
        data_directories.push(PathBuf::from(directory));
    } else if let Some(home) = env::var_os("HOME") {
        data_directories.push(Path::new(&home).join(".local/share"));
    }
    if let Some(directories) = env::var_os("XDG_DATA_DIRS") {
        data_directories.extend(env::split_paths(&directories));
    } else {
        data_directories.extend([
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]);
    }

    for directory in data_directories {
        for icon_name in icon_names {
            for size in ["scalable", "256x256", "128x128", "64x64", "48x48", "32x32"] {
                for extension in ["svg", "png", "webp"] {
                    candidates.push(
                        directory
                            .join("icons/hicolor")
                            .join(size)
                            .join("apps")
                            .join(format!("{icon_name}.{extension}")),
                    );
                }
            }
            for extension in ["svg", "png", "webp", "xpm"] {
                candidates.push(
                    directory
                        .join("pixmaps")
                        .join(format!("{icon_name}.{extension}")),
                );
            }
        }
    }
    candidates
}

fn is_builtin_browser_id(id: &str) -> bool {
    matches!(
        id,
        "chrome" | "firefox" | "edge" | "brave" | "chromium" | "vivaldi" | "opera"
    )
}

fn bundled_browser_icon(browser_id: &str, private: bool) -> Option<String> {
    let key = match (browser_id, private) {
        ("chrome", false) => "chrome",
        ("chrome", true) => "chrome-private",
        ("firefox", false) => "firefox",
        ("firefox", true) => "firefox-private",
        ("edge", false) => "edge",
        ("edge", true) => "edge-private",
        ("brave", false) => "brave",
        ("brave", true) => "brave-private",
        ("chromium", false) => "chromium",
        ("chromium", true) => "chromium-private",
        ("vivaldi", false) => "vivaldi",
        ("vivaldi", true) => "vivaldi-private",
        ("opera", false) => "opera",
        ("opera", true) => "opera-private",
        _ => return None,
    };
    bundled_icon_cache().get(key).cloned()
}

fn bundled_engine_private_icon(engine: BrowserEngine) -> Option<String> {
    let key = match engine {
        BrowserEngine::Firefox => "firefox-private",
        BrowserEngine::Chromium => "chromium-private",
        BrowserEngine::Custom => "private",
    };
    bundled_icon_cache().get(key).cloned()
}

fn bundled_icon_cache() -> &'static HashMap<&'static str, String> {
    BUNDLED_ICON_CACHE.get_or_init(|| {
        HashMap::from([
            (
                "chrome",
                embedded_svg(include_bytes!("../assets/launchers/chrome.svg")),
            ),
            (
                "chrome-private",
                embedded_svg(include_bytes!("../assets/launchers/chrome-private.svg")),
            ),
            (
                "firefox",
                embedded_svg(include_bytes!("../assets/launchers/firefox.svg")),
            ),
            (
                "firefox-private",
                embedded_svg(include_bytes!("../assets/launchers/firefox-private.svg")),
            ),
            (
                "edge",
                embedded_svg(include_bytes!("../assets/launchers/edge.svg")),
            ),
            (
                "edge-private",
                embedded_svg(include_bytes!("../assets/launchers/edge-private.svg")),
            ),
            (
                "brave",
                embedded_svg(include_bytes!("../assets/launchers/brave.svg")),
            ),
            (
                "brave-private",
                embedded_svg(include_bytes!("../assets/launchers/brave-private.svg")),
            ),
            (
                "chromium",
                embedded_svg(include_bytes!("../assets/launchers/chromium.svg")),
            ),
            (
                "chromium-private",
                embedded_svg(include_bytes!("../assets/launchers/chromium-private.svg")),
            ),
            (
                "vivaldi",
                embedded_svg(include_bytes!("../assets/launchers/vivaldi.svg")),
            ),
            (
                "vivaldi-private",
                embedded_svg(include_bytes!("../assets/launchers/vivaldi-private.svg")),
            ),
            (
                "opera",
                embedded_svg(include_bytes!("../assets/launchers/opera.svg")),
            ),
            (
                "opera-private",
                embedded_svg(include_bytes!("../assets/launchers/opera-private.svg")),
            ),
            (
                "private",
                embedded_svg(include_bytes!("../assets/launchers/private.svg")),
            ),
        ])
    })
}

fn embedded_svg(bytes: &[u8]) -> String {
    icon_data_url("image/svg+xml", bytes)
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
            icon_source: None,
            private_icon_text: "PB".to_string(),
            private_icon_source: bundled_engine_private_icon(engine),
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
        fs::write(
            path.clone(),
            launcher_test_config().to_yaml_string().unwrap(),
        )
        .unwrap();
        let catalog = LauncherCatalog::load(&path);
        assert!(catalog.error.is_none());
        assert_eq!(catalog.items.len(), 2);
        assert!(catalog.items.iter().any(LauncherItem::is_browser));
        assert!(catalog.items.iter().any(|item| !item.is_browser()));
    }

    #[test]
    fn bundled_browsers_use_distinct_normal_and_private_icons() {
        for browser_id in [
            "chrome", "firefox", "edge", "brave", "chromium", "vivaldi", "opera",
        ] {
            let normal = bundled_browser_icon(browser_id, false).unwrap();
            let private = bundled_browser_icon(browser_id, true).unwrap();
            assert!(normal.starts_with("data:image/svg+xml;base64,"));
            assert!(private.starts_with("data:image/svg+xml;base64,"));
            assert_ne!(normal, private, "{browser_id} must have a private icon");
        }
    }

    #[test]
    fn managed_browser_profiles_are_stable_per_browser_and_launcher() {
        assert_eq!(
            managed_browser_profile_directory("chrome", "chrome"),
            browser_profile_data_root().join("chrome/default")
        );
        assert_eq!(
            managed_browser_profile_directory("chrome", "chrome-production"),
            browser_profile_data_root().join("chrome/chrome-production")
        );
    }

    #[test]
    fn macos_application_bundle_is_resolved_from_browser_executable_paths() {
        assert_eq!(
            macos_application_bundle_path(Path::new(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
            )),
            Some(PathBuf::from("/Applications/Google Chrome.app"))
        );
        assert_eq!(
            macos_application_bundle_path(Path::new(
                "/Applications/A Browser With Spaces.app/Contents/MacOS/A Browser With Spaces"
            )),
            Some(PathBuf::from("/Applications/A Browser With Spaces.app"))
        );
        assert_eq!(
            macos_application_bundle_path(Path::new(
                "~/Applications/Test Browser.app/Contents/MacOS/Test Browser"
            )),
            Some(user_home_directory().join("Applications/Test Browser.app"))
        );
        assert_eq!(
            macos_application_bundle_path(Path::new("/usr/local/bin/custom-browser")),
            None
        );
    }

    #[test]
    fn detected_browser_uses_managed_profile_unless_user_overrides_it() {
        let command = env::current_exe().unwrap();
        let detected = DetectedBrowser {
            id: "chrome",
            name: "Google Chrome",
            engine: BrowserEngine::Chromium,
            command,
            order: 10,
        };
        let automatic = resolve_browser_item(
            &AppConfig::default(),
            Path::new("."),
            "chrome",
            &BrowserLauncherEntryConfig::default(),
            Some(&detected),
        );
        let expected_profile = managed_browser_profile_directory("chrome", "chrome")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            automatic.profile_dir.as_deref(),
            Some(expected_profile.as_str())
        );

        let configured = BrowserLauncherEntryConfig {
            profile_dir: Some("~/profiles/my-chrome".to_string()),
            ..BrowserLauncherEntryConfig::default()
        };
        let overridden = resolve_browser_item(
            &AppConfig::default(),
            Path::new("."),
            "chrome",
            &configured,
            Some(&detected),
        );
        assert_eq!(
            overridden.profile_dir.as_deref(),
            Some("~/profiles/my-chrome")
        );
    }

    #[test]
    fn custom_browser_can_configure_separate_relative_icon_paths() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("normal.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg"><circle r="1"/></svg>"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("private.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1"/></svg>"#,
        )
        .unwrap();
        let entry = BrowserLauncherEntryConfig {
            engine: Some(BrowserEngine::Custom),
            command: Some(env::current_exe().unwrap().to_string_lossy().into_owned()),
            private_args: vec!["--private".to_string()],
            icon_path: Some("normal.svg".to_string()),
            private_icon_path: Some("private.svg".to_string()),
            ..BrowserLauncherEntryConfig::default()
        };
        let item = resolve_browser_item(
            &AppConfig::default(),
            directory.path(),
            "custom",
            &entry,
            None,
        );
        assert!(
            item.icon_source
                .as_deref()
                .unwrap()
                .starts_with("data:image/svg+xml;base64,")
        );
        assert!(
            item.private_icon_source
                .as_deref()
                .unwrap()
                .starts_with("data:image/svg+xml;base64,")
        );
        assert_ne!(item.icon_source, item.private_icon_source);
    }

    #[test]
    fn custom_application_proxy_arguments_are_expanded() {
        let config = launcher_test_config();
        let proxy = resolve_launcher_proxy(&config, Some("web")).unwrap();
        let arguments =
            expand_proxy_arguments(&["--proxy={proxy-url}".to_string()], &proxy, None).unwrap();
        assert_eq!(arguments, [OsString::from("--proxy=http://127.0.0.1:7890")]);
    }

    #[test]
    fn chromium_without_a_resolved_profile_only_appends_mode_and_proxy_arguments() {
        let config = launcher_test_config();
        let proxy = resolve_launcher_proxy(&config, Some("web")).unwrap();
        let item = test_browser_item(BrowserEngine::Chromium, None);

        let normal = item.build_launch_plan(LaunchMode::Normal, &proxy).unwrap();
        assert_eq!(
            normal.args,
            [
                OsString::from("--user-argument"),
                OsString::from("--proxy-server=http://127.0.0.1:7890"),
                OsString::from("--normal-argument"),
            ]
        );

        let private = item.build_launch_plan(LaunchMode::Private, &proxy).unwrap();
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
    fn chromium_uses_the_same_profile_directory_for_both_modes() {
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
        let private_plan = item
            .build_launch_plan(LaunchMode::Private, &LauncherProxyPlan::Inherit)
            .unwrap();
        assert!(private_plan.args.contains(&OsString::from(format!(
            "--user-data-dir={}",
            profile_dir.display()
        ))));
        assert!(private_plan.args.contains(&OsString::from("--incognito")));
        assert!(profile_dir.is_dir());
    }

    #[test]
    fn launch_services_request_preserves_browser_arguments_and_environment() {
        let config = launcher_test_config();
        let proxy = resolve_launcher_proxy(&config, Some("web")).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let profile_dir = directory.path().join("Profile With Spaces");
        let mut item = test_browser_item(
            BrowserEngine::Chromium,
            Some(profile_dir.to_string_lossy().into_owned()),
        );
        item.command =
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");

        let plan = item.build_launch_plan(LaunchMode::Private, &proxy).unwrap();
        let request = plan
            .macos_application_launch_request()
            .unwrap()
            .expect("browser application should use LaunchServices");

        assert_eq!(
            request.application_bundle,
            PathBuf::from("/Applications/Google Chrome.app")
        );
        assert_eq!(
            request.arguments,
            [
                "--user-argument".to_string(),
                format!("--user-data-dir={}", profile_dir.display()),
                "--proxy-server=http://127.0.0.1:7890".to_string(),
                "--incognito".to_string(),
                "--private-argument".to_string(),
            ]
        );
        assert_eq!(
            request.environment.get("HTTP_PROXY").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            request
                .environment
                .get("STK_PROXY_HOST")
                .map(String::as_str),
            Some("alpha")
        );
    }

    #[test]
    fn launch_services_falls_back_for_working_directories_and_application_launchers() {
        let mut browser = test_browser_item(BrowserEngine::Chromium, None)
            .build_launch_plan(LaunchMode::Normal, &LauncherProxyPlan::Inherit)
            .unwrap();
        browser.command =
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
        browser.working_directory = Some(PathBuf::from("/tmp"));
        assert!(
            browser
                .macos_application_launch_request()
                .unwrap()
                .is_none()
        );

        browser.working_directory = None;
        browser.launch_as_browser_application = false;
        assert!(
            browser
                .macos_application_launch_request()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn chromium_default_profile_mode_only_omits_the_managed_profile_argument() {
        let config = launcher_test_config();
        let proxy = resolve_launcher_proxy(&config, Some("web")).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let profile_dir = directory.path().join("managed-profile");
        let item = test_browser_item(
            BrowserEngine::Chromium,
            Some(profile_dir.to_string_lossy().into_owned()),
        );

        let plan = item
            .build_launch_plan(LaunchMode::DefaultProfile, &proxy)
            .unwrap();
        assert_eq!(
            plan.args,
            [
                OsString::from("--user-argument"),
                OsString::from("--proxy-server=http://127.0.0.1:7890"),
                OsString::from("--normal-argument"),
            ]
        );
        assert!(!profile_dir.exists());
        assert!(item.supports_default_profile());
        assert!(!test_browser_item(BrowserEngine::Firefox, None).supports_default_profile());
    }

    #[test]
    fn default_profile_process_detection_ignores_managed_and_child_processes() {
        let command = Path::new("/Applications/Test Browser/Browser");
        let process_name = command.file_name().unwrap();
        let main_command = [command.as_os_str().to_os_string()];
        assert!(is_default_profile_browser_process(
            command,
            Some(command),
            process_name,
            &main_command,
        ));

        let managed_command = [
            command.as_os_str().to_os_string(),
            OsString::from("--user-data-dir=/tmp/stk-browser"),
        ];
        assert!(!is_default_profile_browser_process(
            command,
            Some(command),
            process_name,
            &managed_command,
        ));

        let child_command = [
            command.as_os_str().to_os_string(),
            OsString::from("--type=renderer"),
        ];
        assert!(!is_default_profile_browser_process(
            command,
            Some(command),
            process_name,
            &child_command,
        ));

        assert!(!is_default_profile_browser_process(
            command,
            Some(Path::new("/Other/Browser")),
            process_name,
            &main_command,
        ));
    }
}
