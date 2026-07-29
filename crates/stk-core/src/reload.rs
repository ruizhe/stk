use crate::{
    config::{AppConfig, ConfigFormat},
    engine::{Engine, RuntimeProfile},
    stats,
};
use anyhow::{Context, anyhow, bail};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tracing::{error, info, warn};

const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
const ENGINE_START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadRequest {
    Filesystem,
    Force,
}

#[derive(Debug, Clone)]
pub struct ReloadHandle {
    sender: mpsc::UnboundedSender<ReloadRequest>,
}

impl ReloadHandle {
    pub fn request_reload(&self) -> bool {
        self.sender.send(ReloadRequest::Force).is_ok()
    }

    fn notify_filesystem_change(&self) {
        let _ = self.sender.send(ReloadRequest::Filesystem);
    }
}

#[derive(Debug)]
pub struct ReloadControl {
    handle: ReloadHandle,
    requests: mpsc::UnboundedReceiver<ReloadRequest>,
}

impl ReloadControl {
    pub fn new() -> Self {
        let (sender, requests) = mpsc::unbounded_channel();
        Self {
            handle: ReloadHandle { sender },
            requests,
        }
    }

    pub fn handle(&self) -> ReloadHandle {
        self.handle.clone()
    }
}

impl Default for ReloadControl {
    fn default() -> Self {
        Self::new()
    }
}

struct ActiveEngine {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl ActiveEngine {
    async fn stop(mut self) -> anyhow::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        flatten_engine_result(self.task.await)
    }
}

pub async fn run_config_file_until_shutdown<F>(
    path: impl AsRef<Path>,
    profile: RuntimeProfile,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send,
{
    run_config_file_with_control_until_shutdown(path, profile, ReloadControl::new(), shutdown).await
}

pub async fn run_config_file_with_control_until_shutdown<F>(
    path: impl AsRef<Path>,
    profile: RuntimeProfile,
    control: ReloadControl,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send,
{
    let path = absolute_path(path.as_ref())?;
    let format = ConfigFormat::from_path(&path)?;
    let initial_content = read_config_content(&path).await?;
    let mut active_config = parse_config(&initial_content, format)?;
    let ReloadControl {
        handle,
        mut requests,
    } = control;
    let _watcher = watch_config_file(&path, handle.clone())?;
    let mut active = start_engine(active_config.clone(), profile, handle.clone()).await?;
    let mut last_observed_content = initial_content;
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                return active.stop().await;
            }
            result = &mut active.task => {
                return flatten_engine_result(result);
            }
            request = requests.recv() => {
                let Some(request) = request else {
                    bail!("configuration reload control channel closed unexpectedly");
                };
                let force = debounce_reload_requests(request, &mut requests).await;
                let content = match read_config_content(&path).await {
                    Ok(content) => content,
                    Err(error) => {
                    stats::record_config_reload_error();
                        warn!(config = %path.display(), %error, "failed to read changed config; keeping current generation");
                        continue;
                    }
                };
                if !force && content == last_observed_content {
                    continue;
                }
                last_observed_content = content.clone();

                let candidate = match parse_config(&content, format) {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        stats::record_config_reload_error();
                        warn!(config = %path.display(), %error, "changed config is invalid; keeping current generation");
                        continue;
                    }
                };
                if !force && candidate == active_config {
                    info!(config = %path.display(), "config file changed without effective configuration changes");
                    continue;
                }

                info!(config = %path.display(), force, "validated reload request; replacing runtime generation");
                let previous_config = active_config.clone();
                active.stop().await?;
                match start_engine(candidate.clone(), profile, handle.clone()).await {
                    Ok(next) => {
                        active = next;
                        active_config = candidate;
                        stats::record_config_reload_success();
                        info!(config = %path.display(), "configuration reloaded");
                    }
                    Err(reload_error) => {
                        stats::record_config_reload_error();
                        error!(config = %path.display(), %reload_error, "new configuration failed to start; rolling back");
                        active = start_engine(previous_config.clone(), profile, handle.clone()).await.map_err(|rollback_error| {
                            anyhow!(
                                "new configuration failed to start: {reload_error:#}; rollback also failed: {rollback_error:#}"
                            )
                        })?;
                        active_config = previous_config;
                        warn!(config = %path.display(), "previous configuration restored");
                    }
                }
            }
        }
    }
}

async fn start_engine(
    config: AppConfig,
    profile: RuntimeProfile,
    reload_handle: ReloadHandle,
) -> anyhow::Result<ActiveEngine> {
    let engine = Engine::with_profile_and_reload(config, profile, reload_handle)?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let mut task = tokio::spawn(engine.run_until_shutdown_with_ready(
        async move {
            let _ = shutdown_rx.await;
        },
        Some(ready_tx),
    ));

    match timeout(ENGINE_START_TIMEOUT, ready_rx).await {
        Ok(Ok(())) => Ok(ActiveEngine {
            shutdown: Some(shutdown_tx),
            task,
        }),
        Ok(Err(_)) => {
            let result = (&mut task).await;
            flatten_engine_result(result).context("engine exited before reporting readiness")?;
            bail!("engine exited before reporting readiness")
        }
        Err(_) => {
            let _ = shutdown_tx.send(());
            let _ = task.await;
            bail!("timed out waiting for engine startup")
        }
    }
}

async fn debounce_reload_requests(
    first: ReloadRequest,
    requests: &mut mpsc::UnboundedReceiver<ReloadRequest>,
) -> bool {
    let mut force = first == ReloadRequest::Force;
    loop {
        match timeout(RELOAD_DEBOUNCE, requests.recv()).await {
            Ok(Some(request)) => force |= request == ReloadRequest::Force,
            Ok(None) | Err(_) => return force,
        }
    }
}

fn watch_config_file(
    path: &Path,
    reload_handle: ReloadHandle,
) -> anyhow::Result<RecommendedWatcher> {
    let mut watcher =
        notify::recommended_watcher(move |result: notify::Result<Event>| match result {
            Ok(event) if is_relevant_event(&event) => {
                reload_handle.notify_filesystem_change();
            }
            Ok(_) => {}
            Err(error) => {
                stats::record_config_reload_error();
                warn!(%error, "configuration file watcher failed");
            }
        })?;
    let directory = path
        .parent()
        .context("configuration file has no parent directory")?;
    watcher.watch(directory, RecursiveMode::NonRecursive)?;
    info!(config = %path.display(), directory = %directory.display(), "watching configuration file changes");
    Ok(watcher)
}

fn is_relevant_event(event: &Event) -> bool {
    !matches!(event.kind, EventKind::Access(_))
}

fn absolute_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

async fn read_config_content(path: &Path) -> anyhow::Result<String> {
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read config {}", path.display()))
}

fn parse_config(input: &str, format: ConfigFormat) -> anyhow::Result<AppConfig> {
    let config = AppConfig::from_str(input, format)?;
    config.validate()?;
    Ok(config)
}

fn flatten_engine_result(
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    result.context("engine task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };
    use tokio::{net::TcpListener, time::sleep};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[tokio::test]
    async fn valid_config_changes_reload_and_invalid_changes_are_ignored() {
        let path = std::env::temp_dir().join(format!(
            "stk-reload-test-{}-{}.yaml",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let reserved_control = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_address = reserved_control.local_addr().unwrap();
        drop(reserved_control);
        let control_config = format!("control:\n  endpoint: tcp:{control_address}\n");
        fs::write(
            &path,
            format!("{control_config}hosts:\n  dormant:\n    auto: false\n"),
        )
        .unwrap();
        let baseline = stats::runtime_snapshot();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let control = ReloadControl::new();
        let reload_handle = control.handle();
        let runtime_path = path.clone();
        let runtime = tokio::spawn(async move {
            run_config_file_with_control_until_shutdown(
                &runtime_path,
                RuntimeProfile::Foreground,
                control,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        sleep(Duration::from_millis(500)).await;
        fs::write(
            &path,
            format!(
                "{control_config}hosts:\n  dormant:\n    auto: false\n  spare:\n    auto: false\n"
            ),
        )
        .unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                if stats::runtime_snapshot().config_reloads_total > baseline.config_reloads_total {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("valid config was not reloaded");

        let reloads_after_file_change = stats::runtime_snapshot().config_reloads_total;
        assert!(reload_handle.request_reload());
        timeout(Duration::from_secs(5), async {
            loop {
                if stats::runtime_snapshot().config_reloads_total > reloads_after_file_change {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("forced reload did not rebuild an unchanged configuration");

        fs::write(&path, "hosts: [invalid\n").unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                if stats::runtime_snapshot().config_reload_errors_total
                    > baseline.config_reload_errors_total
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("invalid config was not observed");
        assert!(!runtime.is_finished());

        let reloads_before_recoverable_listener = stats::runtime_snapshot().config_reloads_total;
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_address = occupied.local_addr().unwrap();
        fs::write(
            &path,
            format!(
                "{control_config}hosts:\n  failed-generation:\n    host: 127.0.0.1\n    username: test\n    host-key-policy: insecure-accept-any\n    local-proxies:\n      - listen: {occupied_address}\n"
            ),
        )
        .unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                if stats::runtime_snapshot().config_reloads_total
                    > reloads_before_recoverable_listener
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("configuration with a recoverable listener failure was not activated");
        assert!(!runtime.is_finished());
        assert!(stats::runtime_snapshot().running);

        drop(occupied);
        timeout(Duration::from_secs(5), async {
            loop {
                match tokio::net::TcpStream::connect(occupied_address).await {
                    Ok(stream) => break stream,
                    Err(_) => sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("listener did not recover after the occupied port was released");

        let _ = shutdown_tx.send(());
        runtime.await.unwrap().unwrap();
        let _ = fs::remove_file(path);
    }
}
