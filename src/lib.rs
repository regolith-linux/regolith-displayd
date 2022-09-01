pub mod modes;
pub mod monitor;

use core::fmt;
use lazy_static::lazy_static;
use log::{debug, error, info};
use monitor::{LogicalMonitor, Monitor, MonitorApply};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::io::Write;
use std::{
    error::Error,
    fs::{self, File},
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
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
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

#[derive(Debug, Clone, SerializeDict, DeserializeDict, Type, PartialEq)]
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
            error!("Invalid configuration recieved for method apply_monitors_config: Wrong serial");
            return Err(zbus::fdo::Error::InvalidArgs(String::from("Wrong serial")));
        }
        let get_dpy_name = |mon: &MonitorApply| {
            let monitor = mon.search_monitor(&manager_obj.monitors).unwrap();
            monitor.get_dpy_name().replace(" ", "_")
        };

        let mut monitors_sorted = logical_monitors.clone();
        monitors_sorted.sort_by_key(get_dpy_name);
        let profile_name = monitors_sorted
            .iter()
            .map(get_dpy_name)
            .collect::<Vec<String>>()
            .join("__");
        info!("Profile FileName: {profile_name}");
        let env_vars: HashMap<String, String> = std::env::vars().collect();
        let home_dir = env_vars.get("HOME").expect("$HOME not defined");
        let kanshi_base_path: PathBuf = env_vars
            .get("KANSHI_PARTIALS")
            .unwrap_or(&format!("{home_dir}/.config/regolith2/kanshi/output/"))
            .into();

        fs::create_dir_all(&kanshi_base_path).unwrap();
        let mut profile_buf = File::options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(kanshi_base_path.join(&profile_name))
            .expect("Error while opening profile file for writing");

        writeln!(&mut profile_buf, "profile {{").unwrap();

        let mut active_mons = Vec::new();
        for logical_monitor in &logical_monitors {
            // If apply_monitors_config called with method == 0 (Verify configuration)
            if method == 0 {
                match logical_monitor.verify(&self.sway_connection, &manager_obj.monitors) {
                    Ok(_) => continue,
                    Err(e) => return Err(e),
                };
            }
            let monitor = logical_monitor
                .search_monitor(&manager_obj.monitors)
                .unwrap();
            active_mons.push(monitor);
            // logical_monitor.apply(&self.sway_connection, &monitor).await;
            logical_monitor.save_kanshi(&mut profile_buf, &monitor);
        }
        if method == 0 {
            return Ok(());
        }
        for disabled_mon in manager_obj.get_disabled_monitors(&active_mons) {
            writeln!(
                &profile_buf,
                "\toutput {} disable",
                disabled_mon.get_dpy_name()
            )
            .expect("Failed to write to file");
        }
        writeln!(&mut profile_buf, "}}").unwrap();
        manager_obj.properties = properties;
        if let Err(e) = profile_buf.sync_all() {
            error!("Error writing data to kanshi config file: {e}");
        }
        
        // reload kanshi config 
        match get_kanshi_pid() { // Get pid of current kanshi instance
            Ok(pid) => {
                info!("kanshi pid: {pid}");
                let mut kanshi_reload_cmd = Command::new("kill");
                kanshi_reload_cmd.args(["-s", "SIGHUP", pid.trim()]); // Send SIGHUP to kanshi
                if let Err(e) = kanshi_reload_cmd.spawn() {
                    error!("Failed to send signal to kanshi: {e}");
                }
            }
            Err(e) => error!("Failed to reload kanshi config: {e}"),
        }
        if let Err(e) = manager_obj.get_monitor_info(&self.sway_connection).await {
            error!("Error getting output information from sway: {e}");
        }
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

    pub async fn watch_changes(
        manager_obj: Arc<Mutex<DisplayManager>>,
        sway_connection: Arc<Mutex<Connection>>,
    )-> Result<(), Box<dyn Error>> {
        let mut prev_monitor_set = HashSet::new();
        let mut prev_logical_monitor_set = HashSet::new();
        loop {
            thread::sleep(Duration::from_millis(700));
            let mut manager_obj_lock = manager_obj.lock().await;
            let display_info = manager_obj_lock.get_monitor_info(&sway_connection).await.unwrap();
            let mut monitor_set = HashSet::new();
            let mut logical_monitor_set = HashSet::new();
            let mut monitors_changed = false;
            for monitor in &display_info.0 {
                if !prev_monitor_set.contains(monitor) {
                    monitors_changed = true;
                }
                monitor_set.insert(monitor.clone());
            }
            for logical_monitor in &display_info.1 {
                if !prev_logical_monitor_set.contains(logical_monitor) {
                    monitors_changed = true;
                }
                logical_monitor_set.insert(logical_monitor.clone());
            }
            if monitors_changed {
                prev_monitor_set = monitor_set;
                prev_logical_monitor_set = logical_monitor_set;
                manager_obj_lock.monitors = display_info.0.clone();
                manager_obj_lock.logical_monitors = display_info.1.clone();
                debug!("monitors info: {:#?}", manager_obj_lock.monitors);
                debug!("logical monitors: {:#?}", manager_obj_lock.logical_monitors);
                Self::emit_monitors_changed().await?;
            }
        }
    }

    /// Get list of all the monitors that are not active
    fn get_disabled_monitors(&self, active_mons: &Vec<&Monitor>) -> Vec<&Monitor> {
        self.monitors
            .iter()
            .filter(|mon| !active_mons.contains(mon))
            .collect()
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

    /// Returns list of all monitors and logical monitors
    pub async fn get_monitor_info<'a>(
        &mut self,
        sway_connection: &Mutex<Connection>,
    ) -> Result<(Vec<Monitor>, Vec<LogicalMonitor>), Box<dyn Error>> {
        let outputs = sway_connection.lock().await.get_outputs().await?;
        let monitors = outputs
            .iter()
            .filter(|o| o.active)
            .map(|o| Monitor::new(o))
            .collect();
        let logical_monitors = outputs.iter().map(|o| LogicalMonitor::new(o)).collect();
        Ok((monitors, logical_monitors))
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

/// Get the pid of the currently running kanshi instance
pub fn get_kanshi_pid() -> Result<String, Box<dyn Error>> {
    let pid_bytes = Command::new("pidof")
        .arg("kanshi")
        .output()?
        .stdout;
    Ok(String::from_utf8(pid_bytes)?)
}
