use anyhow::Context as _;
use hyper::http::uri::Authority;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as _, MapAccess, Visitor},
};
use std::{
    collections::{BTreeMap, HashSet},
    fmt, fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {format} config: {reason}")]
    Parse {
        format: ConfigFormat,
        reason: String,
    },
    #[error("failed to serialize {format} config: {reason}")]
    Serialize {
        format: ConfigFormat,
        reason: String,
    },
    #[error("unsupported config file extension: {0}")]
    UnsupportedFormat(PathBuf),
    #[error("at least one SSH host is required")]
    EmptyHosts,
    #[error("duplicate local listen address: {0}")]
    DuplicateLocalListen(SocketAddr),
    #[error("invalid SSH host {host}: {reason}")]
    InvalidHostConfig { host: String, reason: String },
    #[error("invalid control endpoint: {0}")]
    InvalidControlEndpoint(String),
    #[error("invalid proxy environment configuration: {0}")]
    InvalidEnvConfig(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Yaml,
    Json,
    Toml,
}

impl ConfigFormat {
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("yaml" | "yml") => Ok(Self::Yaml),
            Some("json") => Ok(Self::Json),
            Some("toml") => Ok(Self::Toml),
            _ => Err(ConfigError::UnsupportedFormat(path.to_path_buf())),
        }
    }
}

impl fmt::Display for ConfigFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Yaml => "YAML",
            Self::Json => "JSON",
            Self::Toml => "TOML",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default, skip_serializing_if = "ControlConfig::is_empty")]
    pub control: ControlConfig,
    #[serde(default, skip_serializing_if = "EnvConfig::is_empty")]
    pub env: EnvConfig,
    #[serde(default, skip_serializing_if = "OverrideDefaultConfig::is_empty")]
    pub override_default: OverrideDefaultConfig,
    #[serde(default = "default_hosts", deserialize_with = "deserialize_hosts")]
    pub hosts: BTreeMap<String, HostConfig>,
}

fn deserialize_hosts<'de, D>(deserializer: D) -> Result<BTreeMap<String, HostConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    struct HostsVisitor;

    impl<'de> Visitor<'de> for HostsVisitor {
        type Value = BTreeMap<String, HostConfig>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of uniquely named SSH hosts")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut hosts = BTreeMap::new();
            while let Some((host_name, host)) = map.next_entry::<String, HostConfig>()? {
                if hosts.insert(host_name.clone(), host).is_some() {
                    return Err(A::Error::custom(format!(
                        "duplicate SSH host name: {host_name}"
                    )));
                }
            }
            Ok(hosts)
        }
    }

    deserializer.deserialize_map(HostsVisitor)
}

impl AppConfig {
    pub fn from_str(input: &str, format: ConfigFormat) -> Result<Self, ConfigError> {
        let result = match format {
            ConfigFormat::Yaml => serde_yaml::from_str(input).map_err(|error| error.to_string()),
            ConfigFormat::Json => serde_json::from_str(input).map_err(|error| error.to_string()),
            ConfigFormat::Toml => toml::from_str(input).map_err(|error| error.to_string()),
        };
        result.map_err(|reason| ConfigError::Parse { format, reason })
    }

    pub fn from_yaml_str(input: &str) -> Result<Self, ConfigError> {
        Self::from_str(input, ConfigFormat::Yaml)
    }

    pub fn from_json_str(input: &str) -> Result<Self, ConfigError> {
        Self::from_str(input, ConfigFormat::Json)
    }

    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        Self::from_str(input, ConfigFormat::Toml)
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let input = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_str(&input, ConfigFormat::from_path(path)?)
    }

    pub fn to_string(&self, format: ConfigFormat) -> Result<String, ConfigError> {
        let result = match format {
            ConfigFormat::Yaml => serde_yaml::to_string(self).map_err(|error| error.to_string()),
            ConfigFormat::Json => {
                serde_json::to_string_pretty(self).map_err(|error| error.to_string())
            }
            ConfigFormat::Toml => toml::to_string_pretty(self).map_err(|error| error.to_string()),
        };
        result.map_err(|reason| ConfigError::Serialize { format, reason })
    }

    pub fn to_yaml_string(&self) -> Result<String, ConfigError> {
        self.to_string(ConfigFormat::Yaml)
    }

    pub fn to_json_string(&self) -> Result<String, ConfigError> {
        self.to_string(ConfigFormat::Json)
    }

    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        self.to_string(ConfigFormat::Toml)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(endpoint) = &self.control.endpoint {
            endpoint
                .parse::<crate::control::ControlEndpoint>()
                .map_err(|error| ConfigError::InvalidControlEndpoint(error.to_string()))?;
        }
        self.env.validate()?;
        if self.hosts.is_empty() {
            return Err(ConfigError::EmptyHosts);
        }

        let mut local_listens = HashSet::new();
        for (host_name, host) in &self.hosts {
            if host_name.trim().is_empty() {
                return Err(ConfigError::InvalidHostConfig {
                    host: host_name.clone(),
                    reason: "host name must not be empty".to_string(),
                });
            }
            let host = host.resolve(&self.override_default);
            if !host.auto {
                continue;
            }
            validate_host(host_name, &host, &mut local_listens)?;
        }
        Ok(())
    }

    pub fn resolved_local_proxies(&self) -> anyhow::Result<Vec<LocalProxyCandidate>> {
        let mut candidates = Vec::new();
        for (host_name, host) in &self.hosts {
            let mut host = host.resolve(&self.override_default);
            if !host.auto {
                continue;
            }
            crate::ssh_config::inherit_ssh_config_forwards(&mut host).with_context(|| {
                format!("failed to inherit SSH config forwards for host {host_name}")
            })?;
            candidates.extend(
                host.local_proxies
                    .iter()
                    .filter(|proxy| proxy.auto)
                    .map(|proxy| LocalProxyCandidate {
                        host: host_name.clone(),
                        tunnel: proxy.runtime_name("local-proxy"),
                        listen: proxy.listen,
                        protocol: proxy.resolved_protocol(),
                    }),
            );
        }
        Ok(candidates)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            control: ControlConfig::default(),
            env: EnvConfig::default(),
            override_default: OverrideDefaultConfig::default(),
            hosts: default_hosts(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ControlConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl ControlConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct EnvConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, EnvProfileConfig>,
}

impl EnvConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(default) = &self.default {
            if default.trim().is_empty() {
                return Err(ConfigError::InvalidEnvConfig(
                    "default profile name must not be empty".to_string(),
                ));
            }
            if !self.profiles.contains_key(default) {
                return Err(ConfigError::InvalidEnvConfig(format!(
                    "default profile {default} is not defined"
                )));
            }
        }
        for (name, profile) in &self.profiles {
            if name.trim().is_empty() {
                return Err(ConfigError::InvalidEnvConfig(
                    "profile name must not be empty".to_string(),
                ));
            }
            for (field, value) in [
                ("host", profile.host.as_deref()),
                ("tunnel", profile.tunnel.as_deref()),
            ] {
                if value.is_some_and(|value| value.trim().is_empty()) {
                    return Err(ConfigError::InvalidEnvConfig(format!(
                        "profile {name} has an empty {field}"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct EnvProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<ProxyEnvScheme>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyEnvScheme {
    #[default]
    Auto,
    Socks5h,
    Socks5,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProxyCandidate {
    pub host: String,
    pub tunnel: String,
    pub listen: SocketAddr,
    pub protocol: ProxyProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OverrideDefaultConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit_ssh_config_forwards: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SshAuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key_policy: Option<SshHostKeyPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hosts_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_sessions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_rotation_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_rotation_interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_channels_per_session: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_alive_count_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_initial_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_max_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_spawn_cooldown_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_drain_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "ProbeOverrideConfig::is_empty")]
    pub probe: ProbeOverrideConfig,
    #[serde(default, skip_serializing_if = "ProxyDefaultConfig::is_empty")]
    pub proxy: ProxyDefaultConfig,
    #[serde(default, skip_serializing_if = "ForwardDefaultConfig::is_empty")]
    pub forward: ForwardDefaultConfig,
}

impl OverrideDefaultConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct HostConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit_ssh_config_forwards: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SshAuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_config_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key_policy: Option<SshHostKeyPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hosts_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_sessions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_rotation_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_rotation_interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_channels_per_session: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_alive_count_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_initial_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_max_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_spawn_cooldown_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_drain_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "ProbeOverrideConfig::is_empty")]
    pub probe: ProbeOverrideConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_proxies: Vec<ProxyConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_forwards: Vec<ForwardConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_proxies: Vec<ProxyConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_forwards: Vec<ForwardConfig>,
}

impl HostConfig {
    pub(crate) fn resolve(&self, defaults: &OverrideDefaultConfig) -> ResolvedHostConfig {
        ResolvedHostConfig {
            auto: self.auto.or(defaults.auto).unwrap_or(true),
            inherit_ssh_config_forwards: self
                .inherit_ssh_config_forwards
                .or(defaults.inherit_ssh_config_forwards)
                .unwrap_or(true),
            host: self.host.clone(),
            port: self.port.or(defaults.port),
            username: self.username.clone().or_else(|| defaults.username.clone()),
            auth: self.auth.clone().or_else(|| defaults.auth.clone()),
            ssh_config_host: self.ssh_config_host.clone(),
            ssh_config_path: self
                .ssh_config_path
                .clone()
                .or_else(|| defaults.ssh_config_path.clone()),
            host_key_policy: self.host_key_policy.or(defaults.host_key_policy),
            known_hosts_path: self
                .known_hosts_path
                .clone()
                .or_else(|| defaults.known_hosts_path.clone()),
            keep_alive_secs: self.keep_alive_secs.or(defaults.keep_alive_secs),
            min_sessions: self
                .min_sessions
                .or(defaults.min_sessions)
                .unwrap_or_else(default_min_sessions),
            max_sessions: self
                .max_sessions
                .or(defaults.max_sessions)
                .unwrap_or_else(default_max_sessions),
            session_rotation_enabled: self
                .session_rotation_enabled
                .or(defaults.session_rotation_enabled)
                .unwrap_or_else(default_session_rotation_enabled),
            session_rotation_interval_secs: self
                .session_rotation_interval_secs
                .or(defaults.session_rotation_interval_secs)
                .unwrap_or_else(default_session_rotation_interval_secs),
            max_channels_per_session: self
                .max_channels_per_session
                .or(defaults.max_channels_per_session)
                .unwrap_or_else(default_max_channels_per_session),
            server_alive_count_max: self
                .server_alive_count_max
                .or(defaults.server_alive_count_max),
            connect_timeout_secs: self.connect_timeout_secs.or(defaults.connect_timeout_secs),
            restart_initial_millis: self
                .restart_initial_millis
                .or(defaults.restart_initial_millis)
                .unwrap_or_else(default_restart_initial_millis),
            restart_max_secs: self
                .restart_max_secs
                .or(defaults.restart_max_secs)
                .unwrap_or_else(default_restart_max_secs),
            session_spawn_cooldown_millis: self
                .session_spawn_cooldown_millis
                .or(defaults.session_spawn_cooldown_millis)
                .unwrap_or_else(default_session_spawn_cooldown_millis),
            session_drain_timeout_secs: self
                .session_drain_timeout_secs
                .or(defaults.session_drain_timeout_secs)
                .unwrap_or_else(default_session_drain_timeout_secs),
            probe: self.probe.resolve(&defaults.probe),
            local_proxies: self
                .local_proxies
                .iter()
                .map(|proxy| proxy.resolve(&defaults.proxy))
                .collect(),
            local_forwards: self
                .local_forwards
                .iter()
                .map(|forward| forward.resolve(&defaults.forward))
                .collect(),
            remote_proxies: self
                .remote_proxies
                .iter()
                .map(|proxy| proxy.resolve(&defaults.proxy))
                .collect(),
            remote_forwards: self
                .remote_forwards
                .iter()
                .map(|forward| forward.resolve(&defaults.forward))
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedHostConfig {
    pub auto: bool,
    pub inherit_ssh_config_forwards: bool,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth: Option<SshAuthConfig>,
    pub ssh_config_host: Option<String>,
    pub ssh_config_path: Option<String>,
    pub host_key_policy: Option<SshHostKeyPolicy>,
    pub known_hosts_path: Option<String>,
    pub keep_alive_secs: Option<u64>,
    pub min_sessions: usize,
    pub max_sessions: usize,
    pub session_rotation_enabled: bool,
    pub session_rotation_interval_secs: u64,
    pub max_channels_per_session: usize,
    pub server_alive_count_max: Option<u32>,
    pub connect_timeout_secs: Option<u64>,
    pub restart_initial_millis: u64,
    pub restart_max_secs: u64,
    pub session_spawn_cooldown_millis: u64,
    pub session_drain_timeout_secs: u64,
    pub probe: ProbeConfig,
    pub local_proxies: Vec<ResolvedProxyConfig>,
    pub local_forwards: Vec<ResolvedForwardConfig>,
    pub remote_proxies: Vec<ResolvedProxyConfig>,
    pub remote_forwards: Vec<ResolvedForwardConfig>,
}

impl ResolvedHostConfig {
    pub(crate) fn has_automatic_tunnels(&self) -> bool {
        self.local_proxies.iter().any(|proxy| proxy.auto)
            || self.local_forwards.iter().any(|forward| forward.auto)
            || self.remote_proxies.iter().any(|proxy| proxy.auto)
            || self.remote_forwards.iter().any(|forward| forward.auto)
    }

    pub(crate) fn runtime_pool(&self, host_name: &str) -> SshPoolConfig {
        let remote_proxies = self
            .remote_proxies
            .iter()
            .filter(|proxy| proxy.auto)
            .map(|proxy| SshRemoteForwardConfig::Dynamic {
                name: proxy.runtime_name("remote-proxy"),
                listen: proxy.listen,
                protocol: proxy.resolved_protocol(),
            });
        let remote_forwards = self
            .remote_forwards
            .iter()
            .filter(|forward| forward.auto)
            .map(|forward| SshRemoteForwardConfig::Tcp {
                name: forward.runtime_name("remote-forward"),
                listen: forward.listen,
                local_host: forward.target.host.clone(),
                local_port: forward.target.port,
            });
        SshPoolConfig {
            policy: LoadBalancePolicy::WeightedRtt,
            keep_alive_secs: self.keep_alive_secs,
            min_sessions_per_host: self.min_sessions,
            max_sessions_per_host: self.max_sessions,
            session_rotation_enabled: self.session_rotation_enabled,
            session_rotation_interval_secs: self.session_rotation_interval_secs,
            max_channels_per_session: self.max_channels_per_session,
            server_alive_count_max: self.server_alive_count_max,
            connect_timeout_secs: self.connect_timeout_secs,
            restart_initial_millis: self.restart_initial_millis,
            restart_max_secs: self.restart_max_secs,
            session_spawn_cooldown_millis: self.session_spawn_cooldown_millis,
            session_drain_timeout_secs: self.session_drain_timeout_secs,
            hosts: vec![SshHostConfig {
                name: host_name.to_string(),
                host: self.host.clone(),
                ssh_config_host: self.ssh_config_host.clone(),
                port: self.port,
                username: self.username.clone(),
                auth: self.auth.clone(),
                ssh_config_path: self.ssh_config_path.clone(),
                host_key_policy: self.host_key_policy,
                known_hosts_path: self.known_hosts_path.clone(),
                remote_forwards: remote_proxies.chain(remote_forwards).collect(),
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProxyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub listen: SocketAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mixed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProxyProtocol>,
}

impl ProxyConfig {
    fn resolve(&self, defaults: &ProxyDefaultConfig) -> ResolvedProxyConfig {
        ResolvedProxyConfig {
            auto: self.auto.or(defaults.auto).unwrap_or(true),
            name: self.name.clone(),
            listen: self.listen,
            mixed: self.mixed.or(defaults.mixed).unwrap_or(false),
            protocol: self.protocol.or(defaults.protocol),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProxyDefaultConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mixed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProxyProtocol>,
}

impl ProxyDefaultConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedProxyConfig {
    pub auto: bool,
    pub name: Option<String>,
    pub listen: SocketAddr,
    pub mixed: bool,
    pub protocol: Option<ProxyProtocol>,
}

impl ResolvedProxyConfig {
    pub fn resolved_protocol(&self) -> ProxyProtocol {
        if self.mixed {
            ProxyProtocol::Mixed
        } else {
            self.protocol.unwrap_or(ProxyProtocol::Socks5h)
        }
    }

    pub fn runtime_name(&self, prefix: &str) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{prefix}-{}", self.listen))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ForwardConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub listen: SocketAddr,
    pub target: HostPort,
}

impl ForwardConfig {
    fn resolve(&self, defaults: &ForwardDefaultConfig) -> ResolvedForwardConfig {
        ResolvedForwardConfig {
            auto: self.auto.or(defaults.auto).unwrap_or(true),
            name: self.name.clone(),
            listen: self.listen,
            target: self.target.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ForwardDefaultConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
}

impl ForwardDefaultConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedForwardConfig {
    pub auto: bool,
    pub name: Option<String>,
    pub listen: SocketAddr,
    pub target: HostPort,
}

impl ResolvedForwardConfig {
    pub fn runtime_name(&self, prefix: &str) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{prefix}-{}", self.listen))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

impl FromStr for HostPort {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let authority = Authority::from_str(input)
            .map_err(|error| format!("invalid host:port target {input}: {error}"))?;
        let port = authority
            .port_u16()
            .filter(|port| *port > 0)
            .ok_or_else(|| format!("target must include a non-zero port: {input}"))?;
        let host = authority.host().trim_matches(['[', ']']).to_string();
        if host.is_empty() {
            return Err(format!("target host must not be empty: {input}"));
        }
        Ok(Self { host, port })
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) {
            write!(formatter, "[{}]:{}", self.host, self.port)
        } else {
            write!(formatter, "{}:{}", self.host, self.port)
        }
    }
}

impl Serialize for HostPort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HostPort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyProtocol {
    Socks5h,
    Http,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SshHostKeyPolicy {
    #[default]
    KnownHosts,
    InsecureAcceptAny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SshAuthConfig {
    #[default]
    Agent,
    PrivateKey {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase_env: Option<String>,
    },
    Password {
        password_env: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProbeConfig {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(
        default = "default_probe_interval_secs",
        skip_serializing_if = "is_default_probe_interval_secs"
    )]
    pub interval_secs: u64,
    #[serde(
        default = "default_probe_timeout_millis",
        skip_serializing_if = "is_default_probe_timeout_millis"
    )]
    pub timeout_millis: u64,
    #[serde(
        default = "default_probe_fail_threshold",
        skip_serializing_if = "is_default_probe_fail_threshold"
    )]
    pub fail_threshold: u32,
    #[serde(
        default = "default_probe_recovery_threshold",
        skip_serializing_if = "is_default_probe_recovery_threshold"
    )]
    pub recovery_threshold: u32,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_probe_interval_secs(),
            timeout_millis: default_probe_timeout_millis(),
            fail_threshold: default_probe_fail_threshold(),
            recovery_threshold: default_probe_recovery_threshold(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProbeOverrideConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_threshold: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_threshold: Option<u32>,
}

impl ProbeOverrideConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    fn resolve(&self, defaults: &Self) -> ProbeConfig {
        ProbeConfig {
            enabled: self.enabled.or(defaults.enabled).unwrap_or(true),
            interval_secs: self
                .interval_secs
                .or(defaults.interval_secs)
                .unwrap_or_else(default_probe_interval_secs),
            timeout_millis: self
                .timeout_millis
                .or(defaults.timeout_millis)
                .unwrap_or_else(default_probe_timeout_millis),
            fail_threshold: self
                .fail_threshold
                .or(defaults.fail_threshold)
                .unwrap_or_else(default_probe_fail_threshold),
            recovery_threshold: self
                .recovery_threshold
                .or(defaults.recovery_threshold)
                .unwrap_or_else(default_probe_recovery_threshold),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SshPoolConfig {
    pub policy: LoadBalancePolicy,
    pub keep_alive_secs: Option<u64>,
    pub min_sessions_per_host: usize,
    pub max_sessions_per_host: usize,
    pub session_rotation_enabled: bool,
    pub session_rotation_interval_secs: u64,
    pub max_channels_per_session: usize,
    pub server_alive_count_max: Option<u32>,
    pub connect_timeout_secs: Option<u64>,
    pub restart_initial_millis: u64,
    pub restart_max_secs: u64,
    pub session_spawn_cooldown_millis: u64,
    pub session_drain_timeout_secs: u64,
    pub hosts: Vec<SshHostConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadBalancePolicy {
    WeightedRtt,
    #[cfg(test)]
    RoundRobin,
    #[cfg(test)]
    LeastLatency,
    #[cfg(test)]
    Failover,
}

#[derive(Debug, Clone)]
pub(crate) struct SshHostConfig {
    pub name: String,
    pub host: Option<String>,
    pub ssh_config_host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth: Option<SshAuthConfig>,
    pub ssh_config_path: Option<String>,
    pub host_key_policy: Option<SshHostKeyPolicy>,
    pub known_hosts_path: Option<String>,
    pub remote_forwards: Vec<SshRemoteForwardConfig>,
}

#[derive(Debug, Clone)]
pub(crate) enum SshRemoteForwardConfig {
    Tcp {
        name: String,
        listen: SocketAddr,
        local_host: String,
        local_port: u16,
    },
    Dynamic {
        name: String,
        listen: SocketAddr,
        protocol: ProxyProtocol,
    },
}

fn validate_host(
    host_name: &str,
    host: &ResolvedHostConfig,
    local_listens: &mut HashSet<SocketAddr>,
) -> Result<(), ConfigError> {
    let invalid = |reason: String| ConfigError::InvalidHostConfig {
        host: host_name.to_string(),
        reason,
    };
    if host.host.is_none() && host.ssh_config_host.is_none() {
        return Err(invalid(
            "either host or ssh-config-host must be configured".to_string(),
        ));
    }
    if host
        .host
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
        || host
            .ssh_config_host
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        || host
            .username
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
    {
        return Err(invalid(
            "host, ssh-config-host and an explicitly configured username must not be empty"
                .to_string(),
        ));
    }
    if host.port == Some(0) {
        return Err(invalid("SSH port must be greater than zero".to_string()));
    }
    if host.keep_alive_secs == Some(0) || host.server_alive_count_max == Some(0) {
        return Err(invalid(
            "keep_alive_secs and server_alive_count_max must be greater than zero".to_string(),
        ));
    }
    if host.min_sessions == 0 || host.max_sessions < host.min_sessions {
        return Err(invalid(
            "max_sessions must be greater than or equal to min_sessions, and both must be greater than zero"
                .to_string(),
        ));
    }
    if host.max_channels_per_session == 0
        || host.restart_initial_millis == 0
        || host.restart_max_secs == 0
        || host.session_spawn_cooldown_millis == 0
        || host.session_drain_timeout_secs == 0
        || host.session_rotation_interval_secs == 0
        || host.connect_timeout_secs == Some(0)
    {
        return Err(invalid(
            "session capacity, timeout and restart settings must be greater than zero".to_string(),
        ));
    }
    if host.probe.fail_threshold == 0 || host.probe.recovery_threshold == 0 {
        return Err(invalid(
            "probe thresholds must be greater than zero".to_string(),
        ));
    }
    if host
        .ssh_config_path
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
        || host
            .known_hosts_path
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
    {
        return Err(invalid(
            "SSH config and known_hosts paths must not be empty".to_string(),
        ));
    }
    if host.ssh_config_path.is_some() && host.ssh_config_host.is_none() {
        return Err(invalid(
            "ssh-config-path requires ssh-config-host".to_string(),
        ));
    }
    validate_auth(host, &invalid)?;

    let mut forward_names = HashSet::new();
    for (kind, proxy) in host
        .local_proxies
        .iter()
        .map(|value| ("local proxy", value))
        .chain(
            host.remote_proxies
                .iter()
                .map(|value| ("remote proxy", value)),
        )
    {
        validate_proxy(proxy, kind, &invalid)?;
        if let Some(name) = &proxy.name
            && !forward_names.insert(name.as_str())
        {
            return Err(invalid(format!("duplicate forward name {name}")));
        }
    }
    for (kind, forward) in host
        .local_forwards
        .iter()
        .map(|value| ("local forward", value))
        .chain(
            host.remote_forwards
                .iter()
                .map(|value| ("remote forward", value)),
        )
    {
        if forward.listen.port() == 0 {
            return Err(invalid(format!("{kind} listen port must be non-zero")));
        }
        if let Some(name) = &forward.name
            && (name.trim().is_empty() || !forward_names.insert(name.as_str()))
        {
            return Err(invalid(format!("invalid or duplicate forward name {name}")));
        }
    }

    for listen in host
        .local_proxies
        .iter()
        .filter(|forward| forward.auto)
        .map(|forward| forward.listen)
        .chain(
            host.local_forwards
                .iter()
                .filter(|forward| forward.auto)
                .map(|forward| forward.listen),
        )
    {
        if !local_listens.insert(listen) {
            return Err(ConfigError::DuplicateLocalListen(listen));
        }
    }
    let mut remote_ports = HashSet::new();
    for listen in host
        .remote_proxies
        .iter()
        .filter(|forward| forward.auto)
        .map(|forward| forward.listen)
        .chain(
            host.remote_forwards
                .iter()
                .filter(|forward| forward.auto)
                .map(|forward| forward.listen),
        )
    {
        if !remote_ports.insert(listen.port()) {
            return Err(invalid(format!(
                "duplicate remote listen port {}",
                listen.port()
            )));
        }
    }
    Ok(())
}

fn validate_proxy(
    proxy: &ResolvedProxyConfig,
    kind: &str,
    invalid: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    if proxy.listen.port() == 0 {
        return Err(invalid(format!("{kind} listen port must be non-zero")));
    }
    if proxy
        .name
        .as_ref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err(invalid(format!("{kind} name must not be empty")));
    }
    if proxy.mixed
        && proxy
            .protocol
            .is_some_and(|protocol| protocol != ProxyProtocol::Mixed)
    {
        return Err(invalid(format!(
            "{kind} cannot combine mixed: true with a non-mixed protocol"
        )));
    }
    Ok(())
}

fn validate_auth(
    host: &ResolvedHostConfig,
    invalid: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    match &host.auth {
        Some(SshAuthConfig::PrivateKey {
            path,
            passphrase_env,
        }) if path.trim().is_empty()
            || passphrase_env
                .as_ref()
                .is_some_and(|name| name.trim().is_empty()) =>
        {
            Err(invalid(
                "private key path and passphrase environment name must not be empty".to_string(),
            ))
        }
        Some(SshAuthConfig::Password { password_env }) if password_env.trim().is_empty() => Err(
            invalid("password environment name must not be empty".to_string()),
        ),
        _ => Ok(()),
    }
}

fn default_hosts() -> BTreeMap<String, HostConfig> {
    BTreeMap::from([(
        "default".to_string(),
        HostConfig {
            auto: None,
            inherit_ssh_config_forwards: None,
            tags: Vec::new(),
            host: None,
            port: None,
            username: None,
            auth: None,
            ssh_config_host: Some("localhost".to_string()),
            ssh_config_path: None,
            host_key_policy: None,
            known_hosts_path: None,
            keep_alive_secs: None,
            min_sessions: None,
            max_sessions: None,
            session_rotation_enabled: None,
            session_rotation_interval_secs: None,
            max_channels_per_session: None,
            server_alive_count_max: None,
            connect_timeout_secs: None,
            restart_initial_millis: None,
            restart_max_secs: None,
            session_spawn_cooldown_millis: None,
            session_drain_timeout_secs: None,
            probe: ProbeOverrideConfig::default(),
            local_proxies: vec![ProxyConfig {
                auto: None,
                name: None,
                listen: "127.0.0.1:7890"
                    .parse()
                    .expect("default proxy address must be valid"),
                mixed: Some(true),
                protocol: None,
            }],
            local_forwards: Vec::new(),
            remote_proxies: Vec::new(),
            remote_forwards: Vec::new(),
        },
    )])
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn default_min_sessions() -> usize {
    3
}

fn default_max_sessions() -> usize {
    10
}

fn default_session_rotation_enabled() -> bool {
    true
}

fn default_session_rotation_interval_secs() -> u64 {
    60 * 60
}

fn default_max_channels_per_session() -> usize {
    64
}

fn default_restart_initial_millis() -> u64 {
    500
}

fn default_restart_max_secs() -> u64 {
    30
}

fn default_session_spawn_cooldown_millis() -> u64 {
    1000
}

fn default_session_drain_timeout_secs() -> u64 {
    300
}

fn default_probe_interval_secs() -> u64 {
    10
}

fn is_default_probe_interval_secs(value: &u64) -> bool {
    *value == default_probe_interval_secs()
}

fn default_probe_timeout_millis() -> u64 {
    1500
}

fn is_default_probe_timeout_millis(value: &u64) -> bool {
    *value == default_probe_timeout_millis()
}

fn default_probe_fail_threshold() -> u32 {
    3
}

fn is_default_probe_fail_threshold(value: &u32) -> bool {
    *value == default_probe_fail_threshold()
}

fn default_probe_recovery_threshold() -> u32 {
    2
}

fn is_default_probe_recovery_threshold(value: &u32) -> bool {
    *value == default_probe_recovery_threshold()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_host(config: &AppConfig) -> &HostConfig {
        config.hosts.get("default").unwrap()
    }

    fn default_host_mut(config: &mut AppConfig) -> &mut HostConfig {
        config.hosts.get_mut("default").unwrap()
    }

    #[test]
    fn default_config_is_valid_and_round_trips_all_formats() {
        let expected = AppConfig::default();
        expected.validate().unwrap();
        for format in [ConfigFormat::Yaml, ConfigFormat::Json, ConfigFormat::Toml] {
            let serialized = expected.to_string(format).unwrap();
            let parsed = AppConfig::from_str(&serialized, format).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn default_config_serializes_only_non_default_values() {
        let output = AppConfig::default().to_yaml_string().unwrap();
        assert!(output.contains("ssh-config-host: localhost"));
        assert!(output.contains("listen: 127.0.0.1:7890"));
        assert!(!output.contains("auto:"));
        assert!(!output.contains("inherit-ssh-config-forwards:"));
        assert!(!output.contains("mode:"));
        assert!(!output.contains("probe:"));
        assert!(!output.contains("min-sessions:"));
        assert!(!output.contains("control:"));
    }

    #[test]
    fn automatic_tunnel_detection_ignores_empty_and_disabled_entries() {
        let mut config = AppConfig::default();
        let resolved = default_host(&config).resolve(&config.override_default);
        assert!(resolved.has_automatic_tunnels());

        let host = default_host_mut(&mut config);
        host.local_proxies[0].auto = Some(false);
        let resolved = default_host(&config).resolve(&config.override_default);
        assert!(!resolved.has_automatic_tunnels());

        default_host_mut(&mut config).local_proxies.clear();
        let resolved = default_host(&config).resolve(&config.override_default);
        assert!(!resolved.has_automatic_tunnels());
    }

    #[test]
    fn control_endpoint_round_trips_all_formats() {
        let mut expected = AppConfig::default();
        expected.control.endpoint = Some("tcp:0.0.0.0:19090".to_string());
        expected.validate().unwrap();

        for format in [ConfigFormat::Yaml, ConfigFormat::Json, ConfigFormat::Toml] {
            let serialized = expected.to_string(format).unwrap();
            let parsed = AppConfig::from_str(&serialized, format).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn env_profiles_round_trip_all_formats() {
        let mut expected = AppConfig::default();
        expected.env.default = Some("corp".to_string());
        expected.env.profiles.insert(
            "corp".to_string(),
            EnvProfileConfig {
                host: Some("default".to_string()),
                tunnel: Some("local-proxy-127.0.0.1:7890".to_string()),
                scheme: Some(ProxyEnvScheme::Socks5h),
            },
        );
        expected.validate().unwrap();

        for format in [ConfigFormat::Yaml, ConfigFormat::Json, ConfigFormat::Toml] {
            let serialized = expected.to_string(format).unwrap();
            let parsed = AppConfig::from_str(&serialized, format).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn env_default_must_name_a_defined_profile() {
        let mut config = AppConfig::default();
        config.env.default = Some("missing".to_string());
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidEnvConfig(_)
        ));
    }

    #[test]
    fn resolved_local_proxies_include_ssh_dynamic_forwards() {
        let directory =
            std::env::temp_dir().join(format!("stk-config-env-forwards-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let ssh_config = directory.join("config");
        fs::write(
            &ssh_config,
            "Host target\n  DynamicForward 127.0.0.1:19080\n",
        )
        .unwrap();

        let mut config = AppConfig::default();
        let host = default_host_mut(&mut config);
        host.ssh_config_host = Some("target".to_string());
        host.ssh_config_path = Some(ssh_config.to_string_lossy().into_owned());
        host.local_proxies[0].auto = Some(false);

        let candidates = config.resolved_local_proxies().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].host, "default");
        assert_eq!(candidates[0].tunnel, "ssh-config-dynamic-19080");
        assert_eq!(candidates[0].listen.port(), 19080);
        assert_eq!(candidates[0].protocol, ProxyProtocol::Mixed);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ssh_forward_inheritance_option_parses_in_all_formats() {
        for (format, input) in [
            (
                ConfigFormat::Yaml,
                "hosts:\n  server:\n    host: example.com\n    inherit-ssh-config-forwards: false\n",
            ),
            (
                ConfigFormat::Json,
                r#"{"hosts":{"server":{"host":"example.com","inherit-ssh-config-forwards":false}}}"#,
            ),
            (
                ConfigFormat::Toml,
                "[hosts.server]\nhost = \"example.com\"\ninherit-ssh-config-forwards = false\n",
            ),
        ] {
            let config = AppConfig::from_str(input, format).unwrap();
            assert!(
                !config.hosts["server"]
                    .resolve(&config.override_default)
                    .inherit_ssh_config_forwards
            );
        }
    }

    #[test]
    fn invalid_control_endpoint_is_rejected_by_validation() {
        let mut config = AppConfig::default();
        config.control.endpoint = Some("udp:127.0.0.1:19090".to_string());
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidControlEndpoint(_)
        ));
    }

    #[test]
    fn override_default_is_applied_before_host_and_port_overrides() {
        let config = AppConfig::from_yaml_str(
            r#"
override-default:
  inherit-ssh-config-forwards: false
  min-sessions: 1
  session-rotation-enabled: false
  session-rotation-interval-secs: 120
  probe:
    interval-secs: 20
  proxy:
    auto: false
    mixed: true
  forward:
    auto: false
hosts:
  inherited:
    host: inherited.example
    local-proxies:
      - listen: 127.0.0.1:17001
    local-forwards:
      - listen: 127.0.0.1:17002
        target: example.internal:80
  overridden:
    host: overridden.example
    inherit-ssh-config-forwards: true
    min-sessions: 3
    session-rotation-enabled: true
    session-rotation-interval-secs: 60
    probe:
      interval-secs: 5
    local-proxies:
      - auto: true
        listen: 127.0.0.1:17003
        mixed: false
    local-forwards:
      - auto: true
        listen: 127.0.0.1:17004
        target: example.internal:80
"#,
        )
        .unwrap();

        let inherited = config.hosts["inherited"].resolve(&config.override_default);
        assert!(!inherited.inherit_ssh_config_forwards);
        assert_eq!(inherited.min_sessions, 1);
        assert_eq!(inherited.max_sessions, 10);
        assert!(!inherited.session_rotation_enabled);
        assert_eq!(inherited.session_rotation_interval_secs, 120);
        assert_eq!(inherited.probe.interval_secs, 20);
        assert!(!inherited.local_proxies[0].auto);
        assert!(inherited.local_proxies[0].mixed);
        assert!(!inherited.local_forwards[0].auto);

        let overridden = config.hosts["overridden"].resolve(&config.override_default);
        assert!(overridden.inherit_ssh_config_forwards);
        assert_eq!(overridden.min_sessions, 3);
        assert_eq!(overridden.max_sessions, 10);
        assert!(overridden.session_rotation_enabled);
        assert_eq!(overridden.session_rotation_interval_secs, 60);
        assert_eq!(overridden.probe.interval_secs, 5);
        assert!(overridden.local_proxies[0].auto);
        assert!(!overridden.local_proxies[0].mixed);
        assert!(overridden.local_forwards[0].auto);
    }

    #[test]
    fn override_default_round_trips_without_materializing_inherited_values() {
        let config = AppConfig::from_yaml_str(
            r#"
override-default:
  min-sessions: 1
  proxy:
    mixed: true
hosts:
  server1:
    host: server1.example
    local-proxies:
      - listen: 127.0.0.1:17001
  server2:
    host: server2.example
    min-sessions: 3
    local-proxies:
      - listen: 127.0.0.1:17002
"#,
        )
        .unwrap();

        for format in [ConfigFormat::Yaml, ConfigFormat::Json, ConfigFormat::Toml] {
            let serialized = config.to_string(format).unwrap();
            let parsed = AppConfig::from_str(&serialized, format).unwrap();
            assert_eq!(parsed, config);
            assert_eq!(serialized.matches("min-sessions").count(), 2);
        }
    }

    #[test]
    fn all_format_examples_parse_and_validate() {
        for (format, input) in [
            (
                ConfigFormat::Yaml,
                include_str!("../../../examples/basic.yaml"),
            ),
            (
                ConfigFormat::Json,
                include_str!("../../../examples/basic.json"),
            ),
            (
                ConfigFormat::Toml,
                include_str!("../../../examples/basic.toml"),
            ),
            (
                ConfigFormat::Yaml,
                include_str!("../../../examples/ssh-native.yaml"),
            ),
            (
                ConfigFormat::Json,
                include_str!("../../../examples/ssh-native.json"),
            ),
            (
                ConfigFormat::Toml,
                include_str!("../../../examples/ssh-native.toml"),
            ),
        ] {
            AppConfig::from_str(input, format)
                .unwrap()
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn format_is_detected_from_file_extension() {
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.yml")).unwrap(),
            ConfigFormat::Yaml
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.json")).unwrap(),
            ConfigFormat::Json
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("config.toml")).unwrap(),
            ConfigFormat::Toml
        );
        assert!(matches!(
            ConfigFormat::from_path(Path::new("config.txt")).unwrap_err(),
            ConfigError::UnsupportedFormat(_)
        ));
    }

    #[test]
    fn auto_defaults_to_true_at_host_and_port_levels() {
        let config = AppConfig::from_yaml_str(
            r#"
hosts:
  server1:
    host: example.com
    local-proxies:
      - listen: 127.0.0.1:17890
    local-forwards:
      - listen: 127.0.0.1:17891
        target: 127.0.0.1:80
"#,
        )
        .unwrap();
        let host = config.hosts.get("server1").unwrap();
        let host = host.resolve(&config.override_default);
        assert!(host.auto);
        assert!(host.inherit_ssh_config_forwards);
        assert!(host.local_proxies[0].auto);
        assert!(host.local_forwards[0].auto);
        assert_eq!(host.min_sessions, 3);
        assert_eq!(host.max_sessions, 10);
        assert!(host.session_rotation_enabled);
        assert_eq!(host.session_rotation_interval_secs, 3_600);
        assert_eq!(host.max_channels_per_session, 64);
        assert_eq!(host.restart_initial_millis, 500);
        assert_eq!(host.restart_max_secs, 30);
        assert_eq!(host.session_spawn_cooldown_millis, 1000);
        assert_eq!(host.session_drain_timeout_secs, 300);
        assert_eq!(host.probe, ProbeConfig::default());
    }

    #[test]
    fn logical_host_key_is_separate_from_ssh_config_alias() {
        let config = AppConfig::default();
        let host = default_host(&config).resolve(&config.override_default);
        let pool = host.runtime_pool("logical-name");
        assert_eq!(pool.hosts[0].name, "logical-name");
        assert_eq!(pool.hosts[0].host, None);
        assert_eq!(pool.hosts[0].ssh_config_host.as_deref(), Some("localhost"));
    }

    #[test]
    fn toml_named_hosts_keep_nested_forwards_scoped() {
        let config = AppConfig::from_toml_str(
            r#"
[hosts.server1]

[[hosts.server1.local-proxies]]
listen = "127.0.0.1:17890"

[hosts.server2]

[[hosts.server2.local-proxies]]
listen = "127.0.0.1:17891"
"#,
        )
        .unwrap();

        assert_eq!(
            config.hosts["server1"].local_proxies[0].listen.port(),
            17890
        );
        assert_eq!(
            config.hosts["server2"].local_proxies[0].listen.port(),
            17891
        );
    }

    #[test]
    fn duplicate_named_hosts_are_rejected() {
        let error = AppConfig::from_json_str(
            r#"{
  "hosts": {
    "server1": {},
    "server1": {}
  }
}"#,
        )
        .unwrap_err();

        assert!(matches!(error, ConfigError::Parse { .. }));
        assert!(
            error
                .to_string()
                .contains("duplicate SSH host name: server1")
        );
    }

    #[test]
    fn host_port_parses_domains_and_ipv6() {
        assert_eq!(
            "database.internal:5432".parse::<HostPort>().unwrap(),
            HostPort {
                host: "database.internal".to_string(),
                port: 5432,
            }
        );
        assert_eq!(
            "[::1]:8080".parse::<HostPort>().unwrap().to_string(),
            "[::1]:8080"
        );
    }

    #[test]
    fn all_forward_listens_accept_ipv4_and_ipv6() {
        let config = AppConfig::from_yaml_str(
            r#"
hosts:
  server1:
    host: example.com
    local-proxies:
      - listen: 127.0.0.1:17001
      - listen: "[::1]:17002"
    local-forwards:
      - listen: 127.0.0.1:17003
        target: example.internal:80
      - listen: "[::1]:17004"
        target: example.internal:80
    remote-proxies:
      - listen: 127.0.0.1:17005
      - listen: "[::1]:17006"
    remote-forwards:
      - listen: 127.0.0.1:17007
        target: localhost:80
      - listen: "[::1]:17008"
        target: localhost:80
"#,
        )
        .unwrap();
        config.validate().unwrap();

        let host = &config.hosts["server1"];
        assert!(host.local_proxies[0].listen.is_ipv4());
        assert!(host.local_proxies[1].listen.is_ipv6());
        assert!(host.local_forwards[0].listen.is_ipv4());
        assert!(host.local_forwards[1].listen.is_ipv6());
        assert!(host.remote_proxies[0].listen.is_ipv4());
        assert!(host.remote_proxies[1].listen.is_ipv6());
        assert!(host.remote_forwards[0].listen.is_ipv4());
        assert!(host.remote_forwards[1].listen.is_ipv6());
    }

    #[test]
    fn validation_rejects_duplicate_local_listens() {
        let mut config = AppConfig::default();
        default_host_mut(&mut config)
            .local_forwards
            .push(ForwardConfig {
                auto: Some(true),
                name: None,
                listen: "127.0.0.1:7890".parse().unwrap(),
                target: "127.0.0.1:80".parse().unwrap(),
            });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::DuplicateLocalListen(_)
        ));
    }

    #[test]
    fn validation_rejects_duplicate_remote_ports() {
        let mut config = AppConfig::default();
        default_host_mut(&mut config)
            .remote_proxies
            .push(ProxyConfig {
                auto: Some(true),
                name: None,
                listen: "127.0.0.1:1080".parse().unwrap(),
                mixed: Some(false),
                protocol: None,
            });
        default_host_mut(&mut config)
            .remote_forwards
            .push(ForwardConfig {
                auto: Some(true),
                name: None,
                listen: "0.0.0.0:1080".parse().unwrap(),
                target: "127.0.0.1:8080".parse().unwrap(),
            });
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidHostConfig { .. }
        ));
    }

    #[test]
    fn validation_rejects_invalid_session_pool() {
        let mut config = AppConfig::default();
        let host = default_host_mut(&mut config);
        host.min_sessions = Some(3);
        host.max_sessions = Some(2);
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidHostConfig { .. }
        ));

        let mut config = AppConfig::default();
        default_host_mut(&mut config).session_rotation_interval_secs = Some(0);
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidHostConfig { .. }
        ));
    }

    #[test]
    fn validation_requires_host_or_ssh_config_host() {
        let mut config = AppConfig::default();
        let host = default_host_mut(&mut config);
        host.host = None;
        host.ssh_config_host = None;
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidHostConfig { .. }
        ));
    }

    #[test]
    fn auto_false_host_can_remain_dormant_without_forwards() {
        let mut config = AppConfig::default();
        let host = default_host_mut(&mut config);
        host.auto = Some(false);
        host.local_proxies.clear();
        config.validate().unwrap();
    }

    #[test]
    fn auto_false_port_does_not_conflict_or_register() {
        let mut config = AppConfig::default();
        default_host_mut(&mut config)
            .local_forwards
            .push(ForwardConfig {
                auto: Some(false),
                name: Some("manual-local".to_string()),
                listen: "127.0.0.1:7890".parse().unwrap(),
                target: "127.0.0.1:80".parse().unwrap(),
            });
        default_host_mut(&mut config)
            .remote_proxies
            .push(ProxyConfig {
                auto: Some(false),
                name: Some("manual-remote".to_string()),
                listen: "127.0.0.1:1080".parse().unwrap(),
                mixed: Some(false),
                protocol: None,
            });
        config.validate().unwrap();
        assert!(
            default_host(&config)
                .resolve(&config.override_default)
                .runtime_pool("default")
                .hosts[0]
                .remote_forwards
                .is_empty()
        );
    }

    #[test]
    fn validation_rejects_conflicting_mixed_protocol() {
        let mut config = AppConfig::default();
        default_host_mut(&mut config).local_proxies[0].protocol = Some(ProxyProtocol::Http);
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidHostConfig { .. }
        ));
    }

    #[test]
    fn legacy_and_unknown_fields_are_rejected() {
        for input in [
            "ssh-groups: []\n",
            "proxy-listeners: []\n",
            "telemetry: {}\n",
            "hosts:\n  - name: server1\n",
            "hosts:\n  - name: server1\n    enabled: true\n",
            "hosts:\n  server1:\n    name: legacy-name\n",
            "hosts:\n  server1:\n    use-ssh-config: true\n",
            "hosts:\n  server1:\n    enabled: true\n",
            "hosts:\n  server1:\n    dynamic: true\n",
        ] {
            assert!(matches!(
                AppConfig::from_yaml_str(input).unwrap_err(),
                ConfigError::Parse { .. }
            ));
        }
    }

    #[test]
    fn removed_mode_is_rejected_in_all_formats() {
        for (input, format) in [
            ("mode: daemon\n", ConfigFormat::Yaml),
            (r#"{"mode":"daemon"}"#, ConfigFormat::Json),
            ("mode = \"daemon\"\n", ConfigFormat::Toml),
        ] {
            assert!(matches!(
                AppConfig::from_str(input, format).unwrap_err(),
                ConfigError::Parse { .. }
            ));
        }
    }
}
