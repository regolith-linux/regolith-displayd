//! Applying Night Light gamma tables through the compositor.
//!
//! A minimal Wayland client speaking just what Night Light needs over the
//! compositor socket (`$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`): the registry,
//! `wl_output` and wlr-gamma-control (`zwlr_gamma_control_manager_v1` and
//! `zwlr_gamma_control_v1`).
//!
//! One gamma control is held per output for as long as the connection
//! lives: destroying a control makes the compositor restore the output's
//! previous tables, and only one client may hold the control of an output.
//! When Night Light turns off the identity table is therefore written
//! explicitly rather than by dropping the control.

use crate::night_light::{gamma_table, NEUTRAL_TEMPERATURE};
use log::{debug, info, warn};
use nix::fcntl::{fcntl, FcntlArg, SealFlag};
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use nix::sys::uio::IoVec;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// How long to wait before connecting again when there is no compositor.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

const DISPLAY_ID: u32 = 1;
// wl_display requests and events.
const DISPLAY_SYNC: u16 = 0;
const DISPLAY_GET_REGISTRY: u16 = 1;
const DISPLAY_ERROR: u16 = 0;
// wl_registry.
const REGISTRY_BIND: u16 = 0;
const REGISTRY_GLOBAL: u16 = 0;
const REGISTRY_GLOBAL_REMOVE: u16 = 1;
// wl_output (release exists since version 3).
const OUTPUT_RELEASE: u16 = 0;
const OUTPUT_MAX_VERSION: u32 = 3;
// zwlr_gamma_control_manager_v1.
const MANAGER_GET_GAMMA_CONTROL: u16 = 0;
// zwlr_gamma_control_v1.
const CONTROL_SET_GAMMA: u16 = 0;
const CONTROL_DESTROY: u16 = 1;
const CONTROL_GAMMA_SIZE: u16 = 0;
const CONTROL_FAILED: u16 = 1;

/// Input of the gamma thread.
#[derive(Debug)]
pub enum Command {
    /// The colour temperature the outputs should have, in Kelvin.
    Temperature(u32),
    /// An event read from the compositor connection `generation`.
    Event {
        generation: u64,
        object: u32,
        opcode: u16,
        args: Vec<u8>,
    },
    /// The compositor connection `generation` was closed.
    Disconnected { generation: u64, reason: String },
}

/// Encode a request: object id, then size (header included) and opcode.
fn request(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
    let size = 8 + args.len() as u32;
    let mut message = Vec::with_capacity(size as usize);
    message.extend_from_slice(&object.to_ne_bytes());
    message.extend_from_slice(&((size << 16) | opcode as u32).to_ne_bytes());
    message.extend_from_slice(args);
    message
}

fn put_u32(args: &mut Vec<u8>, value: u32) {
    args.extend_from_slice(&value.to_ne_bytes());
}

/// A string argument: length including the NUL, bytes, NUL, padding to 4.
fn put_string(args: &mut Vec<u8>, value: &str) {
    put_u32(args, value.len() as u32 + 1);
    args.extend_from_slice(value.as_bytes());
    args.push(0);
    while args.len() % 4 != 0 {
        args.push(0);
    }
}

/// Reads the arguments of an event.
struct Args<'a> {
    data: &'a [u8],
}

impl<'a> Args<'a> {
    fn u32(&mut self) -> io::Result<u32> {
        if self.data.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short event"));
        }
        let (head, rest) = self.data.split_at(4);
        self.data = rest;
        Ok(u32::from_ne_bytes([head[0], head[1], head[2], head[3]]))
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        let padded = (len + 3) & !3;
        if len == 0 || self.data.len() < padded {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad string"));
        }
        let text = String::from_utf8_lossy(&self.data[..len - 1]).into_owned();
        self.data = &self.data[padded..];
        Ok(text)
    }
}

/// Split complete messages off the front of `buffer`.
fn split_messages(buffer: &mut Vec<u8>) -> io::Result<Vec<(u32, u16, Vec<u8>)>> {
    let mut messages = Vec::new();
    let mut offset = 0;
    while buffer.len() - offset >= 8 {
        let word = |at: usize| {
            u32::from_ne_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]])
        };
        let object = word(offset);
        let header = word(offset + 4);
        let size = (header >> 16) as usize;
        if size < 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad message size",
            ));
        }
        if buffer.len() - offset < size {
            break;
        }
        let args = buffer[offset + 8..offset + size].to_vec();
        messages.push((object, (header & 0xffff) as u16, args));
        offset += size;
    }
    buffer.drain(..offset);
    Ok(messages)
}

fn socket_path() -> io::Result<PathBuf> {
    let display = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    let display = PathBuf::from(display);
    if display.is_absolute() {
        return Ok(display);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => Ok(PathBuf::from(dir).join(display)),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "XDG_RUNTIME_DIR is not set",
        )),
    }
}

/// A file holding `table` as little-endian uint16 values, ready to be
/// passed to the compositor.
fn table_file(table: &[u16]) -> io::Result<File> {
    let bytes: Vec<u8> = table.iter().flat_map(|v| v.to_le_bytes()).collect();
    let name = CString::new("regolith-displayd-gamma").expect("no NUL in name");
    let (mut file, sealable) = match memfd_create(
        &name,
        MemFdCreateFlag::MFD_CLOEXEC | MemFdCreateFlag::MFD_ALLOW_SEALING,
    ) {
        // SAFETY: memfd_create returned a new descriptor that nothing else owns.
        Ok(fd) => (unsafe { File::from_raw_fd(fd) }, true),
        Err(_) => (unlinked_file()?, false),
    };
    file.write_all(&bytes)?;
    file.seek(SeekFrom::Start(0))?;
    if sealable {
        let seals = SealFlag::F_SEAL_SHRINK
            | SealFlag::F_SEAL_GROW
            | SealFlag::F_SEAL_WRITE
            | SealFlag::F_SEAL_SEAL;
        if let Err(e) = fcntl(file.as_raw_fd(), FcntlArg::F_ADD_SEALS(seals)) {
            debug!("Cannot seal the gamma table: {e}");
        }
    }
    Ok(file)
}

/// Fallback for kernels without memfd: an unlinked file in the runtime dir.
fn unlinked_file() -> io::Result<File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    let path = PathBuf::from(dir).join(format!("regolith-displayd-gamma-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}

#[derive(Debug, Clone, Copy)]
enum Object {
    Registry,
    Callback,
    Output,
    Manager,
    /// A gamma control, by the registry name of its output.
    Control(u32),
}

#[derive(Debug)]
struct Output {
    id: u32,
    version: u32,
    control: Option<u32>,
    gamma_size: Option<usize>,
    /// The temperature whose table the control last set; `None` while the
    /// control has set nothing, so the output still has its own tables.
    applied: Option<u32>,
    /// The compositor refused a control for this output.
    failed: bool,
    /// How many times it did, to warn only once.
    refusals: u32,
}

/// One connection to the compositor, with the objects bound on it.
struct Connection {
    stream: UnixStream,
    next_id: u32,
    registry: u32,
    objects: HashMap<u32, Object>,
    /// Registry name and object id of the gamma control manager.
    manager: Option<(u32, u32)>,
    /// Outputs by registry name.
    outputs: HashMap<u32, Output>,
    temperature: u32,
}

impl Connection {
    /// Connect, start a thread forwarding the compositor's events to
    /// `events`, and ask for the globals.
    fn open(generation: u64, temperature: u32, events: Sender<Command>) -> io::Result<Self> {
        let path = socket_path()?;
        let stream = UnixStream::connect(&path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        let mut reader = stream.try_clone()?;
        std::thread::Builder::new()
            .name("night-light-wayland".into())
            .spawn(move || {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let reason = loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => break "the compositor closed the connection".to_string(),
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => break e.to_string(),
                    }
                    match split_messages(&mut buffer) {
                        Ok(messages) => {
                            for (object, opcode, args) in messages {
                                let event = Command::Event {
                                    generation,
                                    object,
                                    opcode,
                                    args,
                                };
                                if events.send(event).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => break e.to_string(),
                    }
                };
                let _ = events.send(Command::Disconnected { generation, reason });
            })?;
        let mut connection = Connection {
            stream,
            next_id: 2,
            registry: 0,
            objects: HashMap::new(),
            manager: None,
            outputs: HashMap::new(),
            temperature,
        };
        let registry = connection.new_object(Object::Registry);
        connection.registry = registry;
        connection.send(DISPLAY_ID, DISPLAY_GET_REGISTRY, &registry.to_ne_bytes())?;
        // The callback fires once the initial globals have been sent.
        let callback = connection.new_object(Object::Callback);
        connection.send(DISPLAY_ID, DISPLAY_SYNC, &callback.to_ne_bytes())?;
        Ok(connection)
    }

    fn new_object(&mut self, object: Object) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.objects.insert(id, object);
        id
    }

    fn send(&mut self, object: u32, opcode: u16, args: &[u8]) -> io::Result<()> {
        self.stream.write_all(&request(object, opcode, args))
    }

    /// Send a request whose only argument is a file descriptor.
    fn send_fd(&mut self, object: u32, opcode: u16, file: &File) -> io::Result<()> {
        let message = request(object, opcode, &[]);
        let fds = [file.as_raw_fd()];
        let sent = loop {
            match sendmsg(
                self.stream.as_raw_fd(),
                &[IoVec::from_slice(&message)],
                &[ControlMessage::ScmRights(&fds)],
                // Rust ignores SIGPIPE, so a closed socket is an EPIPE error.
                MsgFlags::empty(),
                None,
            ) {
                Ok(sent) => break sent,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(io::Error::from(e)),
            }
        };
        self.stream.write_all(&message[sent..])
    }

    fn bind(
        &mut self,
        name: u32,
        interface: &str,
        version: u32,
        object: Object,
    ) -> io::Result<u32> {
        let id = self.new_object(object);
        let mut args = Vec::new();
        put_u32(&mut args, name);
        put_string(&mut args, interface);
        put_u32(&mut args, version);
        put_u32(&mut args, id);
        self.send(self.registry, REGISTRY_BIND, &args)?;
        Ok(id)
    }

    /// Take the gamma control of the output `name` if it has none.
    fn request_control(&mut self, name: u32) -> io::Result<()> {
        let Some((_, manager)) = self.manager else {
            return Ok(());
        };
        let Some(output) = self.outputs.get(&name) else {
            return Ok(());
        };
        if output.control.is_some() || output.failed {
            return Ok(());
        }
        let output_id = output.id;
        let control = self.new_object(Object::Control(name));
        let mut args = Vec::new();
        put_u32(&mut args, control);
        put_u32(&mut args, output_id);
        self.send(manager, MANAGER_GET_GAMMA_CONTROL, &args)?;
        let output = self.outputs.get_mut(&name).expect("checked above");
        output.control = Some(control);
        output.gamma_size = None;
        output.applied = None;
        Ok(())
    }

    fn destroy_control(&mut self, name: u32) -> io::Result<()> {
        let Some(output) = self.outputs.get_mut(&name) else {
            return Ok(());
        };
        output.gamma_size = None;
        output.applied = None;
        if let Some(control) = output.control.take() {
            self.objects.remove(&control);
            self.send(control, CONTROL_DESTROY, &[])?;
        }
        Ok(())
    }

    /// Bring the output `name` in line with the current temperature.
    fn apply_output(&mut self, name: u32) -> io::Result<()> {
        let temperature = self.temperature.min(NEUTRAL_TEMPERATURE);
        let Some(output) = self.outputs.get(&name) else {
            return Ok(());
        };
        let (Some(control), Some(size)) = (output.control, output.gamma_size) else {
            return Ok(());
        };
        let up_to_date = match output.applied {
            Some(applied) => applied == temperature,
            // Nothing set yet: the output shows its own colours.
            None => temperature == NEUTRAL_TEMPERATURE,
        };
        if up_to_date || size == 0 {
            return Ok(());
        }
        let file = table_file(&gamma_table(temperature, size))?;
        self.send_fd(control, CONTROL_SET_GAMMA, &file)?;
        debug!("Set a {temperature} K gamma table of {size} entries on output {name}");
        if let Some(output) = self.outputs.get_mut(&name) {
            output.applied = Some(temperature);
        }
        Ok(())
    }

    fn apply(&mut self) -> io::Result<()> {
        let names: Vec<u32> = self.outputs.keys().copied().collect();
        for name in names {
            self.apply_output(name)?;
        }
        Ok(())
    }

    fn set_temperature(&mut self, temperature: u32) -> io::Result<()> {
        if self.temperature >= NEUTRAL_TEMPERATURE && temperature < NEUTRAL_TEMPERATURE {
            // Night Light is turning on: try again on outputs whose control
            // was refused (e.g. another gamma tool has quit since).
            let refused: Vec<u32> = self
                .outputs
                .iter_mut()
                .filter(|(_, o)| o.failed)
                .map(|(name, o)| {
                    o.failed = false;
                    *name
                })
                .collect();
            for name in refused {
                self.request_control(name)?;
            }
        }
        self.temperature = temperature;
        self.apply()
    }

    fn handle(&mut self, object: u32, opcode: u16, args: &[u8]) -> io::Result<()> {
        let mut args = Args { data: args };
        if object == DISPLAY_ID {
            if opcode == DISPLAY_ERROR {
                let id = args.u32()?;
                let code = args.u32()?;
                let message = args.string()?;
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("Wayland protocol error on object {id} (code {code}): {message}"),
                ));
            }
            // delete_id: ids are never reused, nothing to do.
            return Ok(());
        }
        let Some(&kind) = self.objects.get(&object) else {
            // An event for an object already destroyed on our side.
            return Ok(());
        };
        match (kind, opcode) {
            (Object::Registry, REGISTRY_GLOBAL) => {
                let name = args.u32()?;
                let interface = args.string()?;
                let version = args.u32()?;
                self.global(name, &interface, version)?;
            }
            (Object::Registry, REGISTRY_GLOBAL_REMOVE) => {
                let name = args.u32()?;
                self.global_remove(name)?;
            }
            (Object::Callback, _) => {
                self.objects.remove(&object);
                if self.manager.is_none() {
                    info!(
                        "The compositor does not offer zwlr_gamma_control_manager_v1; \
                         Night Light cannot be applied"
                    );
                }
            }
            (Object::Control(name), CONTROL_GAMMA_SIZE) => {
                let size = args.u32()? as usize;
                if let Some(output) = self.outputs.get_mut(&name) {
                    debug!("Output {name} has gamma ramps of {size} entries");
                    output.gamma_size = Some(size);
                    output.applied = None;
                }
                self.apply_output(name)?;
            }
            (Object::Control(name), CONTROL_FAILED) => {
                self.destroy_control(name)?;
                if let Some(output) = self.outputs.get_mut(&name) {
                    output.failed = true;
                    output.refusals += 1;
                    if output.refusals == 1 {
                        warn!(
                            "The compositor refused the gamma control of output {name} \
                             (no gamma support, or another client holds it)"
                        );
                    } else {
                        debug!("The compositor refused the gamma control of output {name} again");
                    }
                }
            }
            // wl_output, manager and other events carry nothing we need.
            _ => {}
        }
        Ok(())
    }

    fn global(&mut self, name: u32, interface: &str, version: u32) -> io::Result<()> {
        match interface {
            "wl_output" if !self.outputs.contains_key(&name) => {
                let version = version.min(OUTPUT_MAX_VERSION);
                let id = self.bind(name, interface, version, Object::Output)?;
                self.outputs.insert(
                    name,
                    Output {
                        id,
                        version,
                        control: None,
                        gamma_size: None,
                        applied: None,
                        failed: false,
                        refusals: 0,
                    },
                );
                self.request_control(name)?;
            }
            "zwlr_gamma_control_manager_v1" if self.manager.is_none() => {
                let id = self.bind(name, interface, 1, Object::Manager)?;
                self.manager = Some((name, id));
                let names: Vec<u32> = self.outputs.keys().copied().collect();
                for output in names {
                    self.request_control(output)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn global_remove(&mut self, name: u32) -> io::Result<()> {
        if self.outputs.contains_key(&name) {
            self.destroy_control(name)?;
            let output = self.outputs.remove(&name).expect("checked above");
            self.objects.remove(&output.id);
            if output.version >= 3 {
                self.send(output.id, OUTPUT_RELEASE, &[])?;
            }
            debug!("Output {name} went away");
        } else if self.manager.map(|(n, _)| n) == Some(name) {
            // Without a manager the controls are inert; forget them.
            let names: Vec<u32> = self.outputs.keys().copied().collect();
            for output in names {
                self.destroy_control(output)?;
            }
            if let Some((_, id)) = self.manager.take() {
                self.objects.remove(&id);
            }
        }
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Wakes the reader thread so it exits.
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// The gamma thread: keep a compositor connection (reconnecting when there
/// is none) and apply each temperature received on `commands` to every
/// output. `events` feeds the compositor's events back into `commands`.
pub fn run(commands: Receiver<Command>, events: Sender<Command>) {
    let mut temperature = NEUTRAL_TEMPERATURE;
    let mut connection: Option<Connection> = None;
    let mut generation = 0u64;
    let mut reconnect_at = Some(Instant::now());
    let mut warned = false;
    loop {
        if connection.is_none() {
            if let Some(at) = reconnect_at {
                if Instant::now() >= at {
                    generation += 1;
                    match Connection::open(generation, temperature, events.clone()) {
                        Ok(c) => {
                            info!("Connected to the compositor for Night Light");
                            connection = Some(c);
                            reconnect_at = None;
                            warned = false;
                        }
                        Err(e) => {
                            if warned {
                                debug!("Cannot connect to the compositor: {e}");
                            } else {
                                warn!("Night Light cannot reach the compositor (will retry): {e}");
                                warned = true;
                            }
                            reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
                        }
                    }
                }
            }
        }
        let command = match (connection.is_some(), reconnect_at) {
            (false, Some(at)) => {
                match commands.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            _ => match commands.recv() {
                Ok(command) => command,
                Err(_) => return,
            },
        };
        let current = generation;
        let result = match command {
            Command::Temperature(t) => {
                temperature = t;
                match connection.as_mut() {
                    Some(c) => c.set_temperature(t),
                    None => Ok(()),
                }
            }
            Command::Event {
                generation,
                object,
                opcode,
                args,
            } if generation == current => match connection.as_mut() {
                Some(c) => c.handle(object, opcode, &args),
                None => Ok(()),
            },
            Command::Disconnected { generation, reason } if generation == current => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, reason))
            }
            // Leftovers from an earlier connection.
            _ => Ok(()),
        };
        if let Err(e) = result {
            if connection.take().is_some() {
                warn!("Lost the compositor connection for Night Light: {e}");
                reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_encoded_like_libwayland() {
        let mut args = Vec::new();
        put_u32(&mut args, 7);
        put_string(&mut args, "wl_output");
        put_u32(&mut args, 3);
        put_u32(&mut args, 5);
        let message = request(2, REGISTRY_BIND, &args);
        // 8 header + 4 name + 4 length + 12 padded string + 4 + 4.
        assert_eq!(message.len(), 36);
        assert_eq!(&message[0..4], &2u32.to_ne_bytes());
        assert_eq!(&message[4..8], &((36u32 << 16) | 0).to_ne_bytes());
        assert_eq!(&message[12..16], &10u32.to_ne_bytes());
        assert_eq!(&message[16..28], b"wl_output\0\0\0");
    }

    #[test]
    fn events_are_split_and_decoded() {
        let mut args = Vec::new();
        put_u32(&mut args, 100);
        put_string(&mut args, "zwlr_gamma_control_manager_v1");
        put_u32(&mut args, 1);
        let mut buffer = request(2, REGISTRY_GLOBAL, &args);
        buffer.extend(request(9, CONTROL_GAMMA_SIZE, &256u32.to_ne_bytes()));
        let partial = buffer.split_off(buffer.len() - 4);
        let messages = split_messages(&mut buffer).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(buffer.len(), 8);
        let (object, opcode, data) = &messages[0];
        assert_eq!((*object, *opcode), (2, REGISTRY_GLOBAL));
        let mut decoded = Args { data };
        assert_eq!(decoded.u32().unwrap(), 100);
        assert_eq!(decoded.string().unwrap(), "zwlr_gamma_control_manager_v1");
        assert_eq!(decoded.u32().unwrap(), 1);
        buffer.extend(partial);
        let messages = split_messages(&mut buffer).unwrap();
        assert_eq!(
            messages,
            vec![(9, CONTROL_GAMMA_SIZE, 256u32.to_ne_bytes().to_vec())]
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn tables_are_little_endian_ramps() {
        let mut file = table_file(&gamma_table(NEUTRAL_TEMPERATURE, 4)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        let values: Vec<u16> = bytes
            .chunks(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(values, [0, 21845, 43690, 65535].repeat(3));
    }
}
