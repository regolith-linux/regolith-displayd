//! The primary monitor for X programs.
//!
//! X programs that run through Xwayland ask the X server which output is
//! primary (RandR) to decide where to open and which resolution to use. sway
//! leaves that to other programs; GNOME sets it from the monitor
//! configuration (Mutter's `meta_xwayland_set_primary_output`). This keeps
//! the RandR primary of the session's Xwayland, the one `DISPLAY` names, on
//! the monitor that `GetCurrentState` reports as primary: after every change
//! this daemon answers to (an apply, an answer to the confirmation prompt,
//! new outputs from sway), and whenever Xwayland (re)starts or answers again
//! after it was stopped.
//!
//! In between, an X client may make another output primary, and that choice
//! stands until the next such change: the primary is set once per change,
//! not kept against the X clients.
//!
//! Xwayland is looked for among the processes, never by connecting to it:
//! with `xwayland enable` sway starts Xwayland only when a client connects,
//! and this daemon must not be that client. It connects only when there is
//! something to set or check, and disconnects again, so that an Xwayland
//! started on demand can still exit once its own clients are gone. A stopped
//! Xwayland (SIGSTOP) is left alone like an absent one, and each exchange
//! with the X server runs off the D-Bus service's tasks, so an X server that
//! does not answer never holds up a D-Bus reply.

use crate::{get_kanshi_paths, KanshiPaths, PrimarySource};
use log::{debug, info, warn};
use std::error::Error;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use x11rb::connection::{Connection as _, RequestConnection};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

/// How often the X server and the reported primary are looked at.
const TICK: Duration = Duration::from_millis(500);

/// How long, after the primary was set, the output it was set on is checked
/// again on every tick: Xwayland may replace that output when it learns
/// about sway's new outputs only after it was set. Only a replaced or
/// missing output is set again; a primary an X client chose is left alone.
const CHECK_AGAIN: Duration = Duration::from_secs(3);

/// How long one exchange with the X server may take.
const X_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the kanshi directory is looked up again (see
/// [`get_kanshi_paths`], which asks the resource daemon over D-Bus).
const PATHS_TTL: Duration = Duration::from_secs(10);

/// While no X server runs, the processes are looked through again on each
/// tick only if one was created since the last look, and at least this
/// often in any case.
const FULL_SCAN: Duration = Duration::from_secs(5);

/// How long to wait before subscribing to sway's output events again after
/// the connection was lost.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(1);

type XResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// An X server process: its pid and its start time, so that a restarted
/// server that got the same pid is still a new one.
type Server = (u32, u64);

/// Start following the primary, unless `DISPLAY` names no X display.
pub fn spawn(source: PrimarySource) {
    let display = std::env::var("DISPLAY").unwrap_or_default();
    let Some(number) = display_number(&display) else {
        info!("No X display (DISPLAY={display:?}): the X primary output is left alone");
        return;
    };
    tokio::spawn(follow(source, display, number));
    tokio::spawn(watch_sway_outputs());
}

/// sway's IPC: the magic string that starts each message, the length of a
/// message header, the SUBSCRIBE message, and the type of an output event
/// (an event has the high bit set).
const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const IPC_HEADER: usize = 14;
const IPC_SUBSCRIBE: u32 = 2;
const IPC_EVENT_OUTPUT: u32 = 0x8000_0001;

/// Count every output event of sway as a change (see
/// [`crate::display_event`]), so that an output disabled and enabled again
/// faster than the watcher polls still sets the X primary again. swayipc-types
/// 1.1 has no output events, hence a connection of its own that reads them
/// by their message type.
async fn watch_sway_outputs() {
    loop {
        if let Err(e) = follow_sway_outputs().await {
            if e.kind() == io::ErrorKind::Unsupported {
                // The watcher still notices the changes it sees.
                info!("No output events from sway: {e}");
                return;
            }
            debug!("sway output events: {e}");
        }
        tokio::time::sleep(RESUBSCRIBE_DELAY).await;
    }
}

async fn follow_sway_outputs() -> io::Result<()> {
    // The socket swayipc's Connection::new() uses.
    let path = std::env::var_os("I3SOCK")
        .or_else(|| std::env::var_os("SWAYSOCK"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SWAYSOCK is not set"))?;
    let mut socket = UnixStream::connect(path).await?;
    socket
        .write_all(&ipc_message(IPC_SUBSCRIBE, br#"["output"]"#))
        .await?;
    loop {
        let mut header = [0u8; IPC_HEADER];
        socket.read_exact(&mut header).await?;
        let (length, kind) = ipc_header(&header)?;
        let mut body = vec![0u8; length];
        socket.read_exact(&mut body).await?;
        if kind == IPC_SUBSCRIBE && !subscribed(&body) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sway refused the subscription",
            ));
        }
        if kind == IPC_EVENT_OUTPUT {
            crate::display_event();
        }
    }
}

/// A sway IPC message of type `kind` carrying `payload`.
fn ipc_message(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut message = IPC_MAGIC.to_vec();
    message.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    message.extend_from_slice(&kind.to_ne_bytes());
    message.extend_from_slice(payload);
    message
}

/// The payload length and the type in a sway IPC message header.
fn ipc_header(header: &[u8; IPC_HEADER]) -> io::Result<(usize, u32)> {
    if &header[..6] != IPC_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not sway IPC"));
    }
    let length = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]);
    let kind = u32::from_ne_bytes([header[10], header[11], header[12], header[13]]);
    Ok((length as usize, kind))
}

/// Whether the reply to SUBSCRIBE, `{"success": true}`, says it succeeded.
fn subscribed(reply: &[u8]) -> bool {
    let reply: Vec<u8> = reply
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let success = b"\"success\":true";
    reply.windows(success.len()).any(|w| w == success)
}

/// The display number in a `DISPLAY` value such as ":0", ":1.0", "unix:2" or
/// "localhost:0", or None for an empty or malformed one.
pub fn display_number(display: &str) -> Option<u32> {
    let (_, rest) = display.rsplit_once(':')?;
    let number = rest.split('.').next()?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// What the X primary was set for: the server, the primary, sway's active
/// outputs and the number of changes the daemon had answered to then.
#[derive(Clone, PartialEq)]
struct Wanted {
    server: Server,
    primary: String,
    active: Vec<String>,
    events: u64,
}

/// What was last set on the X server, and the RandR output it was set on.
struct Done {
    wanted: Wanted,
    output: randr::Output,
}

async fn follow(source: PrimarySource, display: String, number: u32) {
    let busy = Arc::new(AtomicBool::new(false));
    let mut server: Option<Server> = None;
    let mut done: Option<Done> = None;
    let mut check_until = Instant::now();
    let mut paths: Option<(KanshiPaths, Instant)> = None;
    let mut scan = Scan::default();
    loop {
        tokio::time::sleep(TICK).await;
        server = match server {
            Some(known) if still_running(known) => Some(known),
            _ => scan.find_xwayland(number),
        };
        let Some(current) = server else {
            // No X server: nothing to do, and nothing to connect to.
            done = None;
            continue;
        };
        if is_stopped(current.0) {
            // A stopped X server would not answer: leave it alone, and treat
            // it as a new one once it runs again.
            if done.take().is_some() {
                debug!("The X server is stopped");
            }
            continue;
        }
        if paths
            .as_ref()
            .map_or(true, |(_, at)| at.elapsed() > PATHS_TTL)
        {
            if let Ok(fresh) = get_kanshi_paths().await {
                paths = Some((fresh, Instant::now()));
            }
        }
        // Read before the state: a change made after this is then seen as a
        // new one on the next tick.
        let events = source.events();
        let (state, primary) = source.reported(paths.as_ref().map(|(p, _)| p)).await;
        let Some(primary) = primary else {
            continue;
        };
        let wanted = Wanted {
            server: current,
            primary: primary.clone(),
            active: state.active_connectors(),
            events,
        };
        let request = match &done {
            Some(done) if done.wanted == wanted => {
                if Instant::now() >= check_until {
                    continue;
                }
                Request::Repair(done.output)
            }
            _ => Request::Set,
        };
        // An X server that doesn't answer keeps its exchange: don't pile up
        // more of them behind it.
        if busy.swap(true, Ordering::AcqRel) {
            continue;
        }
        let flag = Arc::clone(&busy);
        let name = display.clone();
        let task = tokio::task::spawn_blocking(move || {
            let result = set_primary(&name, &primary, request);
            flag.store(false, Ordering::Release);
            result
        });
        match tokio::time::timeout(X_TIMEOUT, task).await {
            Ok(Ok(Ok(outcome))) => match outcome {
                Outcome::Set(output) | Outcome::Unchanged(output) => {
                    if matches!(outcome, Outcome::Set(_)) {
                        info!("X primary output set to {}", wanted.primary);
                    }
                    if request == Request::Set {
                        check_until = Instant::now() + CHECK_AGAIN;
                    }
                    done = Some(Done { wanted, output });
                }
                Outcome::Missing => {
                    debug!("The X server has no output {} yet", wanted.primary);
                    // Set it once it is there.
                    done = None;
                }
            },
            Ok(Ok(Err(e))) => debug!("Cannot set the X primary output: {e}"),
            Ok(Err(e)) => warn!("Cannot set the X primary output: {e}"),
            Err(_) => {
                warn!("The X server did not answer in time");
                done = None;
            }
        }
    }
}

/// Looks for Xwayland among the processes only when one may have started
/// since the last look: when a process was created since (the last pid the
/// kernel handed out moved on), and every [`FULL_SCAN`] in any case, for a
/// process created before the last look that ran Xwayland only after it.
#[derive(Default)]
struct Scan {
    last_pid: Option<String>,
    at: Option<Instant>,
}

impl Scan {
    fn find_xwayland(&mut self, number: u32) -> Option<Server> {
        let last_pid = last_pid();
        let due = self.at.map_or(true, |at| at.elapsed() >= FULL_SCAN);
        if !due && last_pid.is_some() && last_pid == self.last_pid {
            return None;
        }
        self.last_pid = last_pid;
        self.at = Some(Instant::now());
        find_xwayland(number)
    }
}

/// The pid most recently handed out (the last field of `/proc/loadavg`).
fn last_pid() -> Option<String> {
    loadavg_last_pid(&fs::read_to_string("/proc/loadavg").ok()?)
}

fn loadavg_last_pid(loadavg: &str) -> Option<String> {
    loadavg.split_whitespace().nth(4).map(str::to_string)
}

/// Whether `server` is still the same, live process.
fn still_running(server: Server) -> bool {
    start_time(server.0) == Some(server.1) && xwayland_args(server.0).is_some()
}

/// The Xwayland process that serves display `number`, without connecting
/// to the display.
fn find_xwayland(number: u32) -> Option<Server> {
    let wanted = format!(":{number}");
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        // The short name first, which is cheaper to read than the command
        // line of every process.
        if fs::read(format!("/proc/{pid}/comm")).ok().as_deref() != Some(b"Xwayland\n") {
            continue;
        }
        if xwayland_args(pid).is_some_and(|args| args.contains(&wanted)) {
            if let Some(start) = start_time(pid) {
                return Some((pid, start));
            }
        }
    }
    None
}

/// The arguments of process `pid` if it is Xwayland. A process that has
/// exited (a zombie) has no command line.
fn xwayland_args(pid: u32) -> Option<Vec<String>> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut args = cmdline
        .split(|b| *b == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned());
    let program = args.next()?;
    if program.rsplit('/').next() != Some("Xwayland") {
        return None;
    }
    Some(args.collect())
}

/// The fields of a `/proc/<pid>/stat` line after the command name, which is
/// in parentheses and may itself contain spaces and parentheses: the state
/// first, the start time 20th.
fn stat_fields(stat: &str) -> Option<Vec<&str>> {
    let rest = &stat[stat.rfind(')')? + 1..];
    Some(rest.split_whitespace().collect())
}

fn read_stat(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

/// The start time in a stat line, in clock ticks since boot; None for a
/// zombie.
fn stat_start_time(stat: &str) -> Option<u64> {
    let fields = stat_fields(stat)?;
    if fields.first() == Some(&"Z") {
        return None;
    }
    fields.get(19)?.parse().ok()
}

/// Whether a stat line is that of a stopped process (by a signal, or
/// traced).
fn stat_stopped(stat: &str) -> bool {
    stat_fields(stat).is_some_and(|f| matches!(f.first(), Some(&("T" | "t"))))
}

fn start_time(pid: u32) -> Option<u64> {
    stat_start_time(&read_stat(pid)?)
}

fn is_stopped(pid: u32) -> bool {
    read_stat(pid).is_some_and(|stat| stat_stopped(&stat))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Request {
    /// Make the output primary: a change this daemon answers to.
    Set,
    /// The primary was set on this output: set it again only if Xwayland
    /// has replaced that output since, whatever the primary is now.
    Repair(randr::Output),
}

#[derive(Debug, PartialEq)]
enum Outcome {
    /// The primary was another output, or none, and is now this one.
    Set(randr::Output),
    /// Nothing to do: the primary already was this output, or (Repair) the
    /// output it was set on is still there.
    Unchanged(randr::Output),
    /// The X server has no connected output with that name (yet).
    Missing,
}

/// Make the X server's output named `connector` its RandR primary, as
/// `request` says. Xwayland names its outputs after the Wayland outputs,
/// so the name is sway's connector, whatever the layout and scale.
///
/// This is RandR's SetOutputPrimary request alone. (`xrandr --primary` also
/// sets the screen size, which Xwayland refuses for layouts that don't start
/// at (0, 0).)
fn set_primary(display: &str, connector: &str, request: Request) -> XResult<Outcome> {
    let (conn, screen) = RustConnection::connect(Some(display))?;
    if conn
        .extension_information(randr::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Err("the X server has no RandR".into());
    }
    let root = conn.setup().roots[screen].root;
    conn.randr_query_version(1, 3)?.reply()?;
    let resources = conn.randr_get_screen_resources_current(root)?.reply()?;
    let mut target = None;
    for &output in &resources.outputs {
        let info = conn
            .randr_get_output_info(output, resources.config_timestamp)?
            .reply()?;
        if info.name == connector.as_bytes() && info.connection == randr::Connection::CONNECTED {
            target = Some(output);
            break;
        }
    }
    let Some(target) = target else {
        return Ok(Outcome::Missing);
    };
    if request == Request::Repair(target) {
        return Ok(Outcome::Unchanged(target));
    }
    if conn.randr_get_output_primary(root)?.reply()?.output == target {
        return Ok(Outcome::Unchanged(target));
    }
    conn.randr_set_output_primary(root, target)?.check()?;
    if conn.randr_get_output_primary(root)?.reply()?.output == target {
        Ok(Outcome::Set(target))
    } else {
        Ok(Outcome::Missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_numbers_are_read_from_display() {
        assert_eq!(display_number(":0"), Some(0));
        assert_eq!(display_number(":12.0"), Some(12));
        assert_eq!(display_number("unix:3"), Some(3));
        assert_eq!(display_number("localhost:1.1"), Some(1));
        assert_eq!(display_number(""), None);
        assert_eq!(display_number(":"), None);
        assert_eq!(display_number("wayland-1"), None);
        assert_eq!(display_number(":x"), None);
        assert_eq!(display_number(":-1"), None);
    }

    #[test]
    fn this_process_is_not_xwayland() {
        let me = std::process::id();
        assert!(xwayland_args(me).is_none());
        assert!(start_time(me).is_some());
        assert!(!is_stopped(me));
        assert!(!still_running((me, start_time(me).unwrap())));
    }

    #[test]
    fn stat_lines_are_read_past_the_command_name() {
        // A command name may contain spaces and parentheses.
        let mut stat = String::from("4242 (Xway) (x y) T 1 4242 4242 0 -1 4194560");
        stat.push_str(" 0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0");
        assert!(stat_stopped(&stat));
        assert_eq!(stat_start_time(&stat), Some(987654));
        let running = stat.replace(") T ", ") S ");
        assert!(!stat_stopped(&running));
        assert_eq!(stat_start_time(&running), Some(987654));
        // An exited process (a zombie) has no start time worth comparing.
        assert_eq!(stat_start_time(&stat.replace(") T ", ") Z ")), None);
        assert_eq!(stat_start_time("garbage"), None);
    }

    #[test]
    fn the_last_pid_is_read_from_loadavg() {
        assert_eq!(
            loadavg_last_pid("0.15 0.20 0.18 2/431 98765\n").as_deref(),
            Some("98765")
        );
        assert_eq!(loadavg_last_pid("0.15 0.20"), None);
        assert!(last_pid().is_some());
    }

    #[test]
    fn sway_ipc_messages_are_framed() {
        let message = ipc_message(IPC_SUBSCRIBE, br#"["output"]"#);
        let header: [u8; IPC_HEADER] = message[..IPC_HEADER].try_into().unwrap();
        assert_eq!(ipc_header(&header).unwrap(), (10, IPC_SUBSCRIBE));
        assert_eq!(&message[IPC_HEADER..], br#"["output"]"#);
        let mut bad = header;
        bad[0] = b'x';
        assert!(ipc_header(&bad).is_err());
    }

    #[test]
    fn subscription_replies_are_read() {
        assert!(subscribed(br#"{"success": true}"#));
        assert!(subscribed(br#"{ "success":true }"#));
        assert!(!subscribed(br#"{"success": false}"#));
        assert!(!subscribed(br#"{"success": false, "error": "true"}"#));
    }
}
