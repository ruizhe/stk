use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write as _,
    path::{Path, PathBuf},
};
use stk_core::{ConfigScope, default_config_directory};
use tempfile::NamedTempFile;

pub const GUI_CONFIG_FILE_NAME: &str = "gui-config.yaml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN")]
    Chinese,
}

impl Default for Language {
    fn default() -> Self {
        let locale = env::var("LC_ALL")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| env::var("LC_MESSAGES").ok())
            .or_else(|| env::var("LANG").ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if locale.starts_with("zh") {
            Self::Chinese
        } else {
            Self::English
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuiConfig {
    pub language: Language,
}

impl GuiConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&content)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let content = serde_yaml::to_string(self)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(content.as_bytes())?;
        temporary.as_file_mut().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

pub fn gui_config_path() -> PathBuf {
    default_config_directory(ConfigScope::User).join(GUI_CONFIG_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_config_round_trips_language() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(GUI_CONFIG_FILE_NAME);
        let config = GuiConfig {
            language: Language::Chinese,
        };

        config.save(&path).unwrap();

        assert_eq!(GuiConfig::load(&path).unwrap(), config);
        assert_eq!(fs::read_to_string(path).unwrap(), "language: zh-CN\n");
    }

    #[test]
    fn missing_gui_config_uses_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(GUI_CONFIG_FILE_NAME);

        assert_eq!(GuiConfig::load(&path).unwrap(), GuiConfig::default());
    }
}
