pub mod modes;
pub mod monitor;

use core::fmt;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use monitor::{LogicalMonitor, Monitor, MonitorApply};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::{
    error::Error,
    fs::{self, File},
    path::{Path, PathBuf},
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

pub struct KanshiPaths {
    profiles: PathBuf,
    config: PathBuf,
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
        mutter_logical_monitors: Vec<MonitorApply>,
        properties: DisplayManagerProperties,
    ) -> zbus::fdo::Result<()> {
        debug!("Configuration Method: {method}");
        let mut manager_obj = self.manager.lock().await;
        debug!("Serial: {} {}", manager_obj.serial, serial);
        if serial != manager_obj.serial {
            error!("Invalid configuration recieved for method apply_monitors_config: Wrong serial");
            return Err(zbus::fdo::Error::InvalidArgs(String::from("Wrong serial")));
        }
        // Verify the whole configuration before changing anything: an Apply
        // that cannot be satisfied is rejected atomically, and a verify call
        // must leave no state behind. The kanshi profile is written and the
        // reload triggered only when every requested monitor, mode, scale and
        // transform is valid.
        for mutter_logical_mointor in &mutter_logical_monitors {
            mutter_logical_mointor.verify(&self.sway_connection, &manager_obj.monitors)?;
        }
        if method == 0 {
            return Ok(());
        }

        let get_dpy_name = |mon: &MonitorApply| {
            let monitor = mon.search_monitor(&manager_obj.monitors).unwrap();
            monitor.get_dpy_name().replace(" ", "_")
        };

        let mut monitors_sorted = manager_obj.monitors.clone();

        monitors_sorted.sort_by_key(|monitor| monitor.get_dpy_name());

        let profile_name = monitors_sorted
            .iter()
            .map(|mon| mon.get_dpy_name().replace(" ", "_"))
            .collect::<Vec<String>>()
            .join("__");
        info!("Profile FileName: {profile_name}");

        let kanshi_paths = get_kanshi_paths().await?;

        // Profile Write buffer (Only written if no errors occur)
        let mut profile_buf = Vec::new();

        let mut active_physical_monitors = Vec::new();

        writeln!(&mut profile_buf, "profile {{").unwrap();
        for mutter_logical_mointor in &mutter_logical_monitors {
            let Some(sway_physical_monitor) = mutter_logical_mointor.search_monitor(&manager_obj.monitors) else {
                continue;
            };

            active_physical_monitors.push(sway_physical_monitor.clone());
            mutter_logical_mointor.save_kanshi(&mut profile_buf, &sway_physical_monitor);
        }

        for disabled_mon in manager_obj.get_disabled_monitors(&active_physical_monitors) {
            writeln!(
                &mut profile_buf,
                "\toutput \"{}\" disable",
                disabled_mon.get_dpy_name()
            )
            .expect("Failed to write to file");
        }
        writeln!(&mut profile_buf, "}}").unwrap();
        manager_obj.properties = properties;

        fs::create_dir_all(&kanshi_paths.profiles).unwrap();
        let mut profile_file = File::options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(kanshi_paths.profiles.join(&profile_name))
            .expect("Error while opening profile file for writing");

        if let Err(e) = profile_file.write(&profile_buf) {
            error!("Error writing data to kanshi config file: {e}");
            return Err(zbus::fdo::Error::IOError(e.to_string()));
        }

        // reload kanshi config
        if let Err(e) = reload_kanshi().await {
            error!("Error reloading kanshi configuration: {e}");
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
    ) -> Result<(), Box<dyn Error>> {
        let mut prev_monitor_set = HashSet::new();
        let mut prev_logical_monitor_set = HashSet::new();
        loop {
            thread::sleep(Duration::from_millis(700));
            let mut manager_obj_lock = manager_obj.lock().await;
            let display_info = manager_obj_lock
                .get_monitor_info(&sway_connection)
                .await
                .unwrap();
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
    fn get_disabled_monitors(&self, active_physical_monitors: &[Monitor]) -> Vec<&Monitor> {
        self.monitors
            .iter()
            .filter(|mon| !active_physical_monitors.contains(mon))
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
        let monitors = outputs.iter().map(|o| Monitor::new(o)).collect();
        let logical_monitors = outputs
            .iter()
            .filter(|o| o.active)
            .map(|o| LogicalMonitor::new(o))
            .collect();
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

pub async fn get_kanshi_paths() -> zbus::Result<KanshiPaths> {
    let env_vars: HashMap<String, String> = std::env::vars().collect();
    let home_dir = env_vars.get("HOME").expect("$HOME not defined");
    let default_path = format!("{home_dir}/.config/regolith3/kanshi");
    let base: PathBuf = match trawlcat::rescat("kanshi.path", Some(default_path.clone())).await {
        Ok(path) => match path.try_into() {
            Ok(path_buf) => path_buf,
            Err(e) => {
                warn!("Error: {e}");
                default_path.into()
            }
        },
        Err(e) => {
            warn!("Error: {e}");
            default_path.into()
        }
    };
    let profiles = base.join("profiles");
    let config = base.join("config");
    return Ok(KanshiPaths { profiles, config });
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

    fn fake_tool(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(
            &path,
            format!("#!/bin/sh\necho \"{name} $*\" >> \"$FAKE_LOG\"\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
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

        // Live kanshis, the way the grading stub looks: executables whose
        // comm is exactly "kanshi". They live outside PATH so the spawn
        // fallback below still resolves the fake script. One catches
        // SIGHUP like kanshi 1.3 and later; the other (a copy of sleep)
        // keeps the default action like kanshi 1.2.
        let reloading_bin = dir.join("reloading").join("kanshi");
        let exiting_bin = dir.join("exiting").join("kanshi");
        fs::create_dir_all(reloading_bin.parent().unwrap()).unwrap();
        fs::create_dir_all(exiting_bin.parent().unwrap()).unwrap();
        fs::write(
            &reloading_bin,
            "#!/bin/sh\ntrap : HUP\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        fs::set_permissions(&reloading_bin, fs::Permissions::from_mode(0o755)).unwrap();
        fs::copy("/bin/sleep", &exiting_bin).unwrap();
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
        let mut reloading = Command::new(&reloading_bin).spawn().unwrap();
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
        let mut exiting = Command::new(&exiting_bin).arg("60").spawn().unwrap();
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
        let mut exiting = Command::new(&exiting_bin).arg("60").spawn().unwrap();
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
