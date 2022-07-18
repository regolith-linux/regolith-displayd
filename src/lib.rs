pub mod modes;
pub mod monitor;

use log::{error, info};
use monitor::{LogicalMonitor, Monitor};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs::{self, File},
    io::Read,
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};
use swayipc_async::Connection;
use tokio::sync::Mutex;
use zbus::{dbus_interface, ConnectionBuilder, SignalContext};
use zvariant::{DeserializeDict, SerializeDict, Type};

/// Stores configrations, interacts with sway IPC and monitors hardware changes
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DisplayManager {
    serial: u32,
    monitors: Vec<Monitor>,
    logical_monitors: Vec<LogicalMonitor>,
    properties: DisplayManagerProperties,
}

/// DBus Interface for providing bindings
pub struct DisplayServer {
    manager: Arc<Mutex<DisplayManager>>,
    sway_connection: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
pub struct DisplayManagerProperties {
    #[zvariant(rename = "layout-mode")]
    layout: Option<u32>,
    #[zvariant(rename = "supports-changing-layout-mode")]
    support_layout_change: Option<bool>,
    #[zvariant(rename = "global-scale-required")]
    global_scale: Option<bool>,
    #[zvariant(rename = "legacy-ui-scaling-factor")]
    legacy_scale_factor: Option<i32>,
}

#[dbus_interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayServer {
    pub async fn get_current_state(&mut self) -> DisplayManager {
        info!("Recieved 'GetCurrentState' request from control-center");
        let manager_ref = self.manager.lock().await;
        manager_ref.clone()
    }

    pub async fn apply_monitors_config(
        &mut self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<LogicalMonitor>,
        properties: DisplayManagerProperties,
    ) -> zbus::fdo::Result<()> {
        error!("Configuration Method: {method}");
        let mut manager_obj = self.manager.lock().await;
        if serial != manager_obj.serial {
            panic!("Wrong serial");
        }
        for monitor in &logical_monitors {
            monitor.apply(&self.sway_connection).await;
        }
        manager_obj
            .get_monitor_info(&self.sway_connection)
            .await
            .unwrap();
        manager_obj.properties = properties;
        Ok(())
    }

    #[dbus_interface(property)]
    pub async fn apply_monitors_config_allowed(&self) -> bool {
        error!("Call to apply_monitors_config");
        return true;
    }

    pub fn apply_configuration(&self) {
        error!("Applying config");
    }

    pub fn get_resources(&self) {
        error!("GetRresources");
    }
    pub fn change_backlight(&self) {
        error!("GetRresources");
    }

    #[dbus_interface(signal)]
    pub async fn monitors_changed(&self, ctxt: &SignalContext<'_>) -> zbus::Result<()>;
}

impl DisplayServer {
    pub async fn new(
        manager: Arc<Mutex<DisplayManager>>,
        sway_connection: Arc<Mutex<Connection>>,
    ) -> DisplayServer {
        DisplayServer {
            manager,
            sway_connection,
        }
    }
    pub async fn run_server(self) -> Result<zbus::Connection, Box<dyn Error>> {
        info!("Starting display daemon");
        self.manager
            .lock()
            .await
            .get_monitor_info(&self.sway_connection)
            .await?;
        let connection = ConnectionBuilder::session()?
            .name("org.gnome.Mutter.DisplayConfig")?
            .serve_at("/org/gnome/Mutter/DisplayConfig", self)?
            .build()
            .await?;
        Ok(connection)
    }
}
impl DisplayManager {
    pub async fn new() -> DisplayManager {
        DisplayManager {
            serial: 0,
            monitors: Vec::new(),
            logical_monitors: Vec::new(),
            properties: DisplayManagerProperties::new(),
        }
    }
    /// Watch for monitor changes.
    pub async fn watch_changes(
        manager_obj: Arc<Mutex<DisplayManager>>,
        connection: &zbus::Connection,
        sway_connection: Arc<Mutex<Connection>>,
    ) -> Result<(), Box<dyn Error>> {
        let prefix = "card0-";
        let get_status = |path: &PathBuf| {
            let mut status = String::from("Off");
            let status_path = path.join("dpms");
            if status_path.exists() {
                File::open(status_path)
                    .unwrap()
                    .read_to_string(&mut status)
                    .unwrap();
            }
            status
        };
        let get_outputs = || -> Vec<_> {
            fs::read_dir("/sys/class/drm/")
                .unwrap()
                .map(|r| r.unwrap())
                .filter(|item| item.file_name().to_str().unwrap().starts_with(prefix))
                .map(|item| item.path())
                .map(|path| {
                    let status = get_status(&path);
                    (path, status)
                })
                .collect()
        };
        info!("Watching Monitor Changes...");
        let mut outputs;
        loop {
            let manager_obj_ref = Arc::clone(&manager_obj);
            outputs = get_outputs();
            thread::sleep(Duration::from_secs(1));
            for output in &*outputs {
                let curr_status = get_status(&output.0);
                if curr_status != output.1 {
                    info!("Displays changed...");
                    info!("Display Status: {:?}", outputs);
                    if let Err(e) = manager_obj_ref
                        .lock()
                        .await
                        .get_monitor_info(&sway_connection)
                        .await
                    {
                        error!("{e}");
                    };
                    info!("Emiting MonitorsChanged signal...");
                    connection
                        .emit_signal(
                            Some("org.gnome.Mutter.DisplayConfig"),
                            "/org/gnome/Mutter/DisplayConfig",
                            "org.gnome.Mutter.DisplayConfig",
                            "MonitorsChanged",
                            &(),
                        )
                        .await?;
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    async fn get_monitor_info<'a>(
        &mut self,
        sway_connection: &Mutex<Connection>,
    ) -> Result<(), Box<dyn Error>> {
        let outputs = sway_connection.lock().await.get_outputs().await?;
        self.monitors = outputs.iter().map(|o| Monitor::new(o)).collect();
        self.logical_monitors = outputs.iter().map(|o| LogicalMonitor::new(o)).collect();
        info!("monitors info: {:#?}", self.monitors);
        info!("logical monitors: {:#?}", self.logical_monitors);
        Ok(())
    }
}

impl DisplayManagerProperties {
    pub fn new() -> DisplayManagerProperties {
        DisplayManagerProperties {
            layout: Some(1),
            support_layout_change: Some(true),
            global_scale: Some(false),
            legacy_scale_factor: Some(1),
        }
    }
}
