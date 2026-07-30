use log::{error, info, warn};
use regolith_displayd::wayland_observer::{
    OutputSnapshot, WaylandObserverError, WaylandOutputObserver,
};
use regolith_displayd::{wayland_side_effects_required, DisplayManager, DisplayServer};
use std::{error::Error, future::pending, sync::Arc, time::Duration};
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
        ready
            .await
            .map_err(|_| "Wayland observer readiness channel closed")??;
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
        DisplayManager::watch_changes(manager_ref, sway_connection_ref)
            .await
            .unwrap();
    });

    if let Err(e) = try_join!(watch_handle) {
        error!("{}", e);
    }
    pending::<()>().await;
    Ok(())
}

const SWAY_CONNECT_ATTEMPTS: usize = 3;
const SWAY_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(250);

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

    let mut pending_side_effects = false;
    // COSMIC snapshots use the same persistence and signal path as Sway observations.
    while let Ok(result) = receiver.recv() {
        match result {
            Ok(snapshot) => {
                let install = runtime.block_on(DisplayManager::install_wayland_snapshot(
                    Arc::clone(&manager_ref),
                    &snapshot,
                ));
                match install {
                    Ok(state_changed) => {
                        if let Some(sender) = ready_sender.take() {
                            let _ = sender.send(Ok(()));
                        }
                        if wayland_side_effects_required(state_changed, pending_side_effects) {
                            pending_side_effects = runtime.block_on(
                                DisplayManager::persist_wayland_state(Arc::clone(&manager_ref)),
                            );
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
                break;
            }
        }
    }

    info!("Wayland output observation receiver closed; state-only loop exiting");
}

#[cfg(test)]
mod tests {
    use super::cosmic_desktop;

    #[test]
    fn identifies_cosmic_desktop_in_composite_value() {
        assert!(cosmic_desktop(Some("GNOME:COSMIC")));
    }

    #[test]
    fn does_not_treat_other_desktops_as_cosmic() {
        assert!(!cosmic_desktop(Some("GNOME")));
        assert!(!cosmic_desktop(None));
    }
}
