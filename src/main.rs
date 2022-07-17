use log::error;
use regolith_displayd::{DisplayManager, DisplayServer};
use std::{error::Error, future::pending, sync::Arc};
use tokio::{sync::Mutex, try_join};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    // New pointer to Display Manager Object
    let manager = DisplayManager::new().await;
    let manager = Arc::new(Mutex::new(manager));
    let server = DisplayServer::new(Arc::clone(&manager));

    let connection = server.run_server().await.unwrap();

    let watch_handle = tokio::spawn(async move {
        DisplayManager::watch_changes(manager, &connection)
            .await
            .unwrap();
    });

    if let Err(e) = try_join!(watch_handle) {
        error!("{}", e);
    }
    pending::<()>().await;
    Ok(())
}
