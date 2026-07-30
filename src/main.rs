use log::{error, warn};
use regolith_displayd::{DisplayManager, DisplayServer};
use std::{error::Error, future::pending, sync::Arc};
use swayipc_async::Connection as SwayConection;
use tokio::{sync::Mutex, try_join};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    // New pointer to Display Manager Object
    let manager = DisplayManager::new().await;
    let manager_ref = Arc::new(Mutex::new(manager));
    let sway_connection_ref = SwayConection::new()
        .await
        .ok()
        .map(|connection| Arc::new(Mutex::new(connection)));
    if sway_connection_ref.is_none() {
        warn!("Sway IPC backend unavailable; continuing without Sway display observation");
    }

    let server = DisplayServer::new(Arc::clone(&manager_ref), sway_connection_ref.clone()).await;
    server.run_server().await?;

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
