pub mod confirm;
pub mod edid;
pub mod gamma;
pub mod labels;
pub mod modes;
pub mod monitor;
pub mod night_light;

use core::fmt;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use monitor::{LogicalMonitor, Monitor, MonitorApply};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};
use swayipc_async::{Connection, Output};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tokio::time::Instant;
use zbus::{dbus_interface, ConnectionBuilder, SignalContext};
use zvariant::{DeserializeDict, SerializeDict, Type};

lazy_static! {
    static ref ZBUS_CONNECTION: Arc<Mutex<Option<zbus::Connection>>> = Arc::new(Mutex::new(None));
    /// The persistent change awaiting the user's answer, if any.
    static ref PENDING: Mutex<Option<Pending>> = Mutex::new(None);
}

/// How long the user has to keep a persistent change before it is reverted,
/// as in GNOME.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(20);

/// How long `ApplyMonitorsConfig` waits for the prompt to be on screen, with
/// the keyboard, before it replies anyway.
const PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(3);

/// Numbers the persistent changes, so that an answer to one that another
/// change has replaced is ignored.
static CONFIRMATIONS: AtomicU64 = AtomicU64::new(0);

/// A persistent change awaiting the user's answer. Dropping it cancels the
/// confirmation and closes its prompt.
struct Pending {
    id: u64,
    _cancel: oneshot::Sender<()>,
}

/// A persistent change to confirm, and what keeping or reverting it takes.
struct Confirmation {
    id: u64,
    manager: Arc<Mutex<DisplayManager>>,
    sway_connection: Arc<Mutex<Connection>>,
    /// The output to show the prompt on.
    output: String,
    /// When the change is reverted if the user hasn't answered.
    deadline: Instant,
    /// The sway commands that restore the configuration from before the change.
    previous: Vec<String>,
    /// The primary chosen by the last Apply, and the one from before the change.
    primary: Arc<Mutex<Option<PrimaryChoice>>>,
    previous_primary: Option<PrimaryChoice>,
    profile_name: String,
    profile_buf: Vec<u8>,
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
    /// The primary monitor chosen by the last Apply (see [`PrimaryChoice`]).
    /// A persistent change that is reverted brings the previous one back.
    primary: Arc<Mutex<Option<PrimaryChoice>>>,
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

pub struct KanshiPaths {
    profiles: PathBuf,
    config: PathBuf,
}

#[dbus_interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayServer {
    pub async fn get_current_state(&mut self) -> DisplayManager {
        info!("Recieved 'GetCurrentState' request from control-center");
        let mut state = self.manager.lock().await.clone();
        let remembered = self.primary.lock().await.clone();
        let chosen = match &remembered {
            Some((set, connector)) if *set == monitor_set(&state.monitors) => {
                Some(connector.clone())
            }
            _ => match get_kanshi_paths().await {
                Ok(paths) => state.saved_primary(&paths),
                Err(_) => None,
            },
        };
        state.mark_primary(chosen.as_deref());
        state
    }

    /// Verify (0), or apply at once, temporarily (1) or persistently (2).
    ///
    /// Neither method saves anything when it applies. A persistent change
    /// shows a prompt instead: kept, it is saved as a kanshi profile;
    /// reverted with Escape, or not answered within [`CONFIRM_TIMEOUT`], the
    /// configuration from just before it comes back. Any change replaces a
    /// persistent one still awaiting an answer, which then stays as it is,
    /// unsaved, as in GNOME.
    pub async fn apply_monitors_config(
        &mut self,
        serial: u32,
        method: u32,
        mutter_logical_monitors: Vec<MonitorApply>,
        properties: DisplayManagerProperties,
    ) -> zbus::fdo::Result<()> {
        let called = Instant::now();
        debug!("Configuration Method: {method}");
        let mut manager_obj = self.manager.lock().await;
        // Monitors plugged in or unplugged since the watcher last looked:
        // the configuration was made for other monitors, so refuse it rather
        // than save it as their profile. The watcher reports the change,
        // moving the serial on, and the panel reads the new state.
        match manager_obj.get_monitor_info(&self.sway_connection).await {
            Ok((monitors, _)) if !same_monitors(&manager_obj.monitors, &monitors) => {
                error!("Invalid configuration recieved for method apply_monitors_config: the monitors changed");
                return Err(zbus::fdo::Error::InvalidArgs(String::from(
                    "The monitors changed",
                )));
            }
            Ok(_) => {}
            Err(e) => error!("Error getting output information from sway: {e}"),
        }
        debug!("Serial: {} {}", manager_obj.serial, serial);
        if serial != manager_obj.serial {
            error!("Invalid configuration recieved for method apply_monitors_config: Wrong serial");
            return Err(zbus::fdo::Error::InvalidArgs(String::from("Wrong serial")));
        }
        if method > 2 {
            error!("Invalid configuration recieved for method apply_monitors_config: Invalid method {method}");
            return Err(zbus::fdo::Error::InvalidArgs(String::from(
                "Invalid method",
            )));
        }
        // Only the logical layout mode is implemented. The request's
        // properties are options for this request: they never replace the
        // properties GetCurrentState reports.
        if let Some(layout) = properties.layout {
            if layout != 1 {
                return Err(zbus::fdo::Error::InvalidArgs(String::from(
                    "Can't set layout mode",
                )));
            }
        }
        // Verify the whole configuration before changing anything: an Apply
        // that cannot be satisfied is rejected atomically, and a verify call
        // must leave no state behind, nor touch a change awaiting an answer.
        for mutter_logical_mointor in &mutter_logical_monitors {
            mutter_logical_mointor.verify(
                &self.sway_connection,
                &manager_obj.monitors,
                &manager_obj.logical_monitors,
            )?;
        }
        verify_layout(&mutter_logical_monitors, &manager_obj.monitors)?;
        if method == 0 {
            return Ok(());
        }

        // verify_layout made sure there is exactly one primary.
        let primary = mutter_logical_monitors
            .iter()
            .find(|logical| logical.is_primary())
            .map(|logical| logical.connector().to_string())
            .unwrap_or_default();

        let (profile_name, mut profile_buf) = manager_obj.kanshi_profile(&mutter_logical_monitors);
        if let Some(monitor) = manager_obj
            .monitors
            .iter()
            .find(|monitor| monitor.connector() == primary)
        {
            let line = format!(
                "{PRIMARY_OUTPUT} \"{}\"\n",
                monitor.kanshi_name(&manager_obj.monitors)
            );
            profile_buf.splice(0..0, line.into_bytes());
        }
        let commands = manager_obj.sway_commands(&mutter_logical_monitors);
        // What a revert goes back to: the outputs as they are now, which may
        // be a persistent change still awaiting an answer, as in GNOME.
        let previous = current_sway_commands(&self.sway_connection)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("Cannot read the outputs: {e}")))?;
        if let Err(e) = run_sway(&self.sway_connection, &commands).await {
            error!("Error applying the configuration: {e}");
            if let Err(e) = run_sway(&self.sway_connection, &previous).await {
                error!("Error restoring the configuration: {e}");
            }
            return Err(zbus::fdo::Error::Failed(e));
        }
        info!(
            "Applied configuration (method {method}): {}",
            commands.join("; ")
        );
        // This change replaces the one awaiting an answer, if any.
        if PENDING.lock().await.take().is_some() {
            info!("A newer configuration replaces the one awaiting confirmation");
        }
        match manager_obj.get_monitor_info(&self.sway_connection).await {
            Ok((monitors, logical_monitors)) => {
                manager_obj.update(monitors, logical_monitors);
            }
            Err(e) => error!("Error getting output information from sway: {e}"),
        }
        // The primary changes at once, like the layout.
        let previous_primary = self
            .primary
            .lock()
            .await
            .replace((monitor_set(&manager_obj.monitors), primary));
        let mut ready = None;
        if method == 2 {
            let id = CONFIRMATIONS.fetch_add(1, Ordering::Relaxed) + 1;
            let (cancel_tx, cancel_rx) = oneshot::channel();
            let (ready_tx, ready_rx) = oneshot::channel();
            *PENDING.lock().await = Some(Pending {
                id,
                _cancel: cancel_tx,
            });
            let confirmation = Confirmation {
                id,
                manager: Arc::clone(&self.manager),
                sway_connection: Arc::clone(&self.sway_connection),
                output: prompt_output(&self.sway_connection).await,
                deadline: called + CONFIRM_TIMEOUT,
                previous,
                primary: Arc::clone(&self.primary),
                previous_primary,
                profile_name,
                profile_buf,
            };
            tokio::spawn(confirmation.run(cancel_rx, ready_tx));
            ready = Some(ready_rx);
        }
        drop(manager_obj);
        DisplayManager::emit_monitors_changed().await?;
        // Reply once the prompt can take the answer: the caller may press
        // Enter as soon as the call returns.
        if let Some(ready) = ready {
            if tokio::time::timeout(PROMPT_READY_TIMEOUT, ready)
                .await
                .is_err()
            {
                warn!("The confirmation prompt is not on screen yet");
            }
        }
        Ok(())
    }

    #[dbus_interface(property)]
    pub async fn apply_monitors_config_allowed(&self) -> bool {
        info!("Call to apply_monitors_config");
        true
    }

    /// Night Light is applied by this daemon (see `night_light`).
    #[dbus_interface(property)]
    pub async fn night_light_supported(&self) -> bool {
        true
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
            primary: Arc::new(Mutex::new(None)),
        }
    }
    pub async fn run_server(self) -> Result<(), Box<dyn Error>> {
        info!("Starting display daemon");
        // kanshi starts after this daemon: make sure it can load the profiles,
        // and only those that fit the monitors connected now.
        let kanshi_paths = get_kanshi_paths().await?;
        set_aside_invalid_profiles(&kanshi_paths);
        let (monitors, _) = self
            .manager
            .lock()
            .await
            .get_monitor_info(&self.sway_connection)
            .await?;
        hold_profiles_for_other_monitors(&kanshi_paths, &connected_outputs(&monitors));

        let mut connection = ZBUS_CONNECTION.lock().await;
        *connection = Some(
            ConnectionBuilder::session()?
                .name("org.gnome.Mutter.DisplayConfig")?
                .serve_at("/org/gnome/Mutter/DisplayConfig", self)?
                // The Displays panel's monitor labels, which regolith-powerd
                // passes on from org.gnome.Shell.
                .serve_at(labels::PATH, labels::MonitorLabels::new())?
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

    /// Follow sway's outputs: every 700 ms, store them if they changed (see
    /// [`DisplayManager::update`]), tell the Displays panel, and hold back or
    /// restore the profiles kept to particular monitors.
    pub async fn watch_changes(
        manager_obj: Arc<Mutex<DisplayManager>>,
        sway_connection: Arc<Mutex<Connection>>,
    ) -> Result<(), Box<dyn Error>> {
        loop {
            let mut manager_obj_lock = manager_obj.lock().await;
            let display_info = manager_obj_lock
                .get_monitor_info(&sway_connection)
                .await
                .unwrap();
            // A monitor that went away is a change too.
            let changed = manager_obj_lock.update(display_info.0.clone(), display_info.1.clone());
            if changed {
                debug!("monitors info: {:#?}", manager_obj_lock.monitors);
                debug!("logical monitors: {:#?}", manager_obj_lock.logical_monitors);
            }
            drop(manager_obj_lock);
            if changed {
                Self::emit_monitors_changed().await?;
                let kanshi_paths = get_kanshi_paths().await?;
                if hold_profiles_for_other_monitors(
                    &kanshi_paths,
                    &connected_outputs(&display_info.0),
                ) {
                    if let Err(e) = reload_kanshi().await {
                        error!("Error reloading kanshi configuration: {e}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    }

    /// The kanshi profile that applies `config` to the connected monitors:
    /// its file name and its contents.
    ///
    /// Each monitor is named by its description, or by its connector name if
    /// another connected monitor has the same description (see
    /// [`Monitor::kanshi_name`]). The file name joins those names, sorted, so
    /// the same set of monitors always maps to the same file.
    ///
    /// A connector name says nothing about the monitor plugged into it, so a
    /// profile that uses one also records which monitor it was saved for, in
    /// a comment kanshi ignores, and its file name includes that monitor.
    /// [`hold_profiles_for_other_monitors`] keeps such a profile away from
    /// kanshi while other monitors are on those connectors. Its file name
    /// starts with "zz-", so that kanshi, which applies the first profile
    /// that matches in file name order, prefers a profile saved for the
    /// monitors by their descriptions.
    fn kanshi_profile(&self, config: &[MonitorApply]) -> (String, Vec<u8>) {
        let (profile_name, pinned) = self.kanshi_profile_name();

        let mut profile_buf = Vec::new();
        let mut active_physical_monitors = Vec::new();

        for (connector, description) in &pinned {
            writeln!(
                &mut profile_buf,
                "{PINNED_OUTPUT} \"{connector}\" is \"{description}\""
            )
            .unwrap();
        }
        writeln!(&mut profile_buf, "profile {{").unwrap();
        for mutter_logical_mointor in config {
            let Some(sway_physical_monitor) = mutter_logical_mointor.search_monitor(&self.monitors)
            else {
                continue;
            };

            active_physical_monitors.push(sway_physical_monitor.clone());
            mutter_logical_mointor.save_kanshi(
                &mut profile_buf,
                sway_physical_monitor,
                &self.monitors,
            );
        }

        for disabled_mon in self.get_disabled_monitors(&active_physical_monitors) {
            writeln!(
                &mut profile_buf,
                "\toutput \"{}\" disable",
                disabled_mon.kanshi_name(&self.monitors)
            )
            .expect("Failed to write to file");
        }
        writeln!(&mut profile_buf, "}}").unwrap();
        (profile_name, profile_buf)
    }

    /// The file name of the kanshi profile for the connected monitors, and
    /// the outputs it names by connector, with the monitor saved for each.
    fn kanshi_profile_name(&self) -> (String, Vec<(String, String)>) {
        let mut names = Vec::new();
        let mut pinned = Vec::new();
        for monitor in &self.monitors {
            let name = monitor.kanshi_name(&self.monitors);
            let description = monitor.get_dpy_name();
            if name == description {
                names.push(description);
            } else {
                names.push(format!("{name}={description}"));
                pinned.push((name, description));
            }
        }
        names.sort();
        pinned.sort();
        let mut profile_name = names
            .iter()
            .map(|name| name.replace(' ', "_"))
            .collect::<Vec<String>>()
            .join("__");
        if !pinned.is_empty() {
            profile_name.insert_str(0, "zz-");
        }
        (profile_name, pinned)
    }

    /// The primary monitor saved with the profile of the connected monitors,
    /// by connector.
    fn saved_primary(&self, paths: &KanshiPaths) -> Option<String> {
        let (profile_name, _) = self.kanshi_profile_name();
        let profile = fs::read_to_string(paths.profiles.join(profile_name)).ok()?;
        let name = profile.lines().find_map(|line| {
            let rest = line.trim().strip_prefix(PRIMARY_OUTPUT)?.trim();
            rest.strip_prefix('"')?
                .strip_suffix('"')
                .map(str::to_string)
        })?;
        self.monitors
            .iter()
            .find(|monitor| monitor.kanshi_name(&self.monitors) == name)
            .map(|monitor| monitor.connector().to_string())
    }

    /// Mark exactly one logical monitor as primary, as Mutter always does:
    /// the `chosen` monitor if it is on, else the built-in panel, else the
    /// monitor with the largest mode (the first of them on a tie).
    fn mark_primary(&mut self, chosen: Option<&str>) {
        let active: Vec<&Monitor> = self
            .monitors
            .iter()
            .filter(|monitor| {
                self.logical_monitors
                    .iter()
                    .any(|logical| logical.connector() == monitor.connector())
            })
            .collect();
        let primary = chosen
            .and_then(|chosen| active.iter().find(|monitor| monitor.connector() == chosen))
            .or_else(|| active.iter().find(|monitor| monitor.is_builtin()))
            .or_else(|| {
                active
                    .iter()
                    .fold(None, |best: Option<&&Monitor>, monitor| match best {
                        Some(b) if b.current_area() >= monitor.current_area() => Some(b),
                        _ => Some(monitor),
                    })
            })
            .map(|monitor| monitor.connector().to_string());
        for logical in &mut self.logical_monitors {
            let is_primary = Some(logical.connector()) == primary.as_deref();
            logical.set_primary(is_primary);
        }
    }

    /// The sway commands that apply `config`: the outputs it names are
    /// enabled, and the others disabled after them, so that some output stays
    /// on all along.
    fn sway_commands(&self, config: &[MonitorApply]) -> Vec<String> {
        let mut commands = Vec::new();
        let mut active = Vec::new();
        for mutter_logical_mointor in config {
            let Some(monitor) = mutter_logical_mointor.search_monitor(&self.monitors) else {
                continue;
            };
            if let Some(command) = mutter_logical_mointor.sway_command(monitor) {
                commands.push(command);
                active.push(monitor.clone());
            }
        }
        for disabled in self.get_disabled_monitors(&active) {
            commands.push(format!("output \"{}\" disable", disabled.connector()));
        }
        commands
    }

    /// Get list of all the monitors that are not active
    fn get_disabled_monitors(&self, active_physical_monitors: &[Monitor]) -> Vec<&Monitor> {
        self.monitors
            .iter()
            .filter(|mon| {
                !active_physical_monitors
                    .iter()
                    .any(|active| active.same_monitor(mon))
            })
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
    pub async fn get_monitor_info(
        &mut self,
        sway_connection: &Mutex<Connection>,
    ) -> Result<(Vec<Monitor>, Vec<LogicalMonitor>), Box<dyn Error>> {
        let outputs = sway_connection.lock().await.get_outputs().await?;
        let (mut monitors, logical_monitors) = Self::monitors_of(&outputs);
        for monitor in &mut monitors {
            monitor.keep_known_modes(&self.monitors);
        }
        Ok((monitors, logical_monitors))
    }

    /// The monitors of sway's `outputs`, and the logical monitors of the
    /// active ones.
    fn monitors_of(outputs: &[Output]) -> (Vec<Monitor>, Vec<LogicalMonitor>) {
        let monitors = outputs.iter().map(Monitor::new).collect();
        let logical_monitors = outputs
            .iter()
            .filter(|o| o.active)
            .map(LogicalMonitor::new)
            .collect();
        (monitors, logical_monitors)
    }

    /// Store `monitors` and `logical_monitors` if they differ from the current
    /// state, as sets: a monitor that was added, removed or changed mode, or a
    /// logical monitor that was added, removed, moved, scaled or rotated, or
    /// that now shows another monitor. Returns whether they did.
    ///
    /// When monitors were plugged in or unplugged, the serial also moves on,
    /// so a configuration composed for the previous monitors is refused
    /// instead of being saved as their profile. (Mutter moves it on every
    /// change; a new layout or mode keeps it here, so a client may still
    /// apply several layouts in a row with one serial.)
    fn update(&mut self, monitors: Vec<Monitor>, logical_monitors: Vec<LogicalMonitor>) -> bool {
        if same_set(&self.monitors, &monitors)
            && same_set(&self.logical_monitors, &logical_monitors)
        {
            return false;
        }
        if !same_monitors(&self.monitors, &monitors) {
            self.serial = self.serial.wrapping_add(1);
        }
        self.monitors = monitors;
        self.logical_monitors = logical_monitors;
        true
    }
}

/// Whether `a` and `b` hold the same elements, in any order.
fn same_set<T: Eq + Hash>(a: &[T], b: &[T]) -> bool {
    a.iter().collect::<HashSet<_>>() == b.iter().collect::<HashSet<_>>()
}

/// Whether `a` and `b` are the same monitors (connector, vendor, product and
/// serial), whatever their modes.
fn same_monitors(a: &[Monitor], b: &[Monitor]) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|monitor| b.iter().any(|other| other.same_monitor(monitor)))
}

impl Default for DisplayManagerProperties {
    fn default() -> Self {
        Self::new()
    }
}

impl DisplayManagerProperties {
    pub fn new() -> DisplayManagerProperties {
        DisplayManagerProperties {
            layout: Some(1),
            // Only the logical layout mode is implemented.
            support_layout_change: None,
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

pub async fn get_kanshi_paths() -> zbus::Result<KanshiPaths> {
    let env_vars: HashMap<String, String> = std::env::vars().collect();
    let home_dir = env_vars.get("HOME").expect("$HOME not defined");
    let default_path = format!("{home_dir}/.config/regolith3/kanshi");
    let base: PathBuf = match trawlcat::rescat("kanshi.path", Some(default_path.clone())).await {
        Ok(path) => path.into(),
        Err(e) => {
            warn!("Error: {e}");
            default_path.into()
        }
    };
    let profiles = base.join("profiles");
    let config = base.join("config");
    Ok(KanshiPaths { profiles, config })
}

/// The output that a profile in this kanshi configuration names twice, if any.
///
/// kanshi 1.7 and later reject the whole configuration when a profile names
/// the same output twice ("duplicate output"), so no profile applies at all.
/// This daemon wrote such profiles for identical monitors before it told them
/// apart by connector name.
fn duplicate_output(config: &str) -> Option<String> {
    let mut seen = HashSet::new();
    for line in config.lines() {
        let line = line.trim();
        // The start or the end of a profile.
        if line.starts_with("profile") || line.starts_with('}') {
            seen.clear();
            continue;
        }
        let Some(rest) = line.strip_prefix("output") else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        let name = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => rest[1..].split(quote).next().unwrap_or_default(),
            _ => rest.split_whitespace().next().unwrap_or_default(),
        };
        if !seen.insert(name) {
            return Some(name.to_string());
        }
    }
    None
}

/// Move the profiles that kanshi would reject out of the profiles directory,
/// which the kanshi configuration includes as a whole, to "invalid-profiles"
/// next to it. One such file keeps kanshi from applying any profile.
fn set_aside_invalid_profiles(paths: &KanshiPaths) {
    let Ok(entries) = fs::read_dir(&paths.profiles) else {
        return;
    };
    let aside = paths.profiles.with_file_name("invalid-profiles");
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(profile) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(output) = duplicate_output(&profile) else {
            continue;
        };
        let target = aside.join(entry.file_name());
        match fs::create_dir_all(&aside).and_then(|_| fs::rename(&path, &target)) {
            Ok(()) => warn!(
                "kanshi profile {} names output \"{output}\" twice, which kanshi rejects: moved it to {}",
                path.display(),
                target.display()
            ),
            Err(e) => error!(
                "kanshi profile {} names output \"{output}\" twice, which kanshi rejects, and could not be moved: {e}",
                path.display()
            ),
        }
    }
}

/// The comment that records, in a kanshi profile, which monitor an output
/// named by its connector was saved for.
const PINNED_OUTPUT: &str = "# regolith-displayd: output";

/// A primary monitor chosen by an Apply: the connectors of the monitors it was
/// made for, and the connector of the primary.
type PrimaryChoice = (Vec<String>, String);

/// The comment that records, in a kanshi profile, which output is primary.
const PRIMARY_OUTPUT: &str = "# regolith-displayd: primary";

/// The connectors of `monitors`, sorted: which monitors a choice was made for.
fn monitor_set(monitors: &[Monitor]) -> Vec<String> {
    let mut set: Vec<String> = monitors.iter().map(|m| m.connector().to_string()).collect();
    set.sort();
    set
}

/// Refuse a configuration Mutter refuses
/// (`meta_verify_logical_monitor_config_list`), or that sway cannot show:
/// no logical monitor, a monitor used twice, a negative position, overlapping
/// logical monitors, logical monitors not all connected by shared edges, a
/// layout that doesn't start at (0, 0), or not exactly one primary.
/// Each logical monitor has already passed [`MonitorApply::verify`].
fn verify_layout(config: &[MonitorApply], monitors: &[Monitor]) -> zbus::fdo::Result<()> {
    let invalid = |message: &str| Err(zbus::fdo::Error::InvalidArgs(message.to_string()));
    if config.is_empty() {
        return invalid("Monitors config incomplete");
    }
    let mut seen = HashSet::new();
    for logical in config {
        if !seen.insert(logical.connector()) {
            return invalid("A monitor is assigned twice");
        }
    }
    let rects: Vec<(i32, i32, i32, i32)> = config
        .iter()
        .map(|logical| {
            let (x, y) = logical.position();
            let (w, h) = logical.logical_size(monitors);
            (x, y, x + w, y + h)
        })
        .collect();
    if rects.iter().any(|r| r.0 < 0 || r.1 < 0) {
        return invalid("Invalid logical monitor position");
    }
    for (i, a) in rects.iter().enumerate() {
        for b in &rects[i + 1..] {
            if a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3 {
                return invalid("Logical monitors overlap");
            }
        }
    }
    // Adjacent: sharing a stretch of an edge, not just a corner
    // (mtk_rectangle_is_adjacent_to).
    let adjacent = |a: &(i32, i32, i32, i32), b: &(i32, i32, i32, i32)| {
        ((a.0 == b.2 || a.2 == b.0) && !(a.3 <= b.1 || a.1 >= b.3))
            || ((a.1 == b.3 || a.3 == b.1) && !(a.2 <= b.0 || a.0 >= b.2))
    };
    let mut reached = vec![false; rects.len()];
    let mut todo = vec![0];
    reached[0] = true;
    while let Some(i) = todo.pop() {
        for j in 0..rects.len() {
            if !reached[j] && adjacent(&rects[i], &rects[j]) {
                reached[j] = true;
                todo.push(j);
            }
        }
    }
    if reached.iter().any(|r| !r) {
        return invalid("Logical monitors not adjacent");
    }
    if rects.iter().map(|r| r.0).min() != Some(0) || rects.iter().map(|r| r.1).min() != Some(0) {
        return invalid("Logical monitors positions are offset");
    }
    match config.iter().filter(|logical| logical.is_primary()).count() {
        0 => invalid("Config is missing primary logical"),
        1 => Ok(()),
        _ => invalid("Config contains multiple primary logical monitors"),
    }
}

/// The connectors a profile names, with the description of the monitor it was
/// saved for on each (see [`DisplayManager::kanshi_profile`]).
fn pinned_outputs(profile: &str) -> Vec<(String, String)> {
    profile
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(PINNED_OUTPUT)?.trim();
            let (connector, description) = rest.split_once("\" is \"")?;
            let connector = connector.strip_prefix('"')?;
            let description = description.strip_suffix('"')?;
            Some((connector.to_string(), description.to_string()))
        })
        .collect()
}

/// The description of the monitor on each connector.
fn connected_outputs(monitors: &[Monitor]) -> HashMap<String, String> {
    monitors
        .iter()
        .map(|monitor| (monitor.connector().to_string(), monitor.get_dpy_name()))
        .collect()
}

/// Keep the profiles that name outputs by connector where kanshi reads them
/// only while the monitors they were saved for are on those connectors.
///
/// kanshi matches an output by its connector name or by its description,
/// never both, so a profile for identical monitors would also apply to other
/// monitors plugged into the same connectors. Such profiles move to
/// "profiles-on-hold" next to the profiles directory while any of their
/// connectors is empty or has another monitor, and back when the monitors
/// they were saved for are there again. Returns whether a profile moved.
fn hold_profiles_for_other_monitors(
    paths: &KanshiPaths,
    connected: &HashMap<String, String>,
) -> bool {
    let held = paths.profiles.with_file_name("profiles-on-hold");
    let fits = |profile: &str| {
        pinned_outputs(profile)
            .iter()
            .all(|(connector, description)| connected.get(connector) == Some(description))
    };
    let mut moved = false;
    for entry in fs::read_dir(&paths.profiles)
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        let Ok(profile) = fs::read_to_string(&path) else {
            continue;
        };
        if pinned_outputs(&profile).is_empty() || fits(&profile) {
            continue;
        }
        let target = held.join(entry.file_name());
        match fs::create_dir_all(&held).and_then(|_| fs::rename(&path, &target)) {
            Ok(()) => {
                info!(
                    "Other monitors are connected: holding kanshi profile {}",
                    path.display()
                );
                moved = true;
            }
            Err(e) => error!("Cannot hold kanshi profile {}: {e}", path.display()),
        }
    }
    for entry in fs::read_dir(&held).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(profile) = fs::read_to_string(&path) else {
            continue;
        };
        if !fits(&profile) {
            continue;
        }
        let target = paths.profiles.join(entry.file_name());
        // A profile saved since for the same monitors replaces the held one.
        let result = if target.exists() {
            fs::remove_file(&path)
        } else {
            fs::rename(&path, &target)
        };
        match result {
            Ok(()) => {
                info!(
                    "The monitors of kanshi profile {} are connected again",
                    target.display()
                );
                moved = true;
            }
            Err(e) => error!("Cannot restore kanshi profile {}: {e}", path.display()),
        }
    }
    moved
}

/// How [`reload_kanshi`] got kanshi to apply the saved profiles.
#[derive(Debug, PartialEq, Eq)]
pub enum KanshiReload {
    /// A running kanshi was told to re-read its configuration (SIGHUP).
    Signalled,
    /// regolith-init-kanshi.service was started: no kanshi was running, or
    /// the running one could not reload and was stopped.
    StartedService,
    /// The service could not be started either, so kanshi was spawned.
    Spawned,
}

const KANSHI_SERVICE: &str = "regolith-init-kanshi.service";

/// Make kanshi re-read its configuration and apply the matching profile.
///
/// kanshi reloads its config and re-applies the matching profile on SIGHUP, so the
/// running instance (normally regolith-init-kanshi.service) is signalled instead of
/// being killed and replaced. Killing it raced with the replacement: `killall` ran
/// asynchronously and could also kill the new kanshi, sometimes before it applied
/// anything, and neither process was reaped. A kanshi too old to reload on SIGHUP
/// is stopped and started again instead.
pub async fn reload_kanshi() -> zbus::Result<()> {
    let KanshiPaths { config, .. } = get_kanshi_paths().await?;
    let how = reload_or_start_kanshi(&config)?;
    info!("kanshi reload: {how:?}");
    Ok(())
}

/// Pids of live (non-zombie) `kanshi` processes owned by `uid`.
///
/// A zombie is a dead process awaiting reaping, not a running kanshi, but
/// `pkill --exact` matches zombies too. Orphaned zombies from an earlier
/// daemon (reparented to a container init that never reaps) would fool
/// every later Apply into "reloading" a dead process, so the
/// reload-or-start decision is based on live processes only.
fn live_kanshi_pids(uid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.parse::<u32>().is_err() {
            continue;
        }
        // The owner of /proc/<pid> is the process's real uid.
        if fs::metadata(entry.path()).map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if comm.trim_end() != "kanshi" {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // pid (comm) state ppid ... — comm may itself contain parens or
        // spaces, so split after the LAST ')'.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        if stat[close + 1..].split_whitespace().next() == Some("Z") {
            continue;
        }
        if let Ok(pid) = name.parse::<u32>() {
            pids.push(pid);
        }
    }
    pids
}

/// SIGHUP in the signal masks of /proc/<pid>/status: signal n is bit n - 1,
/// and SIGHUP is signal 1.
const SIGHUP_MASK: u64 = 1;

/// Whether a process with this /proc/<pid>/status catches SIGHUP.
fn catches_sighup(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigCgt:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_some_and(|mask| mask & SIGHUP_MASK != 0)
}

/// Whether kanshi `pid` reloads its configuration on SIGHUP.
///
/// kanshi handles SIGHUP since 1.3. Older versions (1.2.0 on Ubuntu jammy)
/// keep the default action and exit, so they must be restarted instead.
fn reloads_on_sighup(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| catches_sighup(&status))
}

/// Stop the kanshi processes `pids` and wait until none of them is alive.
///
/// Each one is reaped by its parent: the service manager, or the thread
/// that waits for a kanshi spawned by this daemon.
fn stop_kanshi(uid: u32, pids: &[u32]) {
    let alive = || -> Vec<String> {
        live_kanshi_pids(uid)
            .into_iter()
            .filter(|pid| pids.contains(pid))
            .map(|pid| pid.to_string())
            .collect()
    };
    for signal in ["-TERM", "-KILL"] {
        let targets = alive();
        if targets.is_empty() {
            return;
        }
        let _ = Command::new("kill").arg(signal).args(&targets).status();
        for _ in 0..50 {
            if alive().is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    warn!("kanshi {:?} did not stop", alive());
}

fn reload_or_start_kanshi(config: &Path) -> std::io::Result<KanshiReload> {
    let uid = fs::metadata("/proc/self")?.uid();
    let live = live_kanshi_pids(uid);
    if !live.is_empty() {
        if live.iter().all(|&pid| reloads_on_sighup(pid)) {
            // A kanshi is running: ask it to reload. pkill also signals any
            // zombie matches, but signalling a zombie is a harmless no-op;
            // what matters is that a live instance was told to reload.
            let _ = Command::new("pkill")
                .args([
                    "--signal",
                    "HUP",
                    "--exact",
                    "--uid",
                    &uid.to_string(),
                    "kanshi",
                ])
                .status();
            return Ok(KanshiReload::Signalled);
        }
        // SIGHUP would kill this kanshi without applying anything. Stop it
        // and start it again: a new kanshi applies the profile on startup.
        info!("kanshi {live:?} does not reload on SIGHUP, restarting it");
        stop_kanshi(uid, &live);
    }

    let started = Command::new("systemctl")
        .args(["--user", "start", KANSHI_SERVICE])
        .status()
        .is_ok_and(|status| status.success());
    if started {
        return Ok(KanshiReload::StartedService);
    }

    let mut child = Command::new("kanshi").arg("-c").arg(config).spawn()?;
    // Reap kanshi when it exits so that it does not linger as a zombie.
    thread::spawn(move || child.wait());
    Ok(KanshiReload::Spawned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;

    /// Children of this process that are zombies named `name`.
    fn zombie_children(name: &str) -> usize {
        let me = std::process::id().to_string();
        fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| fs::read_to_string(entry.ok()?.path().join("stat")).ok())
            .filter(|stat| {
                // pid (comm) state ppid ...
                let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
                    return false;
                };
                let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
                &stat[open + 1..close] == name
                    && rest.first() == Some(&"Z")
                    && rest.get(1) == Some(&me.as_str())
            })
            .count()
    }

    /// Wait until `pid` is listed as a live kanshi. spawn() returns once the
    /// child has started exec, but the kernel gives it its new name a moment
    /// later; until then it is not found by name.
    fn wait_until_live(uid: u32, pid: u32) {
        for _ in 0..100 {
            if live_kanshi_pids(uid).contains(&pid) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("kanshi {pid} was not seen running");
    }

    fn fake_tool(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(
            &path,
            format!("#!/bin/sh\necho \"{name} $*\" >> \"$FAKE_LOG\"\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn monitor(connector: &str, make: &str, model: &str, serial: &str) -> Monitor {
        let output = serde_json::from_value(serde_json::json!({
            "id": 1, "name": connector, "make": make, "model": model, "serial": serial,
            "active": true, "dpms": true, "primary": false, "scale": 1.0, "transform": "normal",
            "modes": [{"width": 1920, "height": 1080, "refresh": 60000}],
            "current_mode": {"width": 1920, "height": 1080, "refresh": 60000},
            "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}
        }))
        .unwrap();
        Monitor::new(&output)
    }

    /// `connector` at `x`, the way the Displays panel applies it.
    fn place(connector: &str, x: i32) -> MonitorApply {
        serde_json::from_value(serde_json::json!({
            "x_pos": x, "y_pos": 0, "scale": 1.0, "transform": 0, "primary": false,
            "monitors": [[connector, "1920x1080@60Hz", {}]]
        }))
        .unwrap()
    }

    fn profile(monitors: Vec<Monitor>, config: &[MonitorApply]) -> (String, String) {
        let manager = DisplayManager {
            serial: 0,
            monitors,
            logical_monitors: Vec::new(),
            properties: DisplayManagerProperties::new(),
        };
        let (name, contents) = manager.kanshi_profile(config);
        (name, String::from_utf8(contents).unwrap())
    }

    fn acme(connector: &str) -> Monitor {
        monitor(connector, "Acme", "View 24", "0")
    }

    fn laptop_panel() -> Monitor {
        monitor("eDP-1", "BOE", "0x0BCA", "Unknown")
    }

    #[test]
    fn identical_monitors_are_named_by_connector() {
        let (name, contents) = profile(
            vec![acme("DP-1"), acme("DP-2")],
            &[place("DP-1", 0), place("DP-2", 1920)],
        );
        assert_eq!(name, "zz-DP-1=Acme_View_24_0__DP-2=Acme_View_24_0");
        assert_eq!(
            contents,
            "# regolith-displayd: output \"DP-1\" is \"Acme View 24 0\"\n\
             # regolith-displayd: output \"DP-2\" is \"Acme View 24 0\"\n\
             profile {\n\
             \toutput \"DP-1\" mode 1920x1080@60Hz position 0,0 transform normal scale 1 enable\n\
             \toutput \"DP-2\" mode 1920x1080@60Hz position 1920,0 transform normal scale 1 enable\n\
             }\n"
        );
        assert_eq!(duplicate_output(&contents), None);
    }

    #[test]
    fn only_identical_monitors_are_named_by_connector() {
        let monitors = vec![acme("DP-2"), laptop_panel(), acme("DP-1")];
        let (name, contents) = profile(monitors, &[place("eDP-1", 0), place("DP-1", 1920)]);
        assert_eq!(
            name,
            "zz-BOE_0x0BCA_Unknown__DP-1=Acme_View_24_0__DP-2=Acme_View_24_0"
        );
        assert_eq!(
            contents,
            "# regolith-displayd: output \"DP-1\" is \"Acme View 24 0\"\n\
             # regolith-displayd: output \"DP-2\" is \"Acme View 24 0\"\n\
             profile {\n\
             \toutput \"BOE 0x0BCA Unknown\" mode 1920x1080@60Hz position 0,0 transform normal scale 1 enable\n\
             \toutput \"DP-1\" mode 1920x1080@60Hz position 1920,0 transform normal scale 1 enable\n\
             \toutput \"DP-2\" disable\n\
             }\n"
        );
    }

    #[test]
    fn distinct_monitors_are_named_by_description() {
        // The same file name and contents as before identical monitors were
        // told apart, so existing profiles are overwritten in place.
        let monitors = vec![
            monitor("DP-1", "LG", "27GL850", "007NTAB1C123"),
            monitor("HDMI-A-1", "Dell Inc.", "DELL U2415", "7MT0167G1V0L"),
            laptop_panel(),
        ];
        let (name, contents) = profile(monitors, &[place("HDMI-A-1", 0), place("DP-1", 1920)]);
        assert_eq!(
            name,
            "BOE_0x0BCA_Unknown__Dell_Inc._DELL_U2415_7MT0167G1V0L__LG_27GL850_007NTAB1C123"
        );
        assert_eq!(
            contents,
            "profile {\n\
             \toutput \"Dell Inc. DELL U2415 7MT0167G1V0L\" mode 1920x1080@60Hz position 0,0 transform normal scale 1 enable\n\
             \toutput \"LG 27GL850 007NTAB1C123\" mode 1920x1080@60Hz position 1920,0 transform normal scale 1 enable\n\
             \toutput \"BOE 0x0BCA Unknown\" disable\n\
             }\n"
        );
    }

    /// A profile for two identical monitors as written before they were told
    /// apart: kanshi 1.7 and later reject it, and with it every profile.
    const OLD_IDENTICAL_PROFILE: &str = "profile {\n\
        \toutput \"Acme View 24 0\" mode 1920x1080@60Hz position 0,0 transform normal scale 1 enable\n\
        \toutput \"Acme View 24 0\" mode 1920x1080@60Hz position 1920,0 transform normal scale 1 enable\n\
        }\n";

    #[test]
    fn duplicate_outputs_are_found_per_profile() {
        assert_eq!(
            duplicate_output(OLD_IDENTICAL_PROFILE).as_deref(),
            Some("Acme View 24 0")
        );
        assert_eq!(
            duplicate_output("profile {\n  output DP-1 enable\n  output DP-1 disable\n}\n")
                .as_deref(),
            Some("DP-1")
        );
        // The same output in two profiles, and the dummy profile.
        let two_profiles = "profile a {\n\toutput \"Acme View 24 0\" enable\n}\n\
                            profile b {\n\toutput \"Acme View 24 0\" disable\n}\n";
        assert_eq!(duplicate_output(two_profiles), None);
        assert_eq!(duplicate_output("profile {}\n"), None);
    }

    #[test]
    fn profiles_kanshi_rejects_are_set_aside() {
        let dir =
            std::env::temp_dir().join(format!("displayd-profiles-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let paths = KanshiPaths {
            profiles: dir.join("profiles"),
            config: dir.join("config"),
        };
        fs::create_dir_all(&paths.profiles).unwrap();
        let (name, contents) = profile(
            vec![acme("DP-1"), acme("DP-2")],
            &[place("DP-1", 0), place("DP-2", 1920)],
        );
        fs::write(paths.profiles.join(name), contents).unwrap();
        fs::write(
            paths.profiles.join("Acme_View_24_0__Acme_View_24_0"),
            OLD_IDENTICAL_PROFILE,
        )
        .unwrap();
        fs::write(paths.profiles.join("dummy"), "profile {}\n").unwrap();

        set_aside_invalid_profiles(&paths);

        let mut kept: Vec<String> = fs::read_dir(&paths.profiles)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        kept.sort();
        assert_eq!(
            kept,
            ["dummy", "zz-DP-1=Acme_View_24_0__DP-2=Acme_View_24_0"]
        );
        assert_eq!(
            fs::read_to_string(dir.join("invalid-profiles/Acme_View_24_0__Acme_View_24_0"))
                .unwrap(),
            OLD_IDENTICAL_PROFILE
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn profiles_for_other_monitors_are_held() {
        let dir = std::env::temp_dir().join(format!("displayd-hold-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let paths = KanshiPaths {
            profiles: dir.join("profiles"),
            config: dir.join("config"),
        };
        let held = dir.join("profiles-on-hold");
        fs::create_dir_all(&paths.profiles).unwrap();
        let pair = vec![laptop_panel(), acme("DP-1"), acme("DP-2")];
        let (identical, contents) = profile(
            pair.clone(),
            &[place("eDP-1", 0), place("DP-1", 1920), place("DP-2", 3840)],
        );
        assert_eq!(
            pinned_outputs(&contents),
            [
                ("DP-1".to_string(), "Acme View 24 0".to_string()),
                ("DP-2".to_string(), "Acme View 24 0".to_string())
            ]
        );
        fs::write(paths.profiles.join(&identical), &contents).unwrap();
        let other = vec![
            laptop_panel(),
            acme("DP-1"),
            monitor("DP-2", "Dell Inc.", "DELL U2415", "7MT0167G1V0L"),
        ];
        let (distinct, distinct_contents) = profile(other.clone(), &[place("eDP-1", 0)]);
        assert!(pinned_outputs(&distinct_contents).is_empty());
        fs::write(paths.profiles.join(&distinct), &distinct_contents).unwrap();
        fs::write(paths.profiles.join("dummy"), "profile {}\n").unwrap();

        // The monitors the profile was saved for: nothing moves.
        assert!(!hold_profiles_for_other_monitors(
            &paths,
            &connected_outputs(&pair)
        ));
        assert_eq!(file_names(&held), Vec::<String>::new());

        // One of the identical monitors is unplugged: the profile is held,
        // so it cannot apply to whatever is plugged into DP-2 next.
        let unplugged = vec![laptop_panel(), acme("DP-1")];
        assert!(hold_profiles_for_other_monitors(
            &paths,
            &connected_outputs(&unplugged)
        ));
        assert_eq!(file_names(&paths.profiles), [distinct.as_str(), "dummy"]);
        assert_eq!(file_names(&held), [identical.as_str()]);

        // Another monitor on DP-2: still held; the other profiles stay.
        assert!(!hold_profiles_for_other_monitors(
            &paths,
            &connected_outputs(&other)
        ));
        assert_eq!(file_names(&paths.profiles), [distinct.as_str(), "dummy"]);

        // The identical monitors are back: so is their profile.
        assert!(hold_profiles_for_other_monitors(
            &paths,
            &connected_outputs(&pair)
        ));
        assert_eq!(
            file_names(&paths.profiles),
            [distinct.as_str(), "dummy", identical.as_str()]
        );
        assert_eq!(file_names(&held), Vec::<String>::new());
        assert_eq!(
            fs::read_to_string(paths.profiles.join(&identical)).unwrap(),
            contents
        );

        // A profile saved since for the same monitors wins over a held copy.
        assert!(hold_profiles_for_other_monitors(
            &paths,
            &connected_outputs(&other)
        ));
        fs::write(paths.profiles.join(&identical), "# newer\n").unwrap();
        assert!(hold_profiles_for_other_monitors(
            &paths,
            &connected_outputs(&pair)
        ));
        assert_eq!(file_names(&held), Vec::<String>::new());
        assert_eq!(
            fs::read_to_string(paths.profiles.join(&identical)).unwrap(),
            "# newer\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Run kanshi on `config` without a compositor. kanshi parses its
    /// configuration first, so it only gets to "failed to connect to display"
    /// when the configuration is valid.
    fn kanshi_parse(kanshi: &Path, dir: &Path, config: &Path) -> String {
        // Through a link with another name, so the reload test does not take
        // it for a running kanshi.
        let link = dir.join("kanshi-parse");
        if !link.exists() {
            std::os::unix::fs::symlink(kanshi, &link).unwrap();
        }
        let output = Command::new(&link)
            .arg("-c")
            .arg(config)
            .env("XDG_RUNTIME_DIR", dir)
            .env("WAYLAND_DISPLAY", "no-compositor")
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    #[test]
    fn kanshi_accepts_profiles_for_identical_monitors() {
        let kanshi = Path::new("/usr/bin/kanshi");
        if !kanshi.exists() {
            eprintln!("skipping, kanshi is not installed");
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("displayd-kanshi-parse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let profiles = dir.join("profiles");
        fs::create_dir_all(&profiles).unwrap();
        let config = dir.join("config");
        fs::write(&config, format!("include {}/*\n", profiles.display())).unwrap();
        let configs = [
            (
                vec![acme("DP-1"), acme("DP-2")],
                vec![place("DP-1", 0), place("DP-2", 1920)],
            ),
            (
                vec![laptop_panel(), acme("DP-1"), acme("DP-2")],
                vec![place("eDP-1", 0), place("DP-2", 1920)],
            ),
            (
                vec![
                    laptop_panel(),
                    monitor("DP-1", "LG", "27GL850", "007NTAB1C123"),
                ],
                vec![place("DP-1", 0)],
            ),
        ];
        for (monitors, config) in configs {
            let (name, contents) = profile(monitors, &config);
            fs::write(profiles.join(name), contents).unwrap();
        }
        let stderr = kanshi_parse(kanshi, &dir, &config);
        assert!(
            stderr.contains("failed to connect to display"),
            "kanshi rejected the profiles: {stderr}"
        );

        // The old profile, for comparison: kanshi 1.7 and later reject it.
        fs::write(
            profiles.join("Acme_View_24_0__Acme_View_24_0"),
            OLD_IDENTICAL_PROFILE,
        )
        .unwrap();
        let stderr = kanshi_parse(kanshi, &dir, &config);
        eprintln!("kanshi on the old profile: {}", stderr.trim());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sighup_is_caught_only_when_set_in_sigcgt() {
        // kanshi 1.5.1 / 1.9: catches SIGHUP, SIGINT, SIGQUIT and SIGTERM.
        let reloads = "Name:\tkanshi\nSigBlk:\t0000000000000000\nSigIgn:\t0000000080000000\nSigCgt:\t0000000000004007\n";
        // kanshi 1.2.0: default action for SIGHUP; ignored SIGHUP does not reload either.
        let exits = "Name:\tkanshi\nSigBlk:\t0000000000000000\nSigIgn:\t0000000080000007\nSigCgt:\t0000000000000000\n";
        assert!(catches_sighup(reloads));
        assert!(!catches_sighup(exits));
        assert!(!catches_sighup("Name:\tkanshi\n"));
    }

    // One test, because it changes PATH for the whole test process.
    #[test]
    fn reload_signals_running_kanshi_and_falls_back_to_starting_it() {
        let dir = std::env::temp_dir().join(format!("displayd-kanshi-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        fake_tool(&dir, "pkill", "exit 0");
        fake_tool(&dir, "systemctl", "exit ${FAKE_SYSTEMCTL_RC:-0}");
        fake_tool(&dir, "kanshi", "touch \"$FAKE_LOG.kanshi-ran\"");
        fake_tool(&dir, "killall", "exit 0");
        std::env::set_var("FAKE_LOG", &log);
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", dir.display()));
        let config = dir.join("config");
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let calls = || fs::read_to_string(&log).unwrap_or_default();

        // Live kanshis: executables whose comm is exactly "kanshi". They
        // live outside PATH so the spawn fallback below still resolves the
        // fake script. One catches SIGHUP like kanshi 1.3 and later; the
        // other keeps the default action like kanshi 1.2. Both are shell
        // scripts, since the kernel takes comm from the executed file name.
        // A copy of a system binary is not usable: a multicall binary such
        // as uutils coreutils dispatches on its name and exits at once.
        // Both wait in the shell's built-in read on a pipe from the test
        // and never fork: a child is also named "kanshi" until it execs,
        // and it would neither catch SIGHUP nor be stopped with its parent.
        let reloading_bin = dir.join("reloading").join("kanshi");
        let exiting_bin = dir.join("exiting").join("kanshi");
        fs::create_dir_all(reloading_bin.parent().unwrap()).unwrap();
        fs::create_dir_all(exiting_bin.parent().unwrap()).unwrap();
        fs::write(
            &reloading_bin,
            "#!/bin/sh\ntrap : HUP\nwhile :; do read -r _; done\n",
        )
        .unwrap();
        fs::set_permissions(&reloading_bin, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&exiting_bin, "#!/bin/sh\nread -r _\n").unwrap();
        fs::set_permissions(&exiting_bin, fs::Permissions::from_mode(0o755)).unwrap();
        // Foreign live kanshis (e.g. a developer session running the real
        // thing) make the other scenarios untestable, and must never be
        // stopped by this test.
        let foreign = live_kanshi_pids(uid);
        if !foreign.iter().all(|&pid| reloads_on_sighup(pid)) {
            eprintln!("skipping, foreign kanshi that does not reload on SIGHUP: {foreign:?}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }

        // A kanshi that reloads is running: it is asked to reload, nothing
        // is killed or started.
        let mut reloading = Command::new(&reloading_bin)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if reloads_on_sighup(reloading.id()) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(live_kanshi_pids(uid).contains(&reloading.id()));
        assert!(reloads_on_sighup(reloading.id()));
        assert_eq!(
            reload_or_start_kanshi(&config).unwrap(),
            KanshiReload::Signalled
        );
        assert_eq!(
            calls(),
            format!("pkill --signal HUP --exact --uid {uid} kanshi\n")
        );
        assert!(
            reloading.try_wait().unwrap().is_none(),
            "the reloading kanshi was stopped"
        );
        reloading.kill().unwrap();
        reloading.wait().unwrap();
        assert!(!live_kanshi_pids(uid).contains(&reloading.id()));

        if !foreign.is_empty() {
            eprintln!("skipping the other scenarios, foreign kanshi running: {foreign:?}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }

        // A kanshi that would die on SIGHUP is running: it is stopped, not
        // signalled, and started again through the systemd unit.
        fs::remove_file(&log).unwrap();
        std::env::set_var("FAKE_SYSTEMCTL_RC", "0");
        let mut exiting = Command::new(&exiting_bin)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        wait_until_live(uid, exiting.id());
        assert!(!reloads_on_sighup(exiting.id()));
        assert_eq!(
            reload_or_start_kanshi(&config).unwrap(),
            KanshiReload::StartedService
        );
        assert_eq!(
            calls(),
            "systemctl --user start regolith-init-kanshi.service\n"
        );
        assert_eq!(
            exiting.wait().unwrap().signal(),
            Some(15),
            "the old kanshi was not stopped with SIGTERM"
        );

        // The same without a systemd user session: stopped, then spawned.
        fs::remove_file(&log).unwrap();
        std::env::set_var("FAKE_SYSTEMCTL_RC", "1");
        let mut exiting = Command::new(&exiting_bin)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        wait_until_live(uid, exiting.id());
        assert_eq!(
            reload_or_start_kanshi(&config).unwrap(),
            KanshiReload::Spawned
        );
        assert_eq!(
            exiting.wait().unwrap().signal(),
            Some(15),
            "the old kanshi was not stopped with SIGTERM"
        );
        assert!(calls().starts_with("systemctl --user start regolith-init-kanshi.service\n"));
        assert!(!calls().contains("pkill"));
        let ran = dir.join("log.kanshi-ran");
        for _ in 0..100 {
            if ran.exists() && zombie_children("kanshi") == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ran.exists(), "kanshi was not spawned again");
        assert_eq!(
            zombie_children("kanshi"),
            0,
            "the spawned kanshi was not reaped"
        );
        let _ = fs::remove_file(&ran);

        // No kanshi is running: the systemd unit is started. pkill is not
        // even attempted: there is nobody to signal.
        fs::remove_file(&log).unwrap();
        std::env::set_var("FAKE_SYSTEMCTL_RC", "0");
        assert_eq!(
            reload_or_start_kanshi(&config).unwrap(),
            KanshiReload::StartedService
        );
        assert_eq!(
            calls(),
            "systemctl --user start regolith-init-kanshi.service\n"
        );

        // No kanshi and no systemd user session: kanshi is spawned and reaped.
        fs::remove_file(&log).unwrap();
        std::env::set_var("FAKE_SYSTEMCTL_RC", "1");
        assert_eq!(
            reload_or_start_kanshi(&config).unwrap(),
            KanshiReload::Spawned
        );
        for _ in 0..100 {
            if ran.exists() && zombie_children("kanshi") == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ran.exists(), "kanshi was not spawned");
        assert!(calls().ends_with(&format!("kanshi -c {}\n", config.display())));
        assert_eq!(
            zombie_children("kanshi"),
            0,
            "the spawned kanshi was not reaped"
        );
        assert!(!calls().contains("killall"));

        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod hotplug_tests {
    use super::*;

    /// A sway output named `connector`, active at `x` with `scale`, in mode
    /// `width`x1080.
    fn sway_output(connector: &str, x: i32, scale: f64, width: i32) -> Output {
        serde_json::from_value(serde_json::json!({
            "id": 1, "name": connector, "make": "Acme", "model": connector, "serial": "0",
            "active": true, "dpms": true, "primary": false, "scale": scale, "transform": "normal",
            "modes": [
                {"width": 1920, "height": 1080, "refresh": 60000},
                {"width": 1280, "height": 1080, "refresh": 60000}
            ],
            "current_mode": {"width": width, "height": 1080, "refresh": 60000},
            "rect": {"x": x, "y": 0, "width": width, "height": 1080}
        }))
        .unwrap()
    }

    fn state(outputs: &[Output]) -> (Vec<Monitor>, Vec<LogicalMonitor>) {
        DisplayManager::monitors_of(outputs)
    }

    /// A manager that has reported `outputs`, at serial 1.
    fn reported(outputs: &[Output]) -> DisplayManager {
        let mut manager = DisplayManager {
            serial: 0,
            monitors: Vec::new(),
            logical_monitors: Vec::new(),
            properties: DisplayManagerProperties::new(),
        };
        let (monitors, logical_monitors) = state(outputs);
        assert!(manager.update(monitors, logical_monitors));
        assert_eq!(manager.serial, 1);
        manager
    }

    /// The models of the monitors, which [`sway_output`] names after the
    /// connector.
    fn connectors(manager: &DisplayManager) -> Vec<String> {
        manager
            .monitors
            .iter()
            .map(|m| m.get_dpy_name().split(' ').nth(1).unwrap().to_string())
            .collect()
    }

    fn dp1() -> Output {
        sway_output("DP-1", 0, 1.0, 1920)
    }

    fn dp2() -> Output {
        sway_output("DP-2", 1920, 1.0, 1920)
    }

    #[test]
    fn an_unplugged_monitor_is_noticed() {
        let mut manager = reported(&[dp1(), dp2()]);
        let (monitors, logical_monitors) = state(&[dp1()]);
        assert!(manager.update(monitors, logical_monitors));
        assert_eq!(connectors(&manager), ["DP-1"]);
        assert_eq!(manager.logical_monitors.len(), 1);
        assert_eq!(manager.serial, 2);
    }

    #[test]
    fn a_plugged_in_monitor_is_noticed() {
        let mut manager = reported(&[dp1()]);
        let (monitors, logical_monitors) = state(&[dp1(), dp2()]);
        assert!(manager.update(monitors, logical_monitors));
        assert_eq!(connectors(&manager), ["DP-1", "DP-2"]);
        assert_eq!(manager.serial, 2);
    }

    #[test]
    fn an_unplugged_disabled_monitor_is_noticed() {
        // A disabled output has a monitor but no logical monitor, so only the
        // monitors tell its removal apart.
        let disabled_dp2 = || {
            let mut output = dp2();
            output.active = false;
            output
        };
        let mut manager = reported(&[dp1(), disabled_dp2()]);
        assert_eq!(manager.logical_monitors.len(), 1);
        let (monitors, logical_monitors) = state(&[dp1(), disabled_dp2()]);
        assert_eq!(logical_monitors.len(), 1);
        assert!(!manager.update(monitors, logical_monitors));
        let (monitors, logical_monitors) = state(&[dp1()]);
        assert!(manager.update(monitors, logical_monitors));
        assert_eq!(connectors(&manager), ["DP-1"]);
        assert_eq!(manager.serial, 2);
    }

    #[test]
    fn the_same_outputs_in_another_order_are_no_change() {
        let mut manager = reported(&[dp1(), dp2()]);
        let (monitors, logical_monitors) = state(&[dp2(), dp1()]);
        assert!(!manager.update(monitors, logical_monitors));
        assert_eq!(manager.serial, 1);
    }

    #[test]
    fn a_new_layout_or_mode_is_a_change() {
        let mut manager = reported(&[dp1(), dp2()]);
        for changed in [
            sway_output("DP-2", 0, 1.0, 1920),    // moved
            sway_output("DP-2", 1920, 2.0, 1920), // scaled
            sway_output("DP-2", 1920, 2.0, 1280), // another mode
        ] {
            let (monitors, logical_monitors) = state(&[dp1(), changed]);
            assert!(manager.update(monitors, logical_monitors));
            // The same monitors: a configuration for them is still valid.
            assert_eq!(manager.serial, 1);
        }
    }

    #[test]
    fn a_mode_change_is_always_noticed() {
        // Each HashSet hashes with new random keys. A monitor in another mode
        // that compared equal but hashed differently was missed whenever the
        // two hashes happened to match closely enough (about 1 in 128).
        for _ in 0..1000 {
            let mut manager = reported(&[dp1()]);
            let (monitors, logical_monitors) = state(&[sway_output("DP-1", 0, 1.0, 1280)]);
            assert!(manager.update(monitors, logical_monitors));
            assert_eq!(manager.serial, 1);
        }
    }

    #[test]
    fn outputs_that_swap_places_are_a_change() {
        let mut manager = reported(&[dp1(), dp2()]);
        let (monitors, logical_monitors) = state(&[
            sway_output("DP-1", 1920, 1.0, 1920),
            sway_output("DP-2", 0, 1.0, 1920),
        ]);
        assert!(manager.update(monitors, logical_monitors));
        assert_eq!(manager.serial, 1);
        let (monitors, logical_monitors) = state(&[
            sway_output("DP-1", 1920, 1.0, 1920),
            sway_output("DP-2", 0, 1.0, 1920),
        ]);
        assert!(!manager.update(monitors, logical_monitors));
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// Two 1920x1080 monitors, DP-1 and DP-2.
    fn monitors() -> Vec<Monitor> {
        let outputs: Vec<Output> = ["DP-1", "DP-2"]
            .iter()
            .map(|connector| {
                serde_json::from_value(serde_json::json!({
                    "id": 1, "name": connector, "make": "Acme", "model": connector,
                    "serial": "0", "active": true, "dpms": true, "primary": false,
                    "scale": 1.0, "transform": "normal",
                    "modes": [{"width": 1920, "height": 1080, "refresh": 60000}],
                    "current_mode": {"width": 1920, "height": 1080, "refresh": 60000},
                    "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}
                }))
                .unwrap()
            })
            .collect();
        DisplayManager::monitors_of(&outputs).0
    }

    /// A logical monitor at (`x`, `y`) showing `connectors` at 1920x1080.
    fn logical(x: i32, y: i32, scale: f64, transform: u32, connectors: &[&str]) -> MonitorApply {
        let monitors: Vec<_> = connectors
            .iter()
            .map(|c| serde_json::json!([c, "1920x1080@60Hz", {}]))
            .collect();
        serde_json::from_value(serde_json::json!({
            "x_pos": x, "y_pos": y, "scale": scale, "transform": transform,
            "primary": true, "monitors": monitors
        }))
        .unwrap()
    }

    /// `logical`, primary or not.
    fn primary(logical: &MonitorApply, primary: bool) -> MonitorApply {
        let mut value = serde_json::to_value(logical).unwrap();
        value["primary"] = serde_json::json!(primary);
        serde_json::from_value(value).unwrap()
    }

    /// Check `config` with its first logical monitor primary.
    fn check(config: &[MonitorApply]) -> Result<(), String> {
        let config: Vec<MonitorApply> = config
            .iter()
            .enumerate()
            .map(|(i, logical)| primary(logical, i == 0))
            .collect();
        verify_layout(&config, &monitors()).map_err(|e| e.to_string())
    }

    #[test]
    fn valid_layouts_are_accepted() {
        check(&[logical(0, 0, 1.0, 0, &["DP-1"])]).unwrap();
        check(&[
            logical(0, 0, 1.0, 0, &["DP-1"]),
            logical(1920, 0, 1.0, 0, &["DP-2"]),
        ])
        .unwrap();
        // Stacked, with only part of an edge shared.
        check(&[
            logical(0, 0, 1.0, 0, &["DP-1"]),
            logical(960, 1080, 1.0, 0, &["DP-2"]),
        ])
        .unwrap();
        // Scaled and rotated: 960x540 and 1080x1920 logical pixels.
        check(&[
            logical(0, 0, 2.0, 0, &["DP-1"]),
            logical(960, 0, 1.0, 1, &["DP-2"]),
        ])
        .unwrap();
    }

    #[test]
    fn invalid_layouts_are_refused() {
        let refused = |config: &[MonitorApply], why: &str| {
            let error = check(config).unwrap_err();
            assert!(error.contains(why), "{error} does not say {why}");
        };
        refused(&[], "incomplete");
        refused(
            &[
                logical(0, 0, 1.0, 0, &["DP-1"]),
                logical(1920, 0, 1.0, 0, &["DP-1"]),
            ],
            "twice",
        );
        refused(
            &[
                logical(-1920, 0, 1.0, 0, &["DP-1"]),
                logical(0, 0, 1.0, 0, &["DP-2"]),
            ],
            "position",
        );
        refused(
            &[
                logical(0, 0, 1.0, 0, &["DP-1"]),
                logical(1000, 0, 1.0, 0, &["DP-2"]),
            ],
            "overlap",
        );
        refused(
            &[
                logical(0, 0, 1.0, 0, &["DP-1"]),
                logical(2000, 0, 1.0, 0, &["DP-2"]),
            ],
            "adjacent",
        );
        // Touching only at a corner.
        refused(
            &[
                logical(0, 0, 1.0, 0, &["DP-1"]),
                logical(1920, 1080, 1.0, 0, &["DP-2"]),
            ],
            "adjacent",
        );
        refused(&[logical(100, 0, 1.0, 0, &["DP-1"])], "offset");
    }

    /// The primary GetCurrentState reports for these sway outputs: the
    /// connector, mode width and connector type of each.
    fn reported_primary(outputs: &[(&str, i32)], chosen: Option<&str>) -> String {
        let outputs: Vec<Output> = outputs
            .iter()
            .map(|(connector, width)| {
                serde_json::from_value(serde_json::json!({
                    "id": 1, "name": connector, "make": "Acme", "model": connector,
                    "serial": "0", "active": true, "dpms": true, "primary": false,
                    "scale": 1.0, "transform": "normal",
                    "modes": [{"width": width, "height": 1080, "refresh": 60000}],
                    "current_mode": {"width": width, "height": 1080, "refresh": 60000},
                    "rect": {"x": 0, "y": 0, "width": width, "height": 1080}
                }))
                .unwrap()
            })
            .collect();
        let (monitors, logical_monitors) = DisplayManager::monitors_of(&outputs);
        let mut manager = DisplayManager {
            serial: 0,
            monitors,
            logical_monitors,
            properties: DisplayManagerProperties::new(),
        };
        manager.mark_primary(chosen);
        let primaries: Vec<&str> = manager
            .logical_monitors
            .iter()
            .filter(|logical| serde_json::to_value(logical).unwrap()["primary"] == true)
            .map(|logical| logical.connector())
            .collect();
        assert_eq!(primaries.len(), 1, "{primaries:?}");
        primaries[0].to_string()
    }

    #[test]
    fn one_primary_is_always_reported() {
        let outputs = [("DP-1", 1920), ("eDP-1", 1280), ("DP-2", 2560)];
        assert_eq!(reported_primary(&outputs, Some("DP-1")), "DP-1");
        // No choice, or a monitor that isn't there: the laptop panel.
        assert_eq!(reported_primary(&outputs, None), "eDP-1");
        assert_eq!(reported_primary(&outputs, Some("HDMI-1")), "eDP-1");
        // No laptop panel: the largest mode, the first of them on a tie.
        assert_eq!(
            reported_primary(&[("DP-1", 1920), ("DP-2", 2560)], None),
            "DP-2"
        );
        assert_eq!(
            reported_primary(&[("DP-1", 1920), ("DP-2", 1920)], None),
            "DP-1"
        );
    }

    #[test]
    fn exactly_one_primary() {
        let pair = |first: bool, second: bool| {
            let config = [
                primary(&logical(0, 0, 1.0, 0, &["DP-1"]), first),
                primary(&logical(1920, 0, 1.0, 0, &["DP-2"]), second),
            ];
            verify_layout(&config, &monitors()).map_err(|e| e.to_string())
        };
        pair(false, true).unwrap();
        assert!(pair(false, false).unwrap_err().contains("missing primary"));
        assert!(pair(true, true).unwrap_err().contains("multiple primary"));
    }
}

impl Confirmation {
    /// Show the prompt and wait for the answer, the timeout or a newer
    /// change. Keep saves the change; Escape or the timeout reverts it.
    /// `ready` is sent once the prompt is on screen with the keyboard, or
    /// dropped if it can't be shown, in which case only the timeout applies.
    async fn run(self, mut cancel: oneshot::Receiver<()>, ready: oneshot::Sender<()>) {
        let mut ready = Some(ready);
        let mut prompt = spawn_prompt(&self.output, self.deadline);
        let mut lines = prompt
            .as_mut()
            .and_then(|child| child.stdout.take())
            .map(|stdout| BufReader::new(stdout).lines());
        let keep = loop {
            let line = async {
                match lines.as_mut() {
                    Some(lines) => lines.next_line().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                line = line => match line {
                    Ok(Some(line)) if line.trim() == "ready" => {
                        if let Some(ready) = ready.take() {
                            let _ = ready.send(());
                        }
                    }
                    Ok(Some(line)) => match confirm::Answer::from_line(&line) {
                        Some(answer) => break answer == confirm::Answer::Keep,
                        None => warn!("Unexpected output from the confirmation prompt: {line}"),
                    },
                    // The prompt is gone without an answer.
                    Ok(None) | Err(_) => {
                        lines = None;
                        ready = None;
                    }
                },
                _ = tokio::time::sleep_until(self.deadline) => {
                    info!("The configuration was not confirmed in time");
                    break false;
                }
                // A newer change replaced this one: dropping the prompt closes it.
                _ = &mut cancel => return,
            }
        };
        if let Some(mut child) = prompt {
            let _ = child.kill().await;
        }
        let mut manager_obj = self.manager.lock().await;
        {
            let mut pending = PENDING.lock().await;
            if pending.as_ref().map(|p| p.id) != Some(self.id) {
                return;
            }
            *pending = None;
        }
        if keep {
            info!("Configuration kept: saving profile {}", self.profile_name);
            if let Err(e) = save_profile(&manager_obj, &self.profile_name, &self.profile_buf).await
            {
                error!("Error saving the kept configuration: {e}");
            }
        } else {
            info!("Configuration not kept: reverting");
            if let Err(e) = run_sway(&self.sway_connection, &self.previous).await {
                error!("Error reverting the configuration: {e}");
            }
            *self.primary.lock().await = self.previous_primary;
            match manager_obj.get_monitor_info(&self.sway_connection).await {
                Ok((monitors, logical_monitors)) => {
                    manager_obj.update(monitors, logical_monitors);
                }
                Err(e) => error!("Error getting output information from sway: {e}"),
            }
        }
        drop(manager_obj);
        if let Err(e) = DisplayManager::emit_monitors_changed().await {
            error!("Error emitting MonitorsChanged: {e}");
        }
    }
}

/// Start the prompt (see [`confirm`]) on `output`, counting down to
/// `deadline`.
fn spawn_prompt(output: &str, deadline: Instant) -> Option<tokio::process::Child> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            error!("Cannot show the confirmation prompt: {e}");
            return None;
        }
    };
    let left = deadline.saturating_duration_since(Instant::now());
    match tokio::process::Command::new(exe)
        .arg(confirm::PROMPT_ARG)
        .arg(output)
        .arg(left.as_millis().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => Some(child),
        Err(e) => {
            error!("Cannot show the confirmation prompt: {e}");
            None
        }
    }
}

/// The sway commands that bring the outputs back to how they are now.
async fn current_sway_commands(sway: &Mutex<Connection>) -> Result<Vec<String>, Box<dyn Error>> {
    let outputs = sway.lock().await.get_outputs().await?;
    Ok(restore_commands(&outputs))
}

/// The sway commands that bring `outputs` back: the active ones first, with
/// their mode, position, scale and transform as sway reports them, then the
/// others disabled.
fn restore_commands(outputs: &[Output]) -> Vec<String> {
    let mut enable = Vec::new();
    let mut disable = Vec::new();
    for output in outputs {
        if !output.active {
            disable.push(format!("output \"{}\" disable", output.name));
            continue;
        }
        let mut command = format!("output \"{}\" enable", output.name);
        if let Some(mode) = &output.current_mode {
            if mode.refresh > 0 {
                command.push_str(&format!(
                    " mode {}x{}@{}Hz",
                    mode.width,
                    mode.height,
                    mode.refresh as f64 / 1000.0
                ));
            } else {
                command.push_str(&format!(" mode {}x{}", mode.width, mode.height));
            }
        }
        command.push_str(&format!(
            " position {} {} scale {} transform {}",
            output.rect.x,
            output.rect.y,
            output.scale.unwrap_or(1.0),
            output.transform.as_deref().unwrap_or("normal")
        ));
        enable.push(command);
    }
    enable.extend(disable);
    enable
}

/// Run `commands` in sway, in order, as one IPC request.
async fn run_sway(sway: &Mutex<Connection>, commands: &[String]) -> Result<(), String> {
    if commands.is_empty() {
        return Ok(());
    }
    let joined = commands.join("; ");
    debug!("sway: {joined}");
    let results = sway
        .lock()
        .await
        .run_command(&joined)
        .await
        .map_err(|e| e.to_string())?;
    for result in results {
        result.map_err(|e| format!("{e} ({joined})"))?;
    }
    Ok(())
}

/// The output to show the prompt on: the focused one if it is on, else the
/// first output that is on.
async fn prompt_output(sway: &Mutex<Connection>) -> String {
    let outputs = match sway.lock().await.get_outputs().await {
        Ok(outputs) => outputs,
        Err(e) => {
            error!("Error getting outputs from sway: {e}");
            return String::new();
        }
    };
    let active = || outputs.iter().filter(|o| o.active);
    active()
        .find(|o| o.focused)
        .or_else(|| active().next())
        .map(|o| o.name.clone())
        .unwrap_or_default()
}

/// Save a kept configuration as kanshi profile `profile_name`, and have
/// kanshi load it.
async fn save_profile(
    manager_obj: &DisplayManager,
    profile_name: &str,
    profile_buf: &[u8],
) -> zbus::fdo::Result<()> {
    let kanshi_paths = get_kanshi_paths().await?;
    fs::create_dir_all(&kanshi_paths.profiles)
        .and_then(|_| fs::write(kanshi_paths.profiles.join(profile_name), profile_buf))
        .map_err(|e| zbus::fdo::Error::IOError(e.to_string()))?;
    // An older profile for the same identical monitors names one output
    // twice and would still keep kanshi from loading any profile.
    set_aside_invalid_profiles(&kanshi_paths);
    hold_profiles_for_other_monitors(&kanshi_paths, &connected_outputs(&manager_obj.monitors));
    if let Err(e) = reload_kanshi().await {
        error!("Error reloading kanshi configuration: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod confirm_tests {
    use super::*;

    fn outputs() -> Vec<Output> {
        serde_json::from_str(
            r#"[{"id": 3, "name": "HEADLESS-1", "make": "Unknown", "model": "Unknown",
                 "serial": "Unknown", "active": true, "dpms": true, "primary": false,
                 "scale": 2.0, "transform": "90", "modes": [],
                 "current_mode": {"width": 1024, "height": 768, "refresh": 60000},
                 "rect": {"x": 80, "y": 120, "width": 384, "height": 512}},
                {"id": 4, "name": "HEADLESS-2", "make": "Unknown", "model": "Unknown",
                 "serial": "Unknown", "active": false, "dpms": false, "primary": false,
                 "modes": [], "rect": {"x": 0, "y": 0, "width": 0, "height": 0}}]"#,
        )
        .unwrap()
    }

    #[test]
    fn restore_commands_replay_the_outputs_as_sway_reports_them() {
        assert_eq!(
            restore_commands(&outputs()),
            [
                "output \"HEADLESS-1\" enable mode 1024x768@60Hz position 80 120 scale 2 transform 90",
                "output \"HEADLESS-2\" disable",
            ]
        );
    }

    #[test]
    fn sway_commands_disable_the_outputs_left_out() {
        let outputs = outputs();
        let (monitors, _) = DisplayManager::monitors_of(&outputs);
        let manager = DisplayManager {
            serial: 0,
            monitors,
            logical_monitors: Vec::new(),
            properties: DisplayManagerProperties::new(),
        };
        let config: Vec<MonitorApply> = serde_json::from_str(
            r#"[{"x_pos": 0, "y_pos": 0, "scale": 1.0, "transform": 3, "primary": false,
                 "monitors": [["HEADLESS-1", "1024x768@60Hz", {}]]}]"#,
        )
        .unwrap();
        assert_eq!(
            manager.sway_commands(&config),
            [
                "output \"HEADLESS-1\" enable mode 1024x768@60Hz position 0 0 scale 1 transform 270",
                "output \"HEADLESS-2\" disable",
            ]
        );
    }
}
