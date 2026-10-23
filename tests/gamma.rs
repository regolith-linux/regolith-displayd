//! The gamma client against a fake compositor that offers wl_output and
//! wlr-gamma-control and records the tables it is given.

use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use nix::sys::uio::IoVec;
use regolith_displayd::gamma;
use regolith_displayd::night_light::{gamma_table, NEUTRAL_TEMPERATURE};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::watch;

const MANAGER_NAME: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Object {
    Display,
    Registry,
    Output(u32),
    Manager,
    Control(u32),
}

/// What the client asked for that the test looks at.
#[derive(Debug, PartialEq)]
enum Request {
    SetGamma { output: u32, table: Vec<u16> },
    Destroy { output: u32 },
}

struct FakeCompositor {
    stream: UnixStream,
    buffer: Vec<u8>,
    fds: VecDeque<RawFd>,
    objects: HashMap<u32, Object>,
    registry: Option<u32>,
    /// Gamma ramp size of each output, by registry name.
    sizes: HashMap<u32, u32>,
    /// Outputs whose gamma control is refused (another client holds it).
    refused: HashSet<u32>,
}

fn word(data: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(data[at..at + 4].try_into().unwrap())
}

fn string(data: &[u8], at: usize) -> (String, usize) {
    let len = word(data, at) as usize;
    let text = String::from_utf8(data[at + 4..at + 4 + len - 1].to_vec()).unwrap();
    (text, at + 4 + ((len + 3) & !3))
}

impl FakeCompositor {
    fn accept(listener: &UnixListener) -> Self {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        FakeCompositor {
            stream,
            buffer: Vec::new(),
            fds: VecDeque::new(),
            objects: HashMap::from([(1, Object::Display)]),
            registry: None,
            sizes: HashMap::new(),
            refused: HashSet::new(),
        }
    }

    fn event(&mut self, object: u32, opcode: u16, args: &[u8]) {
        let size = 8 + args.len() as u32;
        let mut message = object.to_ne_bytes().to_vec();
        message.extend_from_slice(&((size << 16) | opcode as u32).to_ne_bytes());
        message.extend_from_slice(args);
        self.stream.write_all(&message).unwrap();
    }

    fn global(&mut self, name: u32, interface: &str, version: u32) {
        let mut args = name.to_ne_bytes().to_vec();
        args.extend_from_slice(&(interface.len() as u32 + 1).to_ne_bytes());
        args.extend_from_slice(interface.as_bytes());
        args.push(0);
        while args.len() % 4 != 0 {
            args.push(0);
        }
        args.extend_from_slice(&version.to_ne_bytes());
        let registry = self.registry.expect("no registry yet");
        self.event(registry, 0, &args);
    }

    fn add_output(&mut self, name: u32, gamma_size: u32) {
        self.sizes.insert(name, gamma_size);
        self.global(name, "wl_output", 4);
    }

    fn remove_output(&mut self, name: u32) {
        let registry = self.registry.expect("no registry yet");
        self.event(registry, 1, &name.to_ne_bytes());
    }

    /// Read more bytes (and file descriptors) from the client.
    fn fill(&mut self) {
        let mut chunk = [0u8; 4096];
        let mut space = nix::cmsg_space!([RawFd; 8]);
        let message = recvmsg(
            self.stream.as_raw_fd(),
            &[IoVec::from_mut_slice(&mut chunk)],
            Some(&mut space),
            MsgFlags::empty(),
        )
        .expect("no request from the client in time");
        assert!(message.bytes > 0, "the client disconnected");
        for cmsg in message.cmsgs() {
            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                self.fds.extend(fds);
            }
        }
        let bytes = message.bytes;
        self.buffer.extend_from_slice(&chunk[..bytes]);
    }

    /// Wait until the client closes the connection, ignoring its requests.
    fn expect_disconnect(&mut self) {
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(_) => continue,
                Err(e) => panic!("the client did not disconnect: {e}"),
            }
        }
    }

    /// Serve the client until it sends a request the test looks at.
    fn next(&mut self) -> Request {
        loop {
            while self.buffer.len() < 8
                || self.buffer.len() < (word(&self.buffer, 4) >> 16) as usize
            {
                self.fill();
            }
            let size = (word(&self.buffer, 4) >> 16) as usize;
            let message: Vec<u8> = self.buffer.drain(..size).collect();
            let (object, opcode) = (word(&message, 0), word(&message, 4) & 0xffff);
            let args = &message[8..];
            let kind = *self
                .objects
                .get(&object)
                .expect("request on unknown object");
            match (kind, opcode) {
                (Object::Display, 0) => self.event(word(args, 0), 0, &0u32.to_ne_bytes()),
                (Object::Display, 1) => {
                    let registry = word(args, 0);
                    self.objects.insert(registry, Object::Registry);
                    self.registry = Some(registry);
                    self.global(MANAGER_NAME, "zwlr_gamma_control_manager_v1", 1);
                    for name in self.sizes.keys().copied().collect::<Vec<_>>() {
                        self.global(name, "wl_output", 4);
                    }
                }
                (Object::Registry, 0) => {
                    let name = word(args, 0);
                    let (interface, at) = string(args, 4);
                    let version = word(args, at);
                    let id = word(args, at + 4);
                    let object = match interface.as_str() {
                        "wl_output" => {
                            assert!(version <= 4);
                            Object::Output(name)
                        }
                        "zwlr_gamma_control_manager_v1" => Object::Manager,
                        other => panic!("bound unexpected {other}"),
                    };
                    self.objects.insert(id, object);
                }
                (Object::Manager, 0) => {
                    let (control, output) = (word(args, 0), word(args, 4));
                    let Some(&Object::Output(name)) = self.objects.get(&output) else {
                        panic!("gamma control for unknown output {output}");
                    };
                    self.objects.insert(control, Object::Control(name));
                    if self.refused.contains(&name) {
                        self.event(control, 1, &[]);
                    } else {
                        let size = self.sizes[&name];
                        self.event(control, 0, &size.to_ne_bytes());
                    }
                }
                (Object::Control(output), 0) => {
                    let fd = self.fds.pop_front().expect("set_gamma without a file");
                    // SAFETY: the descriptor was just received and is ours.
                    let mut file = unsafe { File::from_raw_fd(fd) };
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes).unwrap();
                    let table = bytes
                        .chunks(2)
                        .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                        .collect();
                    return Request::SetGamma { output, table };
                }
                (Object::Control(output), 1) => {
                    self.objects.remove(&object);
                    return Request::Destroy { output };
                }
                (Object::Output(_), 0) => {
                    self.objects.remove(&object);
                }
                other => panic!("unexpected request {other:?}"),
            }
        }
    }

    /// The next tables set, one per output in `outputs`, in any order.
    fn tables(&mut self, outputs: &[u32]) -> HashMap<u32, Vec<u16>> {
        let mut tables = HashMap::new();
        while tables.len() < outputs.len() {
            match self.next() {
                Request::SetGamma { output, table } => {
                    assert!(outputs.contains(&output), "table set on output {output}");
                    assert!(tables.insert(output, table).is_none());
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        tables
    }
}

/// The tests point WAYLAND_DISPLAY at their own socket, so they take turns.
static DISPLAY: Mutex<()> = Mutex::new(());

struct Setup {
    listener: UnixListener,
    dir: PathBuf,
    _display: MutexGuard<'static, ()>,
}

impl Setup {
    fn new(name: &str) -> Self {
        let display = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("displayd-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("wayland-test");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        // An absolute WAYLAND_DISPLAY is used as is, as libwayland does.
        std::env::set_var("WAYLAND_DISPLAY", &socket);
        Setup {
            listener,
            dir,
            _display: display,
        }
    }

    /// Start the gamma client on its own thread, as the daemon does.
    fn start(&self) -> watch::Sender<u32> {
        let (sink, temperature) = watch::channel(NEUTRAL_TEMPERATURE);
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(gamma::run(temperature))
        });
        sink
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn applies_temperature_to_every_output() {
    let setup = Setup::new("gamma-test");
    let sink = setup.start();
    let mut compositor = FakeCompositor::accept(&setup.listener);
    compositor.sizes.insert(1, 256);
    compositor.sizes.insert(2, 1024);

    // Night Light turns on: each output gets a table of its own size.
    sink.send(2700).unwrap();
    let tables = compositor.tables(&[1, 2]);
    assert_eq!(tables[&1], gamma_table(2700, 256));
    assert_eq!(tables[&2], gamma_table(2700, 1024));

    sink.send(3500).unwrap();
    let tables = compositor.tables(&[1, 2]);
    assert_eq!(tables[&2], gamma_table(3500, 1024));

    // A new output gets the current temperature.
    compositor.add_output(3, 4096);
    assert_eq!(compositor.tables(&[3])[&3], gamma_table(3500, 4096));

    // An output that goes away loses its control.
    compositor.remove_output(2);
    assert_eq!(compositor.next(), Request::Destroy { output: 2 });

    // Night Light turns off: the identity is written, the controls stay.
    sink.send(NEUTRAL_TEMPERATURE).unwrap();
    let tables = compositor.tables(&[1, 3]);
    assert_eq!(tables[&1], gamma_table(NEUTRAL_TEMPERATURE, 256));
    assert_eq!(tables[&3], gamma_table(NEUTRAL_TEMPERATURE, 4096));

    // The daemon stops: the connection closes, so the compositor restores
    // the outputs' own tables.
    drop(sink);
    compositor.expect_disconnect();
}

#[test]
fn retries_refused_outputs_and_reconnects() {
    let setup = Setup::new("gamma-retry-test");
    let sink = setup.start();
    let mut compositor = FakeCompositor::accept(&setup.listener);
    compositor.sizes.insert(1, 256);
    compositor.sizes.insert(2, 256);
    // Another gamma tool holds output 2.
    compositor.refused.insert(2);

    // The refused control is destroyed; output 1 still gets its table.
    sink.send(3000).unwrap();
    let mut table = None;
    let mut destroyed = false;
    while table.is_none() || !destroyed {
        match compositor.next() {
            Request::SetGamma {
                output: 1,
                table: t,
            } => table = Some(t),
            Request::Destroy { output: 2 } => destroyed = true,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(table.unwrap(), gamma_table(3000, 256));

    // The other tool quits. Output 2 is tried again the next time Night
    // Light turns on.
    compositor.refused.clear();
    sink.send(NEUTRAL_TEMPERATURE).unwrap();
    assert_eq!(
        compositor.tables(&[1])[&1],
        gamma_table(NEUTRAL_TEMPERATURE, 256)
    );
    sink.send(2700).unwrap();
    let tables = compositor.tables(&[1, 2]);
    assert_eq!(tables[&1], gamma_table(2700, 256));
    assert_eq!(tables[&2], gamma_table(2700, 256));

    // The compositor goes away and comes back (a new sway): the client
    // connects again and applies the current temperature.
    let sizes = compositor.sizes.clone();
    drop(compositor);
    sink.send(2000).unwrap();
    let mut compositor = FakeCompositor::accept(&setup.listener);
    compositor.sizes = sizes;
    let tables = compositor.tables(&[1, 2]);
    assert_eq!(tables[&1], gamma_table(2000, 256));
    assert_eq!(tables[&2], gamma_table(2000, 256));
}
