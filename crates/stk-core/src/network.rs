use n0_watcher::Watcher;
use netwatch::{interfaces::State as InterfaceState, netmon::Monitor};
use std::sync::Arc;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::AbortHandle,
};
use tracing::{debug, info, warn};

const MONITOR_COMMAND_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetworkAvailability {
    Online,
    Offline,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectivitySnapshot {
    pub(crate) availability: NetworkAvailability,
    pub(crate) events_available: bool,
    pub(crate) resumed: bool,
    pub(crate) generation: u64,
}

impl ConnectivitySnapshot {
    fn initial() -> Self {
        Self {
            availability: NetworkAvailability::Unknown,
            events_available: false,
            resumed: false,
            generation: 0,
        }
    }

    pub(crate) fn is_offline(self) -> bool {
        self.availability == NetworkAvailability::Offline
    }
}

#[derive(Clone)]
pub(crate) struct ConnectivityHandle {
    state: watch::Receiver<ConnectivitySnapshot>,
    commands: Option<mpsc::Sender<MonitorCommand>>,
    fixed_sender: Option<Arc<watch::Sender<ConnectivitySnapshot>>>,
}

impl ConnectivityHandle {
    pub(crate) fn current(&self) -> ConnectivitySnapshot {
        *self.state.borrow()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<ConnectivitySnapshot> {
        self.state.clone()
    }

    pub(crate) async fn refresh(&self) -> ConnectivitySnapshot {
        let Some(commands) = &self.commands else {
            return self.current();
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if commands
            .send(MonitorCommand::Refresh { reply: reply_tx })
            .await
            .is_err()
        {
            return self.current();
        }
        reply_rx.await.unwrap_or_else(|_| self.current())
    }

    #[cfg(test)]
    pub(crate) fn assume_online() -> Self {
        Self::fixed(ConnectivitySnapshot {
            availability: NetworkAvailability::Online,
            events_available: true,
            resumed: false,
            generation: 1,
        })
    }

    #[cfg(test)]
    fn fixed(snapshot: ConnectivitySnapshot) -> Self {
        let (sender, state) = watch::channel(snapshot);
        Self {
            state,
            commands: None,
            fixed_sender: Some(Arc::new(sender)),
        }
    }

    #[cfg(test)]
    pub(crate) fn controlled(
        availability: NetworkAvailability,
        events_available: bool,
    ) -> (Self, ConnectivityTestController) {
        let snapshot = ConnectivitySnapshot {
            availability,
            events_available,
            resumed: false,
            generation: 1,
        };
        let (sender, state) = watch::channel(snapshot);
        let sender = Arc::new(sender);
        (
            Self {
                state,
                commands: None,
                fixed_sender: Some(Arc::clone(&sender)),
            },
            ConnectivityTestController { sender },
        )
    }
}

impl std::fmt::Debug for ConnectivityHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectivityHandle")
            .field("current", &self.current())
            .field("fixed", &self.fixed_sender.is_some())
            .finish()
    }
}

#[cfg(test)]
pub(crate) struct ConnectivityTestController {
    sender: Arc<watch::Sender<ConnectivitySnapshot>>,
}

#[cfg(test)]
impl ConnectivityTestController {
    pub(crate) fn set(&self, availability: NetworkAvailability) {
        self.sender.send_modify(|snapshot| {
            snapshot.availability = availability;
            snapshot.resumed = false;
            snapshot.generation = snapshot.generation.wrapping_add(1);
        });
    }
}

pub(crate) struct ConnectivityMonitor {
    handle: ConnectivityHandle,
    task: AbortHandle,
}

impl ConnectivityMonitor {
    pub(crate) async fn start() -> Self {
        let (state_tx, state_rx) = watch::channel(ConnectivitySnapshot::initial());
        let (command_tx, command_rx) = mpsc::channel(MONITOR_COMMAND_CAPACITY);

        let task = match Monitor::new().await {
            Ok(monitor) => {
                let mut source = monitor.interface_state();
                publish_interface_state(&state_tx, &source.get(), true);
                tokio::spawn(run_event_monitor(monitor, source, state_tx, command_rx))
            }
            Err(error) => {
                warn!(
                    ?error,
                    "network change events are unavailable; using on-demand snapshots"
                );
                publish_interface_state(&state_tx, &InterfaceState::new().await, false);
                tokio::spawn(run_snapshot_monitor(state_tx, command_rx))
            }
        };

        Self {
            handle: ConnectivityHandle {
                state: state_rx,
                commands: Some(command_tx),
                fixed_sender: None,
            },
            task: task.abort_handle(),
        }
    }

    pub(crate) fn handle(&self) -> ConnectivityHandle {
        self.handle.clone()
    }
}

impl Drop for ConnectivityMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum MonitorCommand {
    Refresh {
        reply: oneshot::Sender<ConnectivitySnapshot>,
    },
}

async fn run_event_monitor(
    _monitor: Monitor,
    mut source: n0_watcher::Direct<InterfaceState>,
    state_tx: watch::Sender<ConnectivitySnapshot>,
    mut command_rx: mpsc::Receiver<MonitorCommand>,
) {
    let mut events_active = true;
    loop {
        tokio::select! {
            update = source.updated(), if events_active => {
                match update {
                    Ok(state) => publish_interface_state(&state_tx, &state, true),
                    Err(_) => {
                        events_active = false;
                        publish_events_unavailable(&state_tx);
                        warn!("network change event stream stopped; using on-demand snapshots");
                    }
                }
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                handle_monitor_command(command, &state_tx, events_active).await;
            }
        }
    }
}

async fn run_snapshot_monitor(
    state_tx: watch::Sender<ConnectivitySnapshot>,
    mut command_rx: mpsc::Receiver<MonitorCommand>,
) {
    while let Some(command) = command_rx.recv().await {
        handle_monitor_command(command, &state_tx, false).await;
    }
}

async fn handle_monitor_command(
    command: MonitorCommand,
    state_tx: &watch::Sender<ConnectivitySnapshot>,
    events_available: bool,
) {
    match command {
        MonitorCommand::Refresh { reply } => {
            publish_interface_state(state_tx, &InterfaceState::new().await, events_available);
            let _ = reply.send(*state_tx.borrow());
        }
    }
}

fn publish_events_unavailable(state_tx: &watch::Sender<ConnectivitySnapshot>) {
    let previous = *state_tx.borrow();
    state_tx.send_modify(|snapshot| {
        snapshot.events_available = false;
        snapshot.resumed = false;
        snapshot.generation = snapshot.generation.wrapping_add(1);
    });
    log_connectivity_change(previous, *state_tx.borrow());
}

fn publish_interface_state(
    state_tx: &watch::Sender<ConnectivitySnapshot>,
    state: &InterfaceState,
    events_available: bool,
) {
    let previous = *state_tx.borrow();
    let availability = interface_availability(state);
    let snapshot = ConnectivitySnapshot {
        availability,
        events_available,
        resumed: state.last_unsuspend.is_some(),
        generation: previous.generation.wrapping_add(1),
    };
    state_tx.send_replace(snapshot);
    log_connectivity_change(previous, snapshot);
}

fn interface_availability(state: &InterfaceState) -> NetworkAvailability {
    if !(state.have_v4 || state.have_v6) {
        NetworkAvailability::Offline
    } else if state.default_route_interface.is_some() {
        NetworkAvailability::Online
    } else {
        NetworkAvailability::Unknown
    }
}

fn log_connectivity_change(previous: ConnectivitySnapshot, current: ConnectivitySnapshot) {
    if previous.availability != current.availability {
        match current.availability {
            NetworkAvailability::Online => info!("system network path is available"),
            NetworkAvailability::Offline => info!("system network path is unavailable"),
            NetworkAvailability::Unknown => {
                info!("system network path has usable addresses but no detected default route")
            }
        }
    } else {
        debug!(
            availability = ?current.availability,
            resumed = current.resumed,
            events_available = current.events_available,
            "system network state refreshed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_connectivity_handle_keeps_its_sender_alive() {
        let handle = ConnectivityHandle::assume_online();
        assert_eq!(handle.current().availability, NetworkAvailability::Online);
        assert!(handle.state.has_changed().is_ok());
    }

    #[test]
    fn controlled_connectivity_publishes_transitions() {
        let (handle, controller) =
            ConnectivityHandle::controlled(NetworkAvailability::Offline, true);
        let initial = handle.current();
        controller.set(NetworkAvailability::Online);
        let current = handle.current();
        assert_eq!(current.availability, NetworkAvailability::Online);
        assert!(current.generation > initial.generation);
    }
}
