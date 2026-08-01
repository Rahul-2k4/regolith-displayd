use log::{error, info, warn};
use regolith_displayd::wayland_observer::{
    OutputSnapshot, WaylandObserverError, WaylandOutputObserver,
};
use regolith_displayd::{
    wayland_side_effect_stage, DisplayManager, DisplayServer, WaylandSideEffectStage,
};
use std::{
    error::Error,
    future::{pending, Future},
    sync::{mpsc::RecvTimeoutError, Arc},
    time::Duration,
};
use swayipc_async::Connection as SwayConection;
use tokio::{
    sync::{oneshot, Mutex},
    task::JoinHandle,
    try_join,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    // New pointer to Display Manager Object
    let manager = DisplayManager::new().await;
    let manager_ref = Arc::new(Mutex::new(manager));
    let sway_connection_ref = connect_sway_backend().await?;

    let wayland_observer = if sway_connection_ref.is_none() {
        start_wayland_state_observer(Arc::clone(&manager_ref))
    } else {
        None
    };

    let wayland_observer_handle = if let Some((handle, ready)) = wayland_observer {
        if !wait_for_wayland_readiness(ready, WAYLAND_READINESS_TIMEOUT).await? {
            warn!(
                "Wayland observer readiness timed out after {:?}; registering D-Bus and continuing observation",
                WAYLAND_READINESS_TIMEOUT
            );
        }
        Some(handle)
    } else {
        None
    };

    let server = DisplayServer::new(Arc::clone(&manager_ref), sway_connection_ref.clone()).await;
    server.run_server().await?;

    if let Some(observer_handle) = wayland_observer_handle {
        tokio::spawn(async move {
            if let Err(error) = observer_handle.await {
                error!("Wayland observer task join failure: {error}");
            }
        });
    }

    let watch_handle = tokio::spawn(async move {
        loop {
            let result = DisplayManager::watch_changes(
                Arc::clone(&manager_ref),
                sway_connection_ref.clone(),
            )
            .await
            .map_err(|error| error.to_string());
            let should_restart = watcher_should_restart(&result);
            handle_watch_changes_result(result);
            if !should_restart {
                break;
            }
            warn!(
                "Display watcher will restart after {:?}",
                WATCH_RESTART_DELAY
            );
            tokio::time::sleep(WATCH_RESTART_DELAY).await;
        }
    });

    if let Err(e) = try_join!(watch_handle) {
        error!("{}", e);
    }
    pending::<()>().await;
    Ok(())
}

fn watcher_should_restart(result: &Result<(), String>) -> bool {
    result.is_err()
}

fn handle_watch_changes_result(result: Result<(), String>) {
    if let Err(error) = result {
        error!("Display watcher stopped: {error}");
    }
}

const SWAY_CONNECT_ATTEMPTS: usize = 3;
const SWAY_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(250);
const WAYLAND_RETRY_DELAY: Duration = Duration::from_millis(100);
const WAYLAND_READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const WATCH_RESTART_DELAY: Duration = Duration::from_secs(1);

fn cosmic_desktop(value: Option<&str>) -> bool {
    value
        .unwrap_or_default()
        .split(":")
        .any(|desktop| desktop.eq_ignore_ascii_case("cosmic"))
}

async fn connect_sway_backend() -> Result<Option<Arc<Mutex<SwayConection>>>, Box<dyn Error>> {
    let cosmic = cosmic_desktop(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref());
    let mut last_error = None;

    for attempt in 1..=SWAY_CONNECT_ATTEMPTS {
        match SwayConection::new().await {
            Ok(connection) => return Ok(Some(Arc::new(Mutex::new(connection)))),
            Err(error) => {
                let message = error.to_string();
                warn!(
                    "Sway IPC connection attempt {attempt}/{SWAY_CONNECT_ATTEMPTS} failed: {message}"
                );
                last_error = Some(message);
                if attempt < SWAY_CONNECT_ATTEMPTS {
                    tokio::time::sleep(SWAY_CONNECT_RETRY_DELAY).await;
                }
            }
        }
    }

    let message = format!(
        "Sway IPC backend unavailable after {SWAY_CONNECT_ATTEMPTS} attempts: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    );
    if cosmic {
        warn!(
            "{message}; continuing without the Sway backend and switching to Wayland output observation"
        );
        Ok(None)
    } else {
        Err(message.into())
    }
}

fn start_wayland_state_observer(
    manager_ref: Arc<Mutex<DisplayManager>>,
) -> Option<(JoinHandle<()>, oneshot::Receiver<Result<(), String>>)> {
    match WaylandOutputObserver::observe() {
        Ok(receiver) => {
            let (ready_sender, ready_receiver) = oneshot::channel();
            info!("Starting Wayland output observation for COSMIC without Sway");
            let handle = tokio::task::spawn_blocking(move || {
                consume_wayland_observer(manager_ref, receiver, Some(ready_sender));
            });
            Some((handle, ready_receiver))
        }
        Err(error) => {
            warn!("Wayland output observation unavailable at startup: {error}");
            None
        }
    }
}

fn consume_wayland_observer(
    manager_ref: Arc<Mutex<DisplayManager>>,
    receiver: std::sync::mpsc::Receiver<Result<OutputSnapshot, WaylandObserverError>>,
    mut ready_sender: Option<oneshot::Sender<Result<(), String>>>,
) {
    let runtime = tokio::runtime::Handle::current();

    let mut pending_manager = None;
    let mut pending_stage = None;
    // COSMIC snapshots use the same persistence and signal path as Sway observations.
    loop {
        let result = match pending_stage {
            Some(_) => match receiver.recv_timeout(WAYLAND_RETRY_DELAY) {
                Ok(result) => result,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(manager) = pending_manager.clone() {
                        process_wayland_candidate(
                            &runtime,
                            &manager_ref,
                            Arc::new(Mutex::new(manager)),
                            false,
                            &mut pending_stage,
                            &mut pending_manager,
                            &mut ready_sender,
                        );
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match receiver.recv() {
                Ok(result) => result,
                Err(_) => break,
            },
        };
        match result {
            Ok(snapshot) => {
                let base_manager = pending_manager.clone().unwrap_or_else(|| {
                    runtime.block_on(async { manager_ref.lock().await.clone() })
                });
                let candidate_manager = Arc::new(Mutex::new(base_manager));
                let install = runtime.block_on(DisplayManager::install_wayland_snapshot(
                    Arc::clone(&candidate_manager),
                    &snapshot,
                ));
                match install {
                    Ok(state_changed) => {
                        process_wayland_candidate(
                            &runtime,
                            &manager_ref,
                            candidate_manager,
                            state_changed,
                            &mut pending_stage,
                            &mut pending_manager,
                            &mut ready_sender,
                        );
                    }
                    Err(error) => {
                        warn!(
                            "Wayland snapshot rejected for serial {}: {}",
                            snapshot.serial, error
                        );
                    }
                }
            }
            Err(error) => {
                warn!("Wayland output observation stopped: {error}");
                if let Some(sender) = ready_sender.take() {
                    let _ = sender.send(Err(error.to_string()));
                }
                break;
            }
        }
    }

    if let Some(sender) = ready_sender.take() {
        let _ = sender.send(Err("Wayland observer receiver closed".to_string()));
    }
    info!("Wayland output observation receiver closed; state-only loop exiting");
}

fn notify_wayland_readiness(
    ready_sender: &mut Option<oneshot::Sender<Result<(), String>>>,
    result: Result<(), String>,
) {
    if let Some(sender) = ready_sender.take() {
        let _ = sender.send(result);
    }
}

async fn wait_for_wayland_readiness(
    ready: oneshot::Receiver<Result<(), String>>,
    timeout: Duration,
) -> Result<bool, Box<dyn Error>> {
    match tokio::time::timeout(timeout, ready).await {
        Ok(Ok(Ok(()))) => Ok(true),
        Ok(Ok(Err(error))) => Err(error.into()),
        Ok(Err(_)) => Err("Wayland observer readiness channel closed".into()),
        Err(_) => Ok(false),
    }
}

async fn publish_wayland_state_before_readiness(
    manager_ref: &Arc<Mutex<DisplayManager>>,
    candidate: DisplayManager,
    ready_sender: &mut Option<oneshot::Sender<Result<(), String>>>,
) {
    *manager_ref.lock().await = candidate;
    notify_wayland_readiness(ready_sender, Ok(()));
}

async fn publish_wayland_state_before_signal<Signal, SignalFuture>(
    manager_ref: &Arc<Mutex<DisplayManager>>,
    candidate: DisplayManager,
    signal: Signal,
) -> Result<(), String>
where
    Signal: FnOnce() -> SignalFuture,
    SignalFuture: Future<Output = Result<(), String>>,
{
    *manager_ref.lock().await = candidate;
    signal().await
}

fn process_wayland_candidate(
    runtime: &tokio::runtime::Handle,
    manager_ref: &Arc<Mutex<DisplayManager>>,
    candidate_manager: Arc<Mutex<DisplayManager>>,
    state_changed: bool,
    pending_stage: &mut Option<WaylandSideEffectStage>,
    pending_manager: &mut Option<DisplayManager>,
    ready_sender: &mut Option<oneshot::Sender<Result<(), String>>>,
) {
    let candidate = runtime.block_on(async { candidate_manager.lock().await.clone() });
    runtime.block_on(publish_wayland_state_before_readiness(
        manager_ref,
        candidate.clone(),
        ready_sender,
    ));

    let stage = wayland_side_effect_stage(
        state_changed,
        *pending_stage,
        candidate.has_observed_outputs(),
    );
    let Some(stage) = stage else {
        *pending_stage = None;
        *pending_manager = None;
        runtime.block_on(async {
            *manager_ref.lock().await = candidate;
        });
        return;
    };

    if stage == WaylandSideEffectStage::Signal {
        let result = runtime.block_on(publish_wayland_state_before_signal(
            manager_ref,
            candidate.clone(),
            || async {
                DisplayManager::emit_monitors_changed()
                    .await
                    .map_err(|error| error.to_string())
            },
        ));
        match result {
            Ok(()) => {
                *pending_stage = None;
                *pending_manager = None;
            }
            Err(error) => {
                error!("Error emitting MonitorsChanged: {error}");
                *pending_stage = Some(WaylandSideEffectStage::Signal);
                *pending_manager = Some(candidate);
            }
        }
        return;
    }

    match runtime.block_on(DisplayManager::advance_wayland_side_effect(
        Arc::clone(&candidate_manager),
        stage,
    )) {
        Ok(next_stage) if next_stage == WaylandSideEffectStage::Signal => {
            let result = runtime.block_on(publish_wayland_state_before_signal(
                manager_ref,
                candidate.clone(),
                || async {
                    DisplayManager::emit_monitors_changed()
                        .await
                        .map_err(|error| error.to_string())
                },
            ));
            match result {
                Ok(()) => {
                    *pending_stage = None;
                    *pending_manager = None;
                }
                Err(error) => {
                    error!("Error emitting MonitorsChanged: {error}");
                    *pending_stage = Some(WaylandSideEffectStage::Signal);
                    *pending_manager = Some(candidate);
                }
            }
        }
        Ok(next_stage) => {
            *pending_stage = Some(next_stage);
            *pending_manager = Some(candidate);
        }
        Err(error) => {
            warn!("Unable to advance Wayland display side effects at {stage:?}: {error}");
            *pending_stage = Some(stage);
            *pending_manager = Some(candidate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regolith_displayd::wayland_observer::{OutputHeadSnapshot, OutputModeSnapshot};

    #[test]
    fn restarts_after_initial_watcher_failure() {
        assert!(watcher_should_restart(&Err(
            "initial monitor info failed".to_string()
        )));
        assert!(!watcher_should_restart(&Ok(())));
    }

    #[test]
    fn handles_watch_changes_error_without_panicking() {
        handle_watch_changes_result(Err("watcher stopped".into()));
    }

    #[test]
    fn identifies_cosmic_desktop_in_composite_value() {
        assert!(cosmic_desktop(Some("GNOME:COSMIC")));
    }

    #[test]
    fn does_not_treat_other_desktops_as_cosmic() {
        assert!(!cosmic_desktop(Some("GNOME")));
        assert!(!cosmic_desktop(None));
    }

    #[tokio::test]
    async fn publishes_manager_state_before_emitting_signal() {
        let manager = Arc::new(Mutex::new(DisplayManager::new().await));
        let candidate = DisplayManager::new().await;
        let expected = candidate.clone();
        let observed = Arc::clone(&manager);

        publish_wayland_state_before_signal(&manager, candidate, move || async move {
            assert_eq!(*observed.lock().await, expected);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn readiness_timeout_allows_startup_to_continue() {
        let (_sender, receiver) = oneshot::channel();

        assert!(
            !wait_for_wayland_readiness(receiver, Duration::from_millis(1))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn readiness_is_available_before_side_effect_failure() {
        let (sender, receiver) = oneshot::channel();
        let mut ready_sender = Some(sender);

        notify_wayland_readiness(&mut ready_sender, Ok(()));
        assert_eq!(receiver.await.unwrap(), Ok(()));
        assert!(ready_sender.is_none());
    }
    #[tokio::test]
    async fn candidate_state_is_visible_when_readiness_is_released() {
        let manager = Arc::new(Mutex::new(DisplayManager::new().await));
        let candidate = Arc::new(Mutex::new(DisplayManager::new().await));
        DisplayManager::install_wayland_snapshot(
            Arc::clone(&candidate),
            &OutputSnapshot {
                serial: 42,
                heads: vec![OutputHeadSnapshot {
                    name: "HDMI-A-1".to_string(),
                    description: None,
                    enabled: true,
                    position: Some((0, 0)),
                    transform: Some(0),
                    scale: Some(1.0),
                    current_mode: Some(OutputModeSnapshot {
                        width: 1920,
                        height: 1080,
                        refresh_mhz: Some(60_000),
                        preferred: true,
                        current: true,
                    }),
                    modes: Vec::new(),
                }],
            },
        )
        .await
        .unwrap();
        let expected = candidate.lock().await.clone();
        let (sender, receiver) = oneshot::channel();
        let mut ready_sender = Some(sender);

        publish_wayland_state_before_readiness(&manager, expected.clone(), &mut ready_sender).await;

        assert_eq!(receiver.await.unwrap(), Ok(()));
        assert!(manager.lock().await.has_observed_outputs());
        assert_eq!(*manager.lock().await, expected);
    }

    #[tokio::test]
    async fn side_effect_failure_does_not_delay_readiness_after_state_commit() {
        let manager = Arc::new(Mutex::new(DisplayManager::new().await));
        let candidate = DisplayManager::new().await;
        let (sender, receiver) = oneshot::channel();
        let mut ready_sender = Some(sender);

        publish_wayland_state_before_readiness(&manager, candidate.clone(), &mut ready_sender)
            .await;
        let side_effect = publish_wayland_state_before_signal(&manager, candidate, || async {
            Err("side effect failed".to_string())
        })
        .await;

        assert_eq!(receiver.await.unwrap(), Ok(()));
        assert_eq!(side_effect, Err("side effect failed".to_string()));
    }

    #[tokio::test]
    async fn observer_failure_releases_startup_readiness_waiter() {
        let (sender, receiver) = oneshot::channel();
        let mut ready_sender = Some(sender);

        notify_wayland_readiness(&mut ready_sender, Err("observer stopped".to_string()));
        assert_eq!(receiver.await.unwrap(), Err("observer stopped".to_string()));
    }
}
