pub mod modes;
pub mod monitor;

use core::fmt;
use lazy_static::lazy_static;
use log::{debug, error, info};
use monitor::{LogicalMonitor, Monitor, MonitorApply};
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

lazy_static! {
    static ref ZBUS_CONNECTION: Arc<Mutex<Option<zbus::Connection>>> = Arc::new(Mutex::new(None));
}

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
    // TODO: Make independent of sway
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

#[derive(Debug)]
pub struct ServerError {
    description: String,
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
        logical_monitors: Vec<MonitorApply>,
        properties: DisplayManagerProperties,
    ) -> zbus::fdo::Result<()> {
        debug!("Configuration Method: {method}");
        let mut manager_obj = self.manager.lock().await;
        debug!("Serial: {} {}", manager_obj.serial, serial);
        if serial != manager_obj.serial {
            panic!("Wrong serial");
        }
        for monitor in &logical_monitors {
            if method == 0 {
                match monitor.verify(&self.sway_connection, &manager_obj.monitors) {
                    Ok(_) => continue,
                    Err(e) => return Err(e),
                };
            }
            monitor
                .apply(&self.sway_connection, &manager_obj.monitors)
                .await;
        }
        if method == 0 {
            return Ok(());
        }
        manager_obj
            .get_monitor_info(&self.sway_connection)
            .await
            .unwrap();
        manager_obj.properties = properties;
        DisplayManager::emit_monitors_changed().await?;
        Ok(())
    }

    #[dbus_interface(property)]
    pub async fn apply_monitors_config_allowed(&self) -> bool {
        info!("Call to apply_monitors_config");
        return true;
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
    pub async fn run_server(self) -> Result<(), Box<dyn Error>> {
        info!("Starting display daemon");
        self.manager
            .lock()
            .await
            .get_monitor_info(&self.sway_connection)
            .await?;

        let mut connection = ZBUS_CONNECTION.lock().await;
        *connection = Some(
            ConnectionBuilder::session()?
                .name("org.gnome.Mutter.DisplayConfig")?
                .serve_at("/org/gnome/Mutter/DisplayConfig", self)?
                .build()
                .await?,
        );
        Ok(())
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
        sway_connection: Arc<Mutex<Connection>>,
    ) -> Result<(), Box<dyn Error>> {
        let prefix = "card";
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
            let outputs = fs::read_dir("/sys/class/drm/")
                .unwrap()
                .map(|r| r.unwrap())
                .filter(|item| item.file_name().to_str().unwrap().starts_with(prefix))
                .map(|item| item.path())
                .map(|path| {
                    let status = get_status(&path);
                    (path, status)
                })
                .collect();
            outputs
        };
        info!("Watching Monitor Changes...");
        let mut outputs;
        loop {
            let manager_obj_ref = Arc::clone(&manager_obj);
            outputs = get_outputs();
            thread::sleep(Duration::from_millis(500));
            for output in &*outputs {
                let curr_status = get_status(&output.0);
                if curr_status != output.1 {
                    debug!("Change Detected");
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
                    Self::emit_monitors_changed().await?;
                    info!("Emiting MonitorsChanged signal...");
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub async fn emit_monitors_changed() -> zbus::Result<()> {
        let connection = ZBUS_CONNECTION.lock().await;
        info!("Emiting monitor changed");
        if let Some(con) = &*connection {
            con.emit_signal(
                Option::<&str>::None,
                "/org/gnome/Mutter/DisplayConfig",
                "org.gnome.Mutter.DisplayConfig",
                "MonitorsChanged",
                &(),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn get_monitor_info<'a>(
        &mut self,
        sway_connection: &Mutex<Connection>,
    ) -> Result<(), Box<dyn Error>> {
        let outputs = sway_connection.lock().await.get_outputs().await?;
        self.monitors = outputs.iter().map(|o| Monitor::new(o)).collect();
        self.logical_monitors = outputs
            .iter()
            .filter(|o| o.active)
            .map(|o| LogicalMonitor::new(o))
            .collect();
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

impl ServerError {
    fn _produce_error(err: &str) -> ServerError {
        ServerError {
            description: err.to_string(),
        }
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description)
    }
}

impl Error for ServerError {
    fn description(&self) -> &str {
        &self.description
    }
}
