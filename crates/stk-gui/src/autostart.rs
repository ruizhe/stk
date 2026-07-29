use anyhow::Context as _;
use std::{env, fs, io::Write as _, path::Path};
use tempfile::NamedTempFile;

#[cfg(any(target_os = "windows", target_os = "linux"))]
const APP_NAME: &str = "SSH Tunnel Keeper";

pub fn is_supported() -> bool {
    cfg!(any(
        target_os = "macos",
        target_os = "windows",
        target_os = "linux"
    ))
}

pub fn is_enabled() -> anyhow::Result<bool> {
    platform::is_enabled()
}

pub fn set_enabled(enabled: bool) -> anyhow::Result<bool> {
    platform::set_enabled(enabled)?;
    is_enabled()
}

fn current_executable() -> anyhow::Result<std::path::PathBuf> {
    env::current_exe().context("failed to resolve the GUI executable path")
}

fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("startup item path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::path::{Path, PathBuf};

    const LAUNCH_AGENT_LABEL: &str = "io.sshtunnelkeeper.desktop.autostart";

    pub fn is_enabled() -> anyhow::Result<bool> {
        Ok(registration_path()?.is_file())
    }

    pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
        let path = registration_path()?;
        if enabled {
            let executable = current_executable()?;
            write_atomic(&path, &launch_agent_plist(&executable))
        } else {
            remove_if_present(&path)
        }
    }

    fn registration_path() -> anyhow::Result<PathBuf> {
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCH_AGENT_LABEL}.plist")))
    }

    fn launch_agent_plist(executable: &Path) -> String {
        let executable = xml_escape(&executable.to_string_lossy());
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>{LAUNCH_AGENT_LABEL}</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n\
        <string>{executable}</string>\n\
        <string>--hidden</string>\n\
    </array>\n\
    <key>RunAtLoad</key>\n\
    <true/>\n\
    <key>ProcessType</key>\n\
    <string>Interactive</string>\n\
    <key>LimitLoadToSessionType</key>\n\
    <string>Aqua</string>\n\
</dict>\n\
</plist>\n"
        )
    }

    fn xml_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn launch_agent_contains_escaped_executable_path() {
            let content =
                launch_agent_plist(Path::new("/Applications/A&B.app/Contents/MacOS/stk-gui"));

            assert!(content.contains(LAUNCH_AGENT_LABEL));
            assert!(content.contains("/Applications/A&amp;B.app/Contents/MacOS/stk-gui"));
            assert!(content.contains("<string>--hidden</string>"));
            assert!(content.contains("<key>RunAtLoad</key>"));
        }

        #[test]
        fn launch_agent_registration_round_trips_in_an_isolated_directory() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("stk.plist");
            let content =
                launch_agent_plist(Path::new("/Applications/STK.app/Contents/MacOS/stk-gui"));

            write_atomic(&path, &content).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
            assert!(
                std::process::Command::new("plutil")
                    .args(["-lint", path.to_str().unwrap()])
                    .status()
                    .unwrap()
                    .success()
            );
            remove_if_present(&path).unwrap();
            assert!(!path.exists());
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use anyhow::bail;
    use std::process::Command;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn is_enabled() -> anyhow::Result<bool> {
        let output = Command::new("reg.exe")
            .args(["query", RUN_KEY, "/v", APP_NAME])
            .output()
            .context("failed to query the Windows startup registry")?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "failed to query the Windows startup registry: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }

    pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
        if enabled {
            let executable = current_executable()?;
            let command_line = format!("\"{}\" --hidden", executable.display());
            run_registry_command(&[
                "add",
                RUN_KEY,
                "/v",
                APP_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &command_line,
                "/f",
            ])
        } else if is_enabled()? {
            run_registry_command(&["delete", RUN_KEY, "/v", APP_NAME, "/f"])
        } else {
            Ok(())
        }
    }

    fn run_registry_command(arguments: &[&str]) -> anyhow::Result<()> {
        let output = Command::new("reg.exe")
            .args(arguments)
            .output()
            .context("failed to update the Windows startup registry")?;
        if output.status.success() {
            Ok(())
        } else {
            bail!(
                "failed to update the Windows startup registry: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::path::{Path, PathBuf};

    pub fn is_enabled() -> anyhow::Result<bool> {
        Ok(registration_path()?.is_file())
    }

    pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
        let path = registration_path()?;
        if enabled {
            let executable = current_executable()?;
            write_atomic(&path, &desktop_entry(&executable))
        } else {
            remove_if_present(&path)
        }
    }

    fn registration_path() -> anyhow::Result<PathBuf> {
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .filter(|value| Path::new(value).is_absolute())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .context("neither XDG_CONFIG_HOME nor HOME is available")?;
        Ok(config_home
            .join("autostart")
            .join("ssh-tunnel-keeper.desktop"))
    }

    fn desktop_entry(executable: &Path) -> String {
        let executable = desktop_exec_quote(&executable.to_string_lossy());
        format!(
            "[Desktop Entry]\n\
Type=Application\n\
Version=1.0\n\
Name={APP_NAME}\n\
Comment=Reliable SSH proxies and tunnel management\n\
Exec={executable} --hidden\n\
Icon=ssh-tunnel-keeper\n\
Terminal=false\n\
StartupNotify=false\n\
X-GNOME-Autostart-enabled=true\n"
        )
    }

    fn desktop_exec_quote(value: &str) -> String {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$");
        format!("\"{escaped}\"")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn desktop_entry_quotes_executable_paths() {
            let content = desktop_entry(Path::new("/opt/SSH Tunnel Keeper/stk-gui"));

            assert!(content.contains("Exec=\"/opt/SSH Tunnel Keeper/stk-gui\" --hidden"));
            assert!(content.contains("Terminal=false"));
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use super::*;
    use anyhow::bail;

    pub fn is_enabled() -> anyhow::Result<bool> {
        Ok(false)
    }

    pub fn set_enabled(_enabled: bool) -> anyhow::Result<()> {
        bail!("automatic startup is not supported on this platform")
    }
}
