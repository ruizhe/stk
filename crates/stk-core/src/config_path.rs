use std::{
    env,
    path::{Path, PathBuf},
};

pub const DEFAULT_CONFIG_FILE_NAME: &str = "config.yaml";
pub const CONFIG_FILE_NAMES: [&str; 4] = [
    DEFAULT_CONFIG_FILE_NAME,
    "config.yml",
    "config.json",
    "config.toml",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    User,
    System,
}

pub fn default_config_directory(scope: ConfigScope) -> PathBuf {
    match scope {
        ConfigScope::User => home_directory().join(".config/stk"),
        ConfigScope::System => system_config_directory(),
    }
}

pub fn default_config_path(scope: ConfigScope) -> PathBuf {
    let directory = default_config_directory(scope);
    discover_config_path(&directory).unwrap_or_else(|| directory.join(DEFAULT_CONFIG_FILE_NAME))
}

pub fn resolve_config_path(explicit: Option<&Path>, scope: ConfigScope) -> PathBuf {
    let Some(explicit) = explicit else {
        return default_config_path(scope);
    };
    let path = expand_tilde(explicit);
    if path.is_dir() {
        discover_config_path(&path).unwrap_or_else(|| path.join(DEFAULT_CONFIG_FILE_NAME))
    } else {
        path
    }
}

pub fn discover_config_path(directory: &Path) -> Option<PathBuf> {
    CONFIG_FILE_NAMES
        .into_iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file())
}

fn home_directory() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"))
}

#[cfg(not(target_os = "windows"))]
fn system_config_directory() -> PathBuf {
    PathBuf::from("/etc/stk")
}

#[cfg(target_os = "windows")]
fn system_config_directory() -> PathBuf {
    env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("stk")
}

fn expand_tilde(path: &Path) -> PathBuf {
    let path = path.to_string_lossy();
    if path == "~" {
        return home_directory();
    }
    if let Some(suffix) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home_directory().join(suffix);
    }
    PathBuf::from(path.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn discovers_supported_config_formats_in_preference_order() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("config.toml"), "hosts = {}\n").unwrap();
        assert_eq!(
            discover_config_path(&directory),
            Some(directory.join("config.toml"))
        );

        fs::write(directory.join("config.yaml"), "hosts: {}\n").unwrap();
        assert_eq!(
            discover_config_path(&directory),
            Some(directory.join("config.yaml"))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn explicit_directory_uses_default_filename_when_empty() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        assert_eq!(
            resolve_config_path(Some(&directory), ConfigScope::User),
            directory.join(DEFAULT_CONFIG_FILE_NAME)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn explicit_file_is_preserved() {
        let path = PathBuf::from("custom/settings.json");
        assert_eq!(resolve_config_path(Some(&path), ConfigScope::System), path);
    }

    fn test_directory() -> PathBuf {
        env::temp_dir().join(format!(
            "stk-config-path-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
