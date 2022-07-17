pub mod modes;
pub mod monitor;

use log::{error, info};
use monitor::{LogicalMonitor, Monitor};
use std::{
    borrow::BorrowMut,
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
#[derive(Debug)]
pub struct DisplayManager {
    serial: u32,
    monitors: Vec<Monitor>,
    logical_monitors: Vec<LogicalMonitor>,
    properties: DisplayManagerProperties,
    sway_connection: Connection,
}

/// DBus Interface for providing bindings
pub struct DisplayServer {
    manager: Arc<Mutex<DisplayManager>>,
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
    #[dbus_interface(out_args("serial", "monitors", "logical_monitors", "properties"))]
    pub async fn get_current_state(
        &mut self,
    ) -> (
        u32,
        Vec<Monitor>,
        Vec<LogicalMonitor>,
        DisplayManagerProperties,
    ) {
        info!("Recieved 'GetCurrentState' request from control-center");
        let mut manager_ref = self.manager.borrow_mut().lock().await;
        let DisplayManager {
            serial,
            monitors,
            properties,
            logical_monitors,
            ..
        } = &*manager_ref;
        let response = (
            serial.clone(),
            monitors.clone(),
            logical_monitors.clone(),
            properties.clone(),
        );
        manager_ref.serial += 1;
        response
    }

    #[dbus_interface(signal)]
    pub async fn monitors_changed(&self, ctxt: &SignalContext<'_>) -> zbus::Result<()>;
}

impl DisplayServer {
    pub fn new(manager: Arc<Mutex<DisplayManager>>) -> DisplayServer {
        DisplayServer { manager }
    }
    pub async fn run_server(self) -> Result<zbus::Connection, Box<dyn Error>> {
        info!("Starting display daemon");
        self.manager.lock().await.get_monitor_info().await?;
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
        let sway_connection = Connection::new().await.expect("Unable to connect to sway ipc interface. Make sure sway is running and SWAYSOCK is set");
        DisplayManager {
            serial: 0,
            monitors: Vec::new(),
            logical_monitors: Vec::new(),
            properties: DisplayManagerProperties::new(),
            sway_connection,
        }
    }
    /// Watch for monitor changes.
    pub async fn watch_changes(
        manager_obj: Arc<Mutex<DisplayManager>>,
        connection: &zbus::Connection,
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
                    if let Err(e) = manager_obj_ref.lock().await.get_monitor_info().await {
                        error!("{e}");
                    };
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

    async fn get_monitor_info(&mut self) -> Result<(), Box<dyn Error>> {
        let outputs = self.sway_connection.get_outputs().await?;
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
