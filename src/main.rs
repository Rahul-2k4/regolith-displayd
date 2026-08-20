use log::{error, info, warn};
use regolith_displayd::wayland_observer::{
    OutputSnapshot, WaylandApplyHandle, WaylandObserverError, WaylandOutputObserver,
};
use regolith_displayd::{
    wayland_side_effect_stage, DisplayManager, DisplayServer, WaylandSideEffectStage,
};
use std::{
    error::Error,
    future::Future,
    sync::{mpsc::RecvTimeoutError, Arc},
    time::Duration,
};
use swayipc_async::Connection as SwayConection;
use tokio::{
    sync::{oneshot, Mutex},
    task::JoinHandle,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    // New pointer to Display Manager Object
    let manager = DisplayManager::new().await;
    let manager_ref = Arc::new(Mutex::new(manager));
    let cosmic = cosmic_desktop(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref());
    let sway_connection_ref = connect_sway_backend().await?;
    let backend = select_display_backend(cosmic, sway_connection_ref.is_some());

    // The apply handle is wired into DisplayServer in a follow-up change;
    // for now it's kept alive but unused so this crate keeps compiling
    // against WaylandOutputObserver::observe()'s new tuple return.
    let mut _wayland_apply_handle: Option<WaylandApplyHandle> = None;
    let wayland_observer_handle = if backend == DisplayBackend::Wayland {
        let (handle, ready, apply_handle) = start_wayland_state_observer(Arc::clone(&manager_ref))?;
        _wayland_apply_handle = Some(apply_handle);
        match wait_for_wayland_readiness(ready, WAYLAND_READINESS_TIMEOUT).await {
            Ok(false) => {
                warn!(
                    "Wayland observer readiness timed out after {:?}; registering D-Bus and continuing observation",
                    WAYLAND_READINESS_TIMEOUT
                );
            }
            Ok(true) => {}
            Err(error) => {
                let observer_result = handle.await;
                return Err(finish_wayland_startup_failure(error, observer_result));
            }
        }
        Some(handle)
    } else {
        None
    };

    // COSMIC observes output events through Wayland while retaining Sway IPC for D-Bus compatibility.
    let server = DisplayServer::new(Arc::clone(&manager_ref), sway_connection_ref.clone()).await;
    server.run_server().await?;

    if let Some(observer_handle) = wayland_observer_handle {
        let observer_result = match observer_handle.await {
            Ok(result) => result,
            Err(error) => return Err(format!("Wayland observer task failed: {error}").into()),
        };
        return finish_wayland_observer(observer_result);
    }

    let Some(sway_connection) = sway_connection_ref else {
        return Err("No display observer is available".into());
    };
    let watch_handle = tokio::spawn(supervise_sway_watcher(
        move || {
            let manager_ref = Arc::clone(&manager_ref);
            let sway_connection = Arc::clone(&sway_connection);
            async move {
                DisplayManager::watch_changes(manager_ref, Some(sway_connection))
                    .await
                    .map_err(|error| error.to_string())
            }
        },
        WATCH_RESTART_DELAY,
    ));

    return finish_sway_watcher(watch_handle.await);
}

fn finish_sway_watcher(
    result: Result<Result<(), String>, tokio::task::JoinError>,
) -> Result<(), Box<dyn Error>> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(error) => Err(format!("Display watcher task failed: {error}").into()),
    }
}

fn watcher_should_restart(result: &Result<(), String>) -> bool {
    result.is_err()
}

async fn supervise_sway_watcher<Watch, WatchFuture>(
    mut watch: Watch,
    restart_delay: Duration,
) -> Result<(), String>
where
    Watch: FnMut() -> WatchFuture,
    WatchFuture: Future<Output = Result<(), String>>,
{
    loop {
        let result = watch().await;
        let should_restart = watcher_should_restart(&result);
        handle_watch_changes_result(result);
        if !should_restart {
            return Ok(());
        }
        warn!("Display watcher will restart after {:?}", restart_delay);
        tokio::time::sleep(restart_delay).await;
    }
}

fn handle_watch_changes_result(result: Result<(), String>) {
    if let Err(error) = result {
        error!("Display watcher stopped: {error}");
    }
}

fn finish_wayland_observer(result: Result<(), String>) -> Result<(), Box<dyn Error>> {
    match result {
        Ok(()) => {
            info!("Wayland observer completed normally");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn finish_wayland_startup_failure(
    readiness_error: Box<dyn Error>,
    observer_result: Result<Result<(), String>, tokio::task::JoinError>,
) -> Box<dyn Error> {
    match observer_result {
        Ok(Err(error)) => error.into(),
        Err(error) => format!("Wayland observer task failed: {error}").into(),
        Ok(Ok(())) => readiness_error,
    }
}

fn record_wayland_pending_failure(attempts: &mut usize, error: String) -> Result<(), String> {
    *attempts += 1;
    if *attempts >= WAYLAND_MAX_PENDING_ATTEMPTS {
        Err(format!(
            "Wayland pending side effect failed after {WAYLAND_MAX_PENDING_ATTEMPTS} attempts: {error}"
        ))
    } else {
        Ok(())
    }
}

const SWAY_CONNECT_ATTEMPTS: usize = 3;
const SWAY_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(250);
const WAYLAND_RETRY_DELAY: Duration = Duration::from_millis(100);
const WAYLAND_READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const WAYLAND_MAX_PENDING_ATTEMPTS: usize = 3;
const WATCH_RESTART_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayBackend {
    Wayland,
    Sway,
    Unsupported,
}

fn select_display_backend(cosmic: bool, sway_available: bool) -> DisplayBackend {
    match (cosmic, sway_available) {
        (true, _) => DisplayBackend::Wayland,
        (false, true) => DisplayBackend::Sway,
        (false, false) => DisplayBackend::Unsupported,
    }
}

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
) -> Result<
    (
        JoinHandle<Result<(), String>>,
        oneshot::Receiver<Result<(), String>>,
        WaylandApplyHandle,
    ),
    Box<dyn Error>,
> {
    let (receiver, apply_handle) = WaylandOutputObserver::observe()?;
    let (ready_sender, ready_receiver) = oneshot::channel();
    info!("Starting Wayland output observation for COSMIC");
    let handle = tokio::task::spawn_blocking(move || {
        consume_wayland_observer(manager_ref, receiver, Some(ready_sender))
    });
    Ok((handle, ready_receiver, apply_handle))
}

fn consume_wayland_observer(
    manager_ref: Arc<Mutex<DisplayManager>>,
    receiver: std::sync::mpsc::Receiver<Result<OutputSnapshot, WaylandObserverError>>,
    mut ready_sender: Option<oneshot::Sender<Result<(), String>>>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Handle::current();

    let mut pending_manager = None;
    let mut pending_stage = None;
    let mut pending_attempts = 0;
    // COSMIC snapshots use the same persistence and signal path as Sway observations.
    loop {
        let result = match pending_stage {
            Some(_) => match receiver.recv_timeout(WAYLAND_RETRY_DELAY) {
                Ok(result) => result,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(manager) = pending_manager.clone() {
                        let result = process_wayland_candidate(
                            &runtime,
                            &manager_ref,
                            Arc::new(Mutex::new(manager)),
                            false,
                            &mut pending_stage,
                            &mut pending_manager,
                            &mut ready_sender,
                        );
                        if let Err(error) = result {
                            if let Err(error) =
                                record_wayland_pending_failure(&mut pending_attempts, error)
                            {
                                return Err(error);
                            }
                        } else {
                            pending_attempts = 0;
                        }
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("Wayland observer receiver closed".to_string());
                }
            },
            None => match receiver.recv() {
                Ok(result) => result,
                Err(_) => return Err("Wayland observer receiver closed".to_string()),
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
                        let result = process_wayland_candidate(
                            &runtime,
                            &manager_ref,
                            candidate_manager,
                            state_changed,
                            &mut pending_stage,
                            &mut pending_manager,
                            &mut ready_sender,
                        );
                        if let Err(error) = result {
                            if let Err(error) =
                                record_wayland_pending_failure(&mut pending_attempts, error)
                            {
                                return Err(error);
                            }
                        } else {
                            pending_attempts = 0;
                        }
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
                return Err(error.to_string());
            }
        }
    }
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
) -> Result<(), String> {
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
        return Ok(());
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
        return match result {
            Ok(()) => {
                *pending_stage = None;
                *pending_manager = None;
                Ok(())
            }
            Err(error) => {
                error!("Error emitting MonitorsChanged: {error}");
                *pending_stage = Some(WaylandSideEffectStage::Signal);
                *pending_manager = Some(candidate);
                Err(error)
            }
        };
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
                    Ok(())
                }
                Err(error) => {
                    error!("Error emitting MonitorsChanged: {error}");
                    *pending_stage = Some(WaylandSideEffectStage::Signal);
                    *pending_manager = Some(candidate);
                    Err(error)
                }
            }
        }
        Ok(next_stage) => {
            *pending_stage = Some(next_stage);
            *pending_manager = Some(candidate);
            Ok(())
        }
        Err(error) => {
            warn!("Unable to advance Wayland display side effects at {stage:?}: {error}");
            *pending_stage = Some(stage);
            *pending_manager = Some(candidate);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regolith_displayd::wayland_observer::{OutputHeadSnapshot, OutputModeSnapshot};

    #[test]
    fn sway_supervisor_normal_completion_returns_cleanly() {
        assert!(finish_sway_watcher(Ok(Ok(()))).is_ok());
    }

    #[test]
    fn sway_supervisor_join_failure_is_returned() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(async {
            let handle = tokio::spawn(async {
                panic!("watcher panic");
            });
            finish_sway_watcher(handle.await)
        });
        assert!(
            matches!(result, Err(error) if error.to_string().contains("Display watcher task failed"))
        );
    }

    #[test]
    fn restarts_after_initial_watcher_failure() {
        assert!(watcher_should_restart(&Err(
            "initial monitor info failed".to_string()
        )));
        assert!(!watcher_should_restart(&Ok(())));
    }

    #[tokio::test]
    async fn sway_supervisor_recovers_after_initial_monitor_info_failure() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempt_count = Arc::clone(&attempts);
        let result = supervise_sway_watcher(
            move || {
                let attempt = attempt_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err("initial monitor info failed".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
            Duration::ZERO,
        )
        .await;

        assert_eq!(result, Ok(()));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn sway_supervisor_does_not_restart_after_normal_completion() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempt_count = Arc::clone(&attempts);
        let result = supervise_sway_watcher(
            move || {
                attempt_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
            Duration::ZERO,
        )
        .await;

        assert_eq!(result, Ok(()));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn early_wayland_observer_failure_is_preserved_over_readiness_error() {
        let readiness_error =
            std::io::Error::new(std::io::ErrorKind::Other, "readiness channel closed");
        let error = finish_wayland_startup_failure(
            Box::new(readiness_error),
            Ok(Err("observer failed before readiness".to_string())),
        );

        assert_eq!(error.to_string(), "observer failed before readiness");
    }

    #[test]
    fn readiness_error_is_used_when_observer_completed_cleanly() {
        let readiness_error =
            std::io::Error::new(std::io::ErrorKind::Other, "readiness channel closed");
        let error = finish_wayland_startup_failure(Box::new(readiness_error), Ok(Ok(())));

        assert_eq!(error.to_string(), "readiness channel closed");
    }

    #[tokio::test]
    async fn wayland_observer_join_error_is_preserved() {
        let handle = tokio::spawn(async {
            panic!("observer panic");
        });
        let join_error = handle.await.unwrap_err();
        let readiness_error =
            std::io::Error::new(std::io::ErrorKind::Other, "readiness channel closed");
        let error = finish_wayland_startup_failure(Box::new(readiness_error), Err(join_error));

        assert!(error.to_string().contains("Wayland observer task failed"));
    }

    #[test]
    fn normal_wayland_completion_is_not_reclassified_as_failure() {
        assert!(finish_wayland_observer(Ok(())).is_ok());
    }

    #[tokio::test]
    async fn closed_wayland_receiver_is_returned_by_consumer() {
        let (sender, receiver) =
            std::sync::mpsc::channel::<Result<OutputSnapshot, WaylandObserverError>>();
        drop(sender);
        let manager = Arc::new(Mutex::new(DisplayManager::new().await));
        let result =
            tokio::task::spawn_blocking(move || consume_wayland_observer(manager, receiver, None))
                .await
                .unwrap();

        assert_eq!(result, Err("Wayland observer receiver closed".to_string()));
    }

    #[test]
    fn terminal_wayland_failure_is_surfaced() {
        let result = finish_wayland_observer(Err("observer disconnected".to_string()));
        assert!(matches!(result, Err(error) if error.to_string() == "observer disconnected"));
    }

    #[test]
    fn pending_wayland_side_effect_exhaustion_is_returned() {
        let mut attempts = 0;
        assert!(record_wayland_pending_failure(&mut attempts, "write failed".to_string()).is_ok());
        assert!(record_wayland_pending_failure(&mut attempts, "write failed".to_string()).is_ok());
        let result = record_wayland_pending_failure(&mut attempts, "write failed".to_string());
        assert!(matches!(
            result,
            Err(error) if error.contains("after 3 attempts") && error.contains("write failed")
        ));
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
    fn cosmic_with_sway_selects_wayland() {
        assert_eq!(select_display_backend(true, true), DisplayBackend::Wayland);
    }

    #[test]
    fn cosmic_without_sway_selects_wayland() {
        assert_eq!(select_display_backend(true, false), DisplayBackend::Wayland);
    }

    #[test]
    fn gnome_with_sway_selects_sway() {
        assert_eq!(select_display_backend(false, true), DisplayBackend::Sway);
    }

    #[test]
    fn gnome_without_sway_is_unsupported() {
        assert_eq!(
            select_display_backend(false, false),
            DisplayBackend::Unsupported
        );
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
