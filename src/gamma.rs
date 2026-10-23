//! Applying Night Light gamma tables through the compositor.
//!
//! A Wayland client that binds every `wl_output` and the wlr-gamma-control
//! manager, and sets a gamma table on each output's `zwlr_gamma_control_v1`.
//!
//! One gamma control is held per output for as long as the connection
//! lives: destroying a control makes the compositor restore the output's
//! previous tables, and only one client may hold the control of an output.
//! When Night Light turns off the identity table is therefore written
//! explicitly rather than by dropping the control. When the daemon exits,
//! the connection closes and the compositor restores the outputs' tables.

use crate::night_light::{gamma_table, NEUTRAL_TEMPERATURE};
use log::{debug, info, warn};
use rustix::fs::{fcntl_add_seals, memfd_create, MemfdFlags, SealFlags};
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::io::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::watch;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::{
    self, ZwlrGammaControlV1,
};

/// How long to wait before connecting again when there is no compositor.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// `wl_output` version 3 adds `release`; nothing newer is needed.
const OUTPUT_VERSION: u32 = 3;

struct Output {
    output: WlOutput,
    control: Option<ZwlrGammaControlV1>,
    gamma_size: Option<usize>,
    /// The temperature whose table the control last set; `None` while the
    /// control has set nothing, so the output still has its own tables.
    applied: Option<u32>,
    /// The compositor refused a control for this output.
    failed: bool,
    /// How many times it did, to warn only once.
    refusals: u32,
}

impl Output {
    fn release_control(&mut self) {
        self.gamma_size = None;
        self.applied = None;
        if let Some(control) = self.control.take() {
            control.destroy();
        }
    }
}

/// The state of one compositor connection.
struct Gamma {
    /// Registry name and proxy of the gamma control manager.
    manager: Option<(u32, ZwlrGammaControlManagerV1)>,
    /// Outputs by registry name.
    outputs: HashMap<u32, Output>,
    temperature: u32,
}

impl Gamma {
    /// Take the gamma control of the output `name` if it has none.
    fn request_control(&mut self, name: u32, qh: &QueueHandle<Self>) {
        let (Some((_, manager)), Some(output)) = (&self.manager, self.outputs.get_mut(&name))
        else {
            return;
        };
        if output.control.is_some() || output.failed {
            return;
        }
        output.control = Some(manager.get_gamma_control(&output.output, qh, name));
        output.gamma_size = None;
        output.applied = None;
    }

    fn request_controls(&mut self, qh: &QueueHandle<Self>) {
        let names: Vec<u32> = self.outputs.keys().copied().collect();
        for name in names {
            self.request_control(name, qh);
        }
    }

    /// Bring the output `name` in line with the current temperature.
    fn apply_output(&mut self, name: u32) {
        let temperature = self.temperature.min(NEUTRAL_TEMPERATURE);
        let Some(output) = self.outputs.get_mut(&name) else {
            return;
        };
        let (Some(control), Some(size)) = (&output.control, output.gamma_size) else {
            return;
        };
        let up_to_date = match output.applied {
            Some(applied) => applied == temperature,
            // Nothing set yet: the output shows its own colours.
            None => temperature == NEUTRAL_TEMPERATURE,
        };
        if up_to_date || size == 0 {
            return;
        }
        match table_file(&gamma_table(temperature, size)) {
            Ok(file) => {
                control.set_gamma(file.as_fd());
                output.applied = Some(temperature);
                debug!("Set a {temperature} K gamma table of {size} entries on output {name}");
            }
            Err(e) => warn!("Cannot write the gamma table for output {name}: {e}"),
        }
    }

    fn set_temperature(&mut self, temperature: u32, qh: &QueueHandle<Self>) {
        if self.temperature >= NEUTRAL_TEMPERATURE && temperature < NEUTRAL_TEMPERATURE {
            // Night Light is turning on: try again on outputs whose control
            // was refused (e.g. another gamma tool has quit since).
            for output in self.outputs.values_mut() {
                output.failed = false;
            }
            self.request_controls(qh);
        }
        self.temperature = temperature;
        let names: Vec<u32> = self.outputs.keys().copied().collect();
        for name in names {
            self.apply_output(name);
        }
    }
}

impl Dispatch<WlRegistry, ()> for Gamma {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_output" if !state.outputs.contains_key(&name) => {
                    let output = registry.bind(name, version.min(OUTPUT_VERSION), qh, ());
                    state.outputs.insert(
                        name,
                        Output {
                            output,
                            control: None,
                            gamma_size: None,
                            applied: None,
                            failed: false,
                            refusals: 0,
                        },
                    );
                    state.request_control(name, qh);
                }
                "zwlr_gamma_control_manager_v1" if state.manager.is_none() => {
                    state.manager = Some((name, registry.bind(name, 1, qh, ())));
                    state.request_controls(qh);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(mut output) = state.outputs.remove(&name) {
                    output.release_control();
                    if output.output.version() >= 3 {
                        output.output.release();
                    }
                    debug!("Output {name} went away");
                } else if state.manager.as_ref().map(|(n, _)| *n) == Some(name) {
                    // Without a manager the controls are inert; drop them.
                    for output in state.outputs.values_mut() {
                        output.release_control();
                    }
                    if let Some((_, manager)) = state.manager.take() {
                        manager.destroy();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrGammaControlV1, u32> for Gamma {
    fn event(
        state: &mut Self,
        control: &ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let name = *name;
        let Some(output) = state.outputs.get_mut(&name) else {
            return;
        };
        if output.control.as_ref() != Some(control) {
            return;
        }
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                debug!("Output {name} has gamma ramps of {size} entries");
                output.gamma_size = Some(size as usize);
                output.applied = None;
                state.apply_output(name);
            }
            zwlr_gamma_control_v1::Event::Failed => {
                output.release_control();
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
            _ => {}
        }
    }
}

/// The reply to the `sync` sent after `get_registry`: every global the
/// compositor had has been announced by then.
impl Dispatch<WlCallback, ()> for Gamma {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if state.manager.is_none() {
                info!(
                    "The compositor does not offer zwlr_gamma_control_manager_v1; \
                     Night Light cannot be applied"
                );
            }
        }
    }
}

delegate_noop!(Gamma: ignore WlOutput);
delegate_noop!(Gamma: ZwlrGammaControlManagerV1);

/// A sealed memory file holding `table`, ready to be passed to the
/// compositor.
fn table_file(table: &[u16]) -> io::Result<File> {
    let fd = memfd_create(
        "regolith-displayd-gamma",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    let mut file = File::from(fd);
    let bytes: Vec<u8> = table.iter().flat_map(|v| v.to_ne_bytes()).collect();
    file.write_all(&bytes)?;
    file.seek(SeekFrom::Start(0))?;
    let seals = SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL;
    if let Err(e) = fcntl_add_seals(&file, seals) {
        debug!("Cannot seal the gamma table: {e}");
    }
    Ok(file)
}

/// The compositor socket, found as libwayland finds it.
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

fn connect() -> Result<Connection, Box<dyn Error>> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Connection::from_socket(stream)?)
}

fn would_block(e: &WaylandError) -> bool {
    matches!(e, WaylandError::Io(e) if e.kind() == io::ErrorKind::WouldBlock)
}

/// Apply every temperature from `temperature` on one connection. Returns
/// `Ok` when the sender is gone, and an error when the connection fails.
async fn serve(
    connection: Connection,
    temperature: &mut watch::Receiver<u32>,
) -> Result<(), Box<dyn Error>> {
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let display = connection.display();
    display.get_registry(&qh, ());
    display.sync(&qh, ());
    let mut state = Gamma {
        manager: None,
        outputs: HashMap::new(),
        temperature: *temperature.borrow_and_update(),
    };
    let socket = AsyncFd::new(connection.backend().poll_fd().as_raw_fd())?;
    loop {
        queue.dispatch_pending(&mut state)?;
        match connection.flush() {
            Err(e) if !would_block(&e) => return Err(e.into()),
            _ => {}
        }
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        tokio::select! {
            ready = socket.readable() => {
                let mut ready = ready?;
                match guard.read() {
                    Ok(_) => {}
                    Err(e) if would_block(&e) => ready.clear_ready(),
                    Err(e) => return Err(e.into()),
                }
            }
            changed = temperature.changed() => {
                drop(guard);
                if changed.is_err() {
                    return Ok(());
                }
                let t = *temperature.borrow_and_update();
                state.set_temperature(t, &qh);
            }
        }
    }
}

/// Keep a compositor connection (reconnecting while there is none) and
/// apply each temperature received on `temperature` to every output.
/// Returns when the sender is gone.
pub async fn run(mut temperature: watch::Receiver<u32>) {
    let mut warned = false;
    loop {
        match connect() {
            Ok(connection) => {
                info!("Connected to the compositor for Night Light");
                warned = false;
                match serve(connection, &mut temperature).await {
                    Ok(()) => return,
                    Err(e) => warn!("Lost the compositor connection for Night Light: {e}"),
                }
            }
            Err(e) if warned => debug!("Cannot connect to the compositor: {e}"),
            Err(e) => {
                warn!("Night Light cannot reach the compositor (will retry): {e}");
                warned = true;
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn tables_are_native_endian_ramps() {
        let mut file = table_file(&gamma_table(NEUTRAL_TEMPERATURE, 4)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        let values: Vec<u16> = bytes
            .chunks(2)
            .map(|b| u16::from_ne_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(values, [0, 21845, 43690, 65535].repeat(3));
    }
}
