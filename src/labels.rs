//! Monitor number labels for Settings › Displays.
//!
//! While its window is active, the Displays panel asks GNOME Shell to show
//! each monitor's number in a corner of that monitor (`ShowMonitorLabels` of
//! `org.gnome.Shell`), so the monitors on the screen can be matched with the
//! numbered monitors in the panel, and to hide them again
//! (`HideMonitorLabels`) when the window is no longer active.
//!
//! In the sway session, regolith-powerd owns `org.gnome.Shell` (it also
//! answers gnome-session's end-session dialog there) and passes these two
//! calls on to this daemon, which draws the labels as wlr-layer-shell
//! surfaces in the top left corner of each monitor. This daemon serves them
//! with the same two methods and arguments, on its own bus name
//! (`org.gnome.Mutter.DisplayConfig`), at [`PATH`] with the interface
//! [`INTERFACE`]. That interface is internal to Regolith and may change with
//! regolith-powerd.
//!
//! As in GNOME Shell, the labels belong to the client that showed them: only
//! that client can hide them or show others, and they are hidden when it
//! leaves the bus. Here that client is regolith-powerd, which applies the
//! same rule to the panels that call it, so a panel or a powerd that exits
//! does not leave the labels behind.
//!
//! The labels have their own compositor connection, opened when they are
//! shown and closed when they are hidden or change: closing it removes
//! every surface. It runs on its own thread, so a missing or stuck
//! compositor cannot disturb the `DisplayConfig` service.

use log::{debug, info, warn};
use rustix::fs::{memfd_create, MemfdFlags};
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::io::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::unix::AsyncFd;
use tokio::sync::watch;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_display::WlDisplay;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
    Layer, ZwlrLayerShellV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};
use zbus::export::futures_util::StreamExt;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;
use zbus::{dbus_interface, MessageHeader, SignalContext};
use zvariant::{OwnedValue, Value};

/// A label: the connector of the monitor and the number to show on it.
pub type Label = (String, i32);

/// Height of a label, in logical pixels.
const LABEL_HEIGHT: u32 = 72;
/// Height of a digit, in logical pixels.
const DIGIT_HEIGHT: f64 = 40.0;
/// Distance from the top and left edges of the monitor, in logical pixels.
const MARGIN: i32 = 12;
const CORNER_RADIUS: f64 = 12.0;
/// Label background, as in GNOME's on-screen displays (RGBA).
const BACKGROUND: [f64; 4] = [0.14, 0.14, 0.14, 0.92];

/// The labels to show for the `params` of `ShowMonitorLabels`: connector
/// names mapped to the monitor numbers (int32), sorted by number.
pub fn labels_of(params: &HashMap<String, OwnedValue>) -> Vec<Label> {
    let mut labels: Vec<Label> = params
        .iter()
        .filter_map(|(connector, value)| {
            let number = match **value {
                Value::I32(n) => n,
                Value::U32(n) => i32::try_from(n).ok()?,
                _ => {
                    warn!("Monitor label for {connector} is not a number: {value:?}");
                    return None;
                }
            };
            Some((connector.clone(), number))
        })
        .collect();
    labels.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    labels
}

/// Where the labels are served, on the bus name of the `DisplayConfig`
/// service.
pub const PATH: &str = "/org/regolith/displayd/MonitorLabels";
/// The interface the labels are served with. It has the two methods of
/// `org.gnome.Shell` that regolith-powerd passes on.
pub const INTERFACE: &str = "org.regolith.displayd.MonitorLabels";

/// The monitor labels, for the client that shows them.
pub struct MonitorLabels {
    /// The labels to show, if any.
    labels: Arc<watch::Sender<Option<Vec<Label>>>>,
    /// The client whose labels are shown, if any.
    client: Arc<Mutex<Option<Holder>>>,
    /// Numbers the times a client takes the labels.
    holds: AtomicU64,
}

/// The client whose labels are shown.
#[derive(Clone, PartialEq)]
struct Holder {
    /// Its unique bus name.
    name: String,
    /// Which time a client took the labels.
    hold: u64,
}

#[dbus_interface(name = "org.regolith.displayd.MonitorLabels")]
impl MonitorLabels {
    /// Show the labels `params` asks for (connector names mapped to int32
    /// numbers), as `org.gnome.Shell.ShowMonitorLabels` does. Ignored while
    /// another client's labels are shown.
    pub async fn show_monitor_labels(
        &self,
        #[zbus(header)] header: MessageHeader<'_>,
        #[zbus(signal_context)] ctxt: SignalContext<'_>,
        params: HashMap<String, OwnedValue>,
    ) {
        let labels = labels_of(&params);
        let Some(sender) = header.sender().ok().flatten().map(|s| s.to_string()) else {
            return;
        };
        let new_holder = {
            let mut client = self.client.lock().unwrap();
            match &*client {
                Some(current) if current.name != sender => {
                    debug!(
                        "Ignoring ShowMonitorLabels from {sender}: the labels are {}'s",
                        current.name
                    );
                    return;
                }
                Some(_) => None,
                None => {
                    let holder = Holder {
                        name: sender.clone(),
                        hold: self.holds.fetch_add(1, Ordering::Relaxed),
                    };
                    *client = Some(holder.clone());
                    Some(holder)
                }
            }
        };
        info!("Showing monitor labels {labels:?} for {sender}");
        self.show(Some(labels));
        if let Some(holder) = new_holder {
            self.hide_when_gone(ctxt.connection().clone(), holder);
        }
    }

    /// Hide the labels, as `org.gnome.Shell.HideMonitorLabels` does. Only
    /// the client that showed them can hide them.
    pub async fn hide_monitor_labels(&self, #[zbus(header)] header: MessageHeader<'_>) {
        let sender = header.sender().ok().flatten().map(|s| s.to_string());
        {
            let mut client = self.client.lock().unwrap();
            let holder = client.as_ref().map(|c| c.name.clone());
            if holder.is_none() || holder != sender {
                debug!("Ignoring HideMonitorLabels from {sender:?}: the labels are {holder:?}'s");
                return;
            }
            *client = None;
        }
        info!("Hiding monitor labels");
        self.show(None);
    }
}

impl MonitorLabels {
    /// Start the thread that draws the labels.
    pub fn new() -> MonitorLabels {
        let (labels, wanted) = watch::channel(None);
        let spawned = std::thread::Builder::new()
            .name("monitor-labels".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(run(wanted)),
                    Err(e) => warn!("Monitor labels are unavailable: {e}"),
                }
            });
        if let Err(e) = spawned {
            warn!("Monitor labels are unavailable: cannot start their thread: {e}");
        }
        MonitorLabels {
            labels: Arc::new(labels),
            client: Arc::new(Mutex::new(None)),
            holds: AtomicU64::new(0),
        }
    }

    /// Show `labels`, or none.
    fn show(&self, labels: Option<Vec<Label>>) {
        self.labels.send_if_modified(|current| {
            if *current == labels {
                return false;
            }
            *current = labels;
            true
        });
    }

    /// Hide the labels when `holder` leaves the bus, if they are still its.
    /// Stops watching once it hid them or another hold began.
    fn hide_when_gone(&self, connection: zbus::Connection, holder: Holder) {
        let labels = Arc::clone(&self.labels);
        let current = Arc::clone(&self.client);
        tokio::spawn(async move {
            let holds = || current.lock().unwrap().as_ref() == Some(&holder);
            let hide = || {
                let mut current = current.lock().unwrap();
                if current.as_ref() == Some(&holder) {
                    info!("Hiding monitor labels: {} left the bus", holder.name);
                    *current = None;
                    labels.send_replace(None);
                }
            };
            let Ok(proxy) = DBusProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changes) = proxy.receive_name_owner_changed().await else {
                return;
            };
            let Ok(name) = BusName::try_from(holder.name.as_str()) else {
                return;
            };
            // It may have left before the stream was set up.
            if !proxy.name_has_owner(name).await.unwrap_or(true) {
                hide();
                return;
            }
            while let Some(change) = changes.next().await {
                if !holds() {
                    return;
                }
                let Ok(args) = change.args() else {
                    continue;
                };
                if args.name().as_str() == holder.name && args.new_owner().is_none() {
                    hide();
                    return;
                }
            }
        });
    }
}

impl Default for MonitorLabels {
    fn default() -> Self {
        Self::new()
    }
}

/// Show the labels `wanted` asks for, each time on a new connection.
async fn run(mut wanted: watch::Receiver<Option<Vec<Label>>>) {
    loop {
        let labels = wanted.borrow_and_update().clone();
        if let Some(labels) = labels {
            let shown = match connect() {
                Ok(connection) => serve(connection, labels, &mut wanted).await,
                Err(e) => Err(e),
            };
            match shown {
                // The labels changed or were hidden.
                Ok(()) => continue,
                Err(e) => warn!("Cannot show the monitor labels: {e}"),
            }
        }
        if wanted.changed().await.is_err() {
            return;
        }
    }
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

fn connect() -> Result<Connection, Box<dyn Error>> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Connection::from_socket(stream)?)
}

fn would_block(e: &WaylandError) -> bool {
    matches!(e, WaylandError::Io(e) if e.kind() == io::ErrorKind::WouldBlock)
}

/// Put `labels` on the screen and keep them there until `wanted` changes.
/// Returning drops the connection, and with it the labels.
async fn serve(
    connection: Connection,
    labels: Vec<Label>,
    wanted: &mut watch::Receiver<Option<Vec<Label>>>,
) -> Result<(), Box<dyn Error>> {
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let display = connection.display();
    let registry = display.get_registry(&qh, ());
    display.sync(&qh, Step::Globals);
    let mut state = Labels {
        labels,
        registry: Some(registry),
        ..Default::default()
    };
    let socket = AsyncFd::new(connection.backend().poll_fd().as_raw_fd())?;
    loop {
        queue.dispatch_pending(&mut state)?;
        if let Some(e) = state.error.take() {
            return Err(e.into());
        }
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
            _ = wanted.changed() => return Ok(()),
        }
    }
}

/// What a `wl_display.sync` waits for.
#[derive(Clone, Copy)]
enum Step {
    /// The globals have been announced.
    Globals,
    /// The outputs have sent their names and scales.
    Outputs,
}

struct Output {
    output: WlOutput,
    name: Option<String>,
    scale: i32,
}

struct Surface {
    surface: WlSurface,
    text: String,
    scale: i32,
    drawn: bool,
}

#[derive(Default)]
struct Labels {
    labels: Vec<Label>,
    registry: Option<WlRegistry>,
    /// The globals: name, interface and version.
    globals: Vec<(u32, String, u32)>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    outputs: Vec<Output>,
    surfaces: Vec<Surface>,
    error: Option<String>,
}

impl Labels {
    fn global(&self, interface: &str) -> Option<(u32, u32)> {
        self.globals
            .iter()
            .find(|(_, i, _)| i == interface)
            .map(|(name, _, version)| (*name, *version))
    }

    /// Bind the globals the labels need, and ask the outputs for their
    /// names: `wl_output.name` since version 4, or else xdg-output's name
    /// (sway 1.7).
    fn bind(&mut self, display: &WlDisplay, qh: &QueueHandle<Self>) {
        let Some(registry) = self.registry.clone() else {
            return;
        };
        let bind = |interface: &str, max: u32| {
            self.global(interface)
                .map(|(name, version)| (name, version.min(max)))
                .ok_or_else(|| format!("the compositor has no {interface}"))
        };
        let needed = (|| {
            Ok::<_, String>((
                bind("wl_compositor", 4)?,
                bind("wl_shm", 1)?,
                bind("zwlr_layer_shell_v1", 4)?,
            ))
        })();
        let ((compositor, compositor_version), (shm, _), (layer_shell, layer_shell_version)) =
            match needed {
                Ok(needed) => needed,
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            };
        self.compositor = Some(registry.bind(compositor, compositor_version, qh, ()));
        self.shm = Some(registry.bind(shm, 1, qh, ()));
        self.layer_shell = Some(registry.bind(layer_shell, layer_shell_version, qh, ()));
        let xdg_output_manager: Option<ZxdgOutputManagerV1> = self
            .global("zxdg_output_manager_v1")
            .filter(|(_, version)| *version >= 2)
            .map(|(name, version)| registry.bind(name, version.min(3), qh, ()));
        let outputs: Vec<(u32, u32)> = self
            .globals
            .iter()
            .filter(|(_, interface, _)| interface == "wl_output")
            .map(|(name, _, version)| (*name, *version))
            .collect();
        for (index, (name, version)) in outputs.into_iter().enumerate() {
            let output: WlOutput = registry.bind(name, version.min(4), qh, index);
            if output.version() < 4 {
                if let Some(manager) = &xdg_output_manager {
                    manager.get_xdg_output(&output, qh, index);
                }
            }
            self.outputs.push(Output {
                output,
                name: None,
                scale: 1,
            });
        }
        display.sync(qh, Step::Outputs);
    }

    /// Create a layer surface for each label on its output. Each is drawn
    /// once the compositor has configured it.
    fn create_surfaces(&mut self, qh: &QueueHandle<Self>) {
        let (Some(compositor), Some(layer_shell)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        for (connector, number) in &self.labels {
            let Some(output) = self
                .outputs
                .iter()
                .find(|o| o.name.as_deref() == Some(connector))
            else {
                warn!("No output {connector} for monitor label {number}");
                continue;
            };
            let text = number.to_string();
            let (width, height) = label_size(&text);
            let scale = if compositor.version() >= 3 {
                output.scale
            } else {
                1
            };
            let surface = compositor.create_surface(qh, ());
            // An empty input region: the label lets clicks through.
            let region = compositor.create_region(qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
            if compositor.version() >= 3 {
                surface.set_buffer_scale(scale);
            }
            let layer_surface = layer_shell.get_layer_surface(
                &surface,
                Some(&output.output),
                Layer::Overlay,
                "regolith-monitor-label".into(),
                qh,
                self.surfaces.len(),
            );
            layer_surface.set_size(width, height);
            layer_surface.set_anchor(Anchor::Top | Anchor::Left);
            layer_surface.set_margin(MARGIN, 0, 0, MARGIN);
            layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
            surface.commit();
            self.surfaces.push(Surface {
                surface,
                text,
                scale,
                drawn: false,
            });
        }
    }

    fn draw(&mut self, index: usize, qh: &QueueHandle<Self>) {
        let Some(shm) = &self.shm else {
            return;
        };
        let Some(label) = self.surfaces.get_mut(index) else {
            return;
        };
        if label.drawn {
            label.surface.commit();
            return;
        }
        let (width, height, pixels) = render(&label.text, label.scale as u32);
        let file = match pixel_file(&pixels) {
            Ok(file) => file,
            Err(e) => {
                self.error = Some(format!("cannot share the label image: {e}"));
                return;
            }
        };
        let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            width as i32 * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        let scale = label.scale as u32;
        label.surface.attach(Some(&buffer), 0, 0);
        label
            .surface
            .damage(0, 0, (width / scale) as i32, (height / scale) as i32);
        label.surface.commit();
        label.drawn = true;
    }
}

/// A file holding the pixels of a label, to share with the compositor.
fn pixel_file(pixels: &[u8]) -> io::Result<File> {
    let fd = memfd_create("regolith-displayd-label", MemfdFlags::CLOEXEC)?;
    let mut file = File::from(fd);
    file.write_all(pixels)?;
    Ok(file)
}

impl Dispatch<WlRegistry, ()> for Labels {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<WlCallback, Step> for Labels {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        step: &Step,
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            match step {
                Step::Globals => state.bind(&connection.display(), qh),
                Step::Outputs => state.create_surfaces(qh),
            }
        }
    }
}

impl Dispatch<WlOutput, usize> for Labels {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => output.name = Some(name),
            wl_output::Event::Scale { factor } => output.scale = factor.max(1),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for Labels {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let (Some(output), zxdg_output_v1::Event::Name { name }) =
            (state.outputs.get_mut(*index), event)
        {
            output.name.get_or_insert(name);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for Labels {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            layer_surface.ack_configure(serial);
            state.draw(*index, qh);
        }
    }
}

delegate_noop!(Labels: WlCompositor);
delegate_noop!(Labels: WlRegion);
delegate_noop!(Labels: WlShmPool);
delegate_noop!(Labels: ZwlrLayerShellV1);
delegate_noop!(Labels: ZxdgOutputManagerV1);
delegate_noop!(Labels: ignore WlShm);
delegate_noop!(Labels: ignore WlBuffer);
delegate_noop!(Labels: ignore WlSurface);

/// The size of the label for `text`, in logical pixels.
pub fn label_size(text: &str) -> (u32, u32) {
    let unit = DIGIT_HEIGHT / GLYPH_HEIGHT;
    let digits = text.chars().filter(char::is_ascii_digit).count().max(1) as f64;
    let text_width = (digits * GLYPH_WIDTH + (digits - 1.0) * GLYPH_GAP) * unit;
    let width = (text_width + LABEL_HEIGHT as f64 - DIGIT_HEIGHT).ceil() as u32;
    (width.max(LABEL_HEIGHT), LABEL_HEIGHT)
}

/// Draw the label for `text` at `scale`: its width and height in pixels
/// and its pixels, premultiplied ARGB8888 in memory order (B, G, R, A).
pub fn render(text: &str, scale: u32) -> (u32, u32, Vec<u8>) {
    let scale = scale.max(1);
    let (logical_width, logical_height) = label_size(text);
    let (width, height) = (logical_width * scale, logical_height * scale);
    let s = scale as f64;
    let unit = DIGIT_HEIGHT / GLYPH_HEIGHT * s;
    let half_stroke = STROKE / 2.0 * unit;

    // The digits' strokes in pixels, centred in the label.
    let digits: Vec<char> = text.chars().filter(char::is_ascii_digit).collect();
    let count = digits.len() as f64;
    let text_width = (count * GLYPH_WIDTH + (count - 1.0).max(0.0) * GLYPH_GAP) * unit;
    let left = (width as f64 - text_width) / 2.0;
    let top = (height as f64 - GLYPH_HEIGHT * unit) / 2.0;
    let mut segments = Vec::new();
    for (i, digit) in digits.iter().enumerate() {
        let x0 = left + i as f64 * (GLYPH_WIDTH + GLYPH_GAP) * unit;
        for stroke in glyph(*digit) {
            for pair in stroke.windows(2) {
                let point = |(x, y): (f64, f64)| (x0 + x * unit, top + y * unit);
                segments.push((point(pair[0]), point(pair[1])));
            }
        }
    }

    let radius = CORNER_RADIUS * s;
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let p = (x as f64 + 0.5, y as f64 + 0.5);
            let box_coverage = (0.5
                - rounded_rect_distance(p, width as f64, height as f64, radius))
            .clamp(0.0, 1.0);
            let ink = segments
                .iter()
                .map(|&(a, b)| segment_distance(p, a, b))
                .fold(f64::INFINITY, f64::min);
            let ink_coverage = (half_stroke + 0.5 - ink).clamp(0.0, 1.0);
            // White ink over the background, both cut by the rounded box.
            let alpha = box_coverage * (ink_coverage + BACKGROUND[3] * (1.0 - ink_coverage));
            let channel = |c: f64| {
                let value =
                    box_coverage * (ink_coverage + c * BACKGROUND[3] * (1.0 - ink_coverage));
                (value * 255.0).round() as u8
            };
            pixels.extend_from_slice(&[
                channel(BACKGROUND[2]),
                channel(BACKGROUND[1]),
                channel(BACKGROUND[0]),
                (alpha * 255.0).round() as u8,
            ]);
        }
    }
    (width, height, pixels)
}

/// Signed distance from `p` to the edge of a `width` x `height` rectangle
/// at the origin with corners of `radius`: negative inside.
fn rounded_rect_distance(p: (f64, f64), width: f64, height: f64, radius: f64) -> f64 {
    let qx = (p.0 - width / 2.0).abs() - (width / 2.0 - radius);
    let qy = (p.1 - height / 2.0).abs() - (height / 2.0 - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

fn segment_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

// The digits are drawn as strokes in a GLYPH_WIDTH x GLYPH_HEIGHT box
// (y down), so no font is needed.
const GLYPH_WIDTH: f64 = 10.0;
const GLYPH_HEIGHT: f64 = 16.0;
const GLYPH_GAP: f64 = 3.0;
const STROKE: f64 = 2.5;

/// Points on an elliptic arc around (`cx`, `cy`) from angle `from` to `to`,
/// in degrees counterclockwise from 3 o'clock.
fn arc(cx: f64, cy: f64, rx: f64, ry: f64, from: f64, to: f64) -> Vec<(f64, f64)> {
    let steps = ((to - from).abs() / 10.0).ceil().max(1.0) as usize;
    (0..=steps)
        .map(|i| {
            let angle = (from + (to - from) * i as f64 / steps as f64).to_radians();
            (cx + rx * angle.cos(), cy - ry * angle.sin())
        })
        .collect()
}

fn chain(parts: &[Vec<(f64, f64)>]) -> Vec<(f64, f64)> {
    parts.concat()
}

/// The strokes of `digit`.
fn glyph(digit: char) -> Vec<Vec<(f64, f64)>> {
    match digit {
        '0' => vec![arc(5.0, 8.0, 3.6, 6.2, 0.0, 360.0)],
        '1' => vec![vec![(2.6, 4.0), (5.8, 1.8), (5.8, 14.2)]],
        '2' => vec![chain(&[
            arc(5.0, 5.1, 3.5, 3.3, 165.0, -35.0),
            vec![(1.5, 14.2), (8.6, 14.2)],
        ])],
        '3' => vec![
            arc(5.0, 5.0, 3.3, 3.2, 155.0, -90.0),
            arc(5.0, 11.0, 3.7, 3.2, 90.0, -155.0),
        ],
        '4' => vec![vec![(7.0, 14.2), (7.0, 1.8), (1.2, 10.6), (9.0, 10.6)]],
        '5' => vec![chain(&[
            vec![(8.2, 1.8), (2.6, 1.8), (2.2, 7.7)],
            arc(5.0, 10.3, 3.7, 3.9, 140.0, -145.0),
        ])],
        '6' => vec![
            arc(5.0, 10.4, 3.6, 3.8, 0.0, 360.0),
            arc(9.6, 10.4, 8.2, 8.6, 100.0, 180.0),
        ],
        '7' => vec![vec![(1.4, 1.8), (8.6, 1.8), (3.8, 14.2)]],
        '8' => vec![
            arc(5.0, 4.8, 3.1, 3.0, 0.0, 360.0),
            arc(5.0, 11.0, 3.6, 3.2, 0.0, 360.0),
        ],
        '9' => glyph('6')
            .into_iter()
            .map(|stroke| {
                stroke
                    .into_iter()
                    .map(|(x, y)| (GLYPH_WIDTH - x, GLYPH_HEIGHT - y))
                    .collect()
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(image: &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
        let i = ((y * image.0 + x) * 4) as usize;
        image.2[i..i + 4].try_into().unwrap()
    }

    /// The pixels of the digits: (nearly) white and opaque.
    fn ink(image: &(u32, u32, Vec<u8>)) -> Vec<bool> {
        image
            .2
            .chunks(4)
            .map(|p| p.iter().all(|&c| c > 240))
            .collect()
    }

    #[test]
    fn labels_are_read_from_the_panel_parameters_in_number_order() {
        let mut params = HashMap::new();
        params.insert("HDMI-A-1".to_string(), OwnedValue::from(2i32));
        params.insert("eDP-1".to_string(), OwnedValue::from(1i32));
        params.insert("DP-3".to_string(), OwnedValue::from(3u32));
        params.insert("DP-9".to_string(), OwnedValue::from(Value::from("x")));
        assert_eq!(
            labels_of(&params),
            [
                ("eDP-1".to_string(), 1),
                ("HDMI-A-1".to_string(), 2),
                ("DP-3".to_string(), 3)
            ]
        );
    }

    #[test]
    fn a_label_is_a_rounded_box_with_the_number_in_white() {
        let image = render("1", 1);
        assert_eq!((image.0, image.1), (LABEL_HEIGHT, LABEL_HEIGHT));
        assert_eq!(image.2.len(), (image.0 * image.1 * 4) as usize);
        // Outside the rounded corner: transparent.
        assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 0]);
        // Background near the edge: dark and nearly opaque.
        let background = pixel(&image, 4, LABEL_HEIGHT / 2);
        assert_eq!(background[3], (BACKGROUND[3] * 255.0).round() as u8);
        assert!(background[..3].iter().all(|&c| c < 40));
        // The stem of the 1, in the middle: white.
        let unit = DIGIT_HEIGHT / GLYPH_HEIGHT;
        let left = (LABEL_HEIGHT as f64 - GLYPH_WIDTH * unit) / 2.0;
        let stem = (left + 5.8 * unit) as u32;
        assert_eq!(pixel(&image, stem, LABEL_HEIGHT / 2), [255, 255, 255, 255]);
    }

    #[test]
    fn every_digit_is_drawn_and_looks_different() {
        let drawn: Vec<Vec<bool>> = ('0'..='9')
            .map(|d| ink(&render(&d.to_string(), 1)))
            .collect();
        for (i, a) in drawn.iter().enumerate() {
            let inked = a.iter().filter(|&&p| p).count();
            assert!(inked > 100, "digit {i} has {inked} inked pixels");
            for (j, b) in drawn.iter().enumerate().skip(i + 1) {
                let differ = a.iter().zip(b).filter(|(p, q)| p != q).count();
                assert!(
                    differ > 50,
                    "digits {i} and {j} differ in {differ} pixels only"
                );
            }
        }
    }

    #[test]
    fn labels_grow_with_the_number_and_the_scale() {
        assert_eq!(label_size("7"), (LABEL_HEIGHT, LABEL_HEIGHT));
        let (wide, height) = label_size("12");
        assert!(wide > LABEL_HEIGHT);
        assert_eq!(height, LABEL_HEIGHT);
        let image = render("12", 2);
        assert_eq!((image.0, image.1), (wide * 2, LABEL_HEIGHT * 2));
        assert_eq!(image.2.len(), (wide * 2 * LABEL_HEIGHT * 2 * 4) as usize);
    }
}
