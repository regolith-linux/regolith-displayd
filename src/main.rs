use log::error;
use regolith_displayd::{confirm, night_light, DisplayManager, DisplayServer};
use std::{error::Error, future::pending, sync::Arc};
use swayipc_async::Connection as SwayConection;
use tokio::{sync::Mutex, try_join};

fn main() -> Result<(), Box<dyn Error>> {
    // The daemon runs itself with these arguments to show the prompt that
    // confirms a persistent display change.
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == confirm::PROMPT_ARG {
        let millis = args[3].parse().unwrap_or(0);
        std::process::exit(confirm::run_prompt(&args[2], millis));
    }
    daemon()
}

#[tokio::main]
async fn daemon() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    // New pointer to Display Manager Object
    let manager = DisplayManager::new().await;
    let manager_ref = Arc::new(Mutex::new(manager));
    let sway_connection = SwayConection::new().await.expect(
        "Unable to connect to sway ipc interface. Make sure sway is running and SWAYSOCK is set",
    );
    let sway_connection_ref = Arc::new(Mutex::new(sway_connection));
    let server =
        DisplayServer::new(Arc::clone(&manager_ref), Arc::clone(&sway_connection_ref)).await;
    server.run_server().await.unwrap();

    // Apply the color plugin's Night Light temperature, as Mutter does.
    night_light::spawn();

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
