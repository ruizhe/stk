use n0_watcher::Watcher;
use netwatch::{interfaces::State as InterfaceState, netmon::Monitor};
use std::sync::Arc;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::AbortHandle,
    time::Instant,
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
    pub(crate) resume_generation: u64,
    pub(crate) generation: u64,
}

impl ConnectivitySnapshot {
    fn initial() -> Self {
        Self {
            availability: NetworkAvailability::Unknown,
            events_available: false,
            resume_generation: 0,
            generation: 0,
        }
    }

    pub(crate) fn is_offline(self) -> bool {
        self.availability == NetworkAvailability::Offline
    }

    pub(crate) fn resumed_since(self, previous: Self) -> bool {
        self.resume_generation != previous.resume_generation
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
            resume_generation: 0,
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
            resume_generation: 0,
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
                let mut publisher = ConnectivityPublisher::new(state_tx);
                publisher.publish_interface_state(&source.get(), true);
                tokio::spawn(run_event_monitor(monitor, source, publisher, command_rx))
            }
            Err(error) => {
                warn!(
                    ?error,
                    "network change events are unavailable; using on-demand snapshots"
                );
                let mut publisher = ConnectivityPublisher::new(state_tx);
                publisher.publish_interface_state(&InterfaceState::new().await, false);
                tokio::spawn(run_snapshot_monitor(publisher, command_rx))
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

struct ConnectivityPublisher {
    state_tx: watch::Sender<ConnectivitySnapshot>,
    last_unsuspend: Option<Instant>,
}

impl ConnectivityPublisher {
    fn new(state_tx: watch::Sender<ConnectivitySnapshot>) -> Self {
        Self {
            state_tx,
            last_unsuspend: None,
        }
    }

    fn current(&self) -> ConnectivitySnapshot {
        *self.state_tx.borrow()
    }

    fn publish_interface_state(&mut self, state: &InterfaceState, events_available: bool) -> bool {
        self.publish_snapshot(
            interface_availability(state),
            events_available,
            state.last_unsuspend,
        )
    }

    fn publish_events_unavailable(&mut self) -> bool {
        self.publish_snapshot(self.current().availability, false, None)
    }

    fn publish_snapshot(
        &mut self,
        availability: NetworkAvailability,
        events_available: bool,
        last_unsuspend: Option<Instant>,
    ) -> bool {
        let previous = self.current();
        let resumed = last_unsuspend.is_some_and(|current| {
            self.last_unsuspend
                .is_none_or(|last_seen| current > last_seen)
        });
        if resumed {
            self.last_unsuspend = last_unsuspend;
        }
        let resume_generation = if resumed {
            previous.resume_generation.wrapping_add(1)
        } else {
            previous.resume_generation
        };
        if previous.availability == availability
            && previous.events_available == events_available
            && previous.resume_generation == resume_generation
        {
            return false;
        }
        let snapshot = ConnectivitySnapshot {
            availability,
            events_available,
            resume_generation,
            generation: previous.generation.wrapping_add(1),
        };
        self.state_tx.send_replace(snapshot);
        log_connectivity_change(previous, snapshot, resumed);
        true
    }
}

async fn run_event_monitor(
    _monitor: Monitor,
    mut source: n0_watcher::Direct<InterfaceState>,
    mut publisher: ConnectivityPublisher,
    mut command_rx: mpsc::Receiver<MonitorCommand>,
) {
    let mut events_active = true;
    loop {
        tokio::select! {
            update = source.updated(), if events_active => {
                match update {
                    Ok(state) => {
                        publisher.publish_interface_state(&state, true);
                    }
                    Err(_) => {
                        events_active = false;
                        publisher.publish_events_unavailable();
                        warn!("network change event stream stopped; using on-demand snapshots");
                    }
                }
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                handle_monitor_command(command, &mut publisher, events_active).await;
            }
        }
    }
}

async fn run_snapshot_monitor(
    mut publisher: ConnectivityPublisher,
    mut command_rx: mpsc::Receiver<MonitorCommand>,
) {
    while let Some(command) = command_rx.recv().await {
        handle_monitor_command(command, &mut publisher, false).await;
    }
}

async fn handle_monitor_command(
    command: MonitorCommand,
    publisher: &mut ConnectivityPublisher,
    events_available: bool,
) {
    match command {
        MonitorCommand::Refresh { reply } => {
            publisher.publish_interface_state(&InterfaceState::new().await, events_available);
            let _ = reply.send(publisher.current());
        }
    }
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

fn log_connectivity_change(
    previous: ConnectivitySnapshot,
    current: ConnectivitySnapshot,
    resumed: bool,
) {
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
            resumed,
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

    #[test]
    fn duplicate_network_snapshots_are_not_rebroadcast() {
        let (state_tx, mut state_rx) = watch::channel(ConnectivitySnapshot::initial());
        let mut publisher = ConnectivityPublisher::new(state_tx);

        assert!(publisher.publish_snapshot(NetworkAvailability::Online, true, None));
        state_rx.borrow_and_update();
        let published = publisher.current();

        assert!(!publisher.publish_snapshot(NetworkAvailability::Online, true, None));
        assert_eq!(publisher.current(), published);
        assert!(!state_rx.has_changed().unwrap());
    }

    #[test]
    fn unsuspend_timestamp_is_published_once_as_an_event_generation() {
        let (state_tx, mut state_rx) = watch::channel(ConnectivitySnapshot::initial());
        let mut publisher = ConnectivityPublisher::new(state_tx);
        let first_resume = Instant::now();

        assert!(publisher.publish_snapshot(NetworkAvailability::Online, true, Some(first_resume),));
        state_rx.borrow_and_update();
        let first = publisher.current();
        assert_eq!(first.resume_generation, 1);

        assert!(
            !publisher.publish_snapshot(NetworkAvailability::Online, true, Some(first_resume),)
        );
        assert!(!publisher.publish_snapshot(NetworkAvailability::Online, true, None));
        assert!(!state_rx.has_changed().unwrap());

        assert!(publisher.publish_snapshot(
            NetworkAvailability::Online,
            true,
            Some(first_resume + std::time::Duration::from_secs(1)),
        ));
        let second = publisher.current();
        assert_eq!(second.resume_generation, 2);
        assert!(second.resumed_since(first));
    }
}
