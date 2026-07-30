use log::{error, info, warn};
use regolith_displayd::wayland_observer::{
    OutputSnapshot, WaylandObserverError, WaylandOutputObserver,
};
use regolith_displayd::{DisplayManager, DisplayServer};
use std::{error::Error, future::pending, sync::Arc, time::Duration};
use swayipc_async::Connection as SwayConection;
use tokio::{sync::Mutex, task::JoinHandle, try_join};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    // New pointer to Display Manager Object
    let manager = DisplayManager::new().await;
    let manager_ref = Arc::new(Mutex::new(manager));
    let sway_connection_ref = connect_sway_backend().await?;

    let wayland_observer_handle = if sway_connection_ref.is_none() {
        start_wayland_state_observer(Arc::clone(&manager_ref))
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
            "{message}; continuing without the Sway backend and switching to Wayland output observation for in-memory state updates only; continuous persistence is still pending"
        );
        Ok(None)
    } else {
        Err(message.into())
    }
}

fn start_wayland_state_observer(manager_ref: Arc<Mutex<DisplayManager>>) -> Option<JoinHandle<()>> {
    match WaylandOutputObserver::observe() {
        Ok(receiver) => {
            info!(
                "Starting Wayland output observation for COSMIC without Sway; this updates DisplayManager state only and continuous persistence is still pending"
            );
            Some(tokio::task::spawn_blocking(move || {
                consume_wayland_observer(manager_ref, receiver);
            }))
        }
        Err(error) => {
            warn!(
                "Wayland output observation unavailable at startup: {error}; keeping empty/prior state"
            );
            None
        }
    }
}

fn consume_wayland_observer(
    manager_ref: Arc<Mutex<DisplayManager>>,
    receiver: std::sync::mpsc::Receiver<Result<OutputSnapshot, WaylandObserverError>>,
) {
    let runtime = tokio::runtime::Handle::current();

    // COSMIC fallback only updates in-memory DisplayManager state here.
    // Writing persistence artifacts remains out of scope for this slice.
    while let Ok(result) = receiver.recv() {
        match result {
            Ok(snapshot) => apply_wayland_snapshot(&runtime, &manager_ref, snapshot),
            Err(error) => {
                warn!(
                    "Wayland output observation stopped after state-only updates: {error}; continuous persistence is still pending"
                );
                break;
            }
        }
    }

    info!("Wayland output observation receiver closed; state-only loop exiting");
}

fn apply_wayland_snapshot(
    runtime: &tokio::runtime::Handle,
    manager_ref: &Arc<Mutex<DisplayManager>>,
    snapshot: OutputSnapshot,
) {
    runtime.block_on(async {
        let mut manager = manager_ref.lock().await;
        if let Err(error) = manager.replace_from_wayland_snapshot(&snapshot) {
            warn!(
                "Rejected incomplete Wayland display snapshot serial {}: {error}; keeping prior state",
                snapshot.serial
            );
        }
    });
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
