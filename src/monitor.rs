use crate::edid;
use crate::modes::Modes;
use log::warn;
use num;
use num_derive::FromPrimitive;
use serde::{Deserialize, Serialize};
use std::hash::Hash;
use std::io::Write;
use std::sync::Arc;
use swayipc_async::{Connection, Output};
use tokio::sync::Mutex;
use zbus::fdo::Error::{self as ZError, Failed};
use zvariant::{DeserializeDict, SerializeDict, Type};

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Monitor {
    description: (String, String, String, String),
    modes: Vec<Modes>,
    properties: MonitorProperties,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct LogicalMonitor {
    x_pos: i32,
    y_pos: i32,
    scale: f64,
    transform: u32,
    primary: bool, // false always for wayland
    monitors: Vec<(String, String, String, String)>,
    properties: LogicalMonitorProperties,
}

#[derive(Debug, PartialEq, Eq, Clone, DeserializeDict, SerializeDict, Type, Hash)]
#[zvariant(signature = "dict")]
pub struct MonitorProperties {
    #[zvariant(rename = "width-mm")]
    width: Option<i32>,
    #[zvariant(rename = "height-mm")]
    height: Option<i32>,
    #[zvariant(rename = "is-underscanning")]
    underscanning: Option<bool>,
    #[zvariant(rename = "is-builtin")]
    builtin: Option<bool>,
    #[zvariant(rename = "max-screen-size")]
    max_size: Option<(i32, i32)>,
    #[zvariant(rename = "display-name")]
    name: Option<String>,
}

#[derive(FromPrimitive, PartialEq, Eq)]
pub enum MonitorTransform {
    Normal = 0,
    Left = 1,
    Down = 2,
    Right = 3,
    Flipped = 4,
    FlippedLeft = 5,
    FlippedDown = 6,
    FlippedRight = 7,
}

#[derive(Debug, PartialEq, Eq, Clone, DeserializeDict, SerializeDict, Type)]
#[zvariant(signature = "dict")]
pub struct LogicalMonitorProperties {
    #[zvariant(rename = "dummy")]
    dummy: Option<i32>,
    #[zvariant(rename = "dummy2")]
    dummy2: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct MonitorApply {
    x_pos: i32,
    y_pos: i32,
    scale: f64,
    transform: u32,
    primary: bool, // false always for wayland
    pub monitors: Vec<(String, String, MonitorProperties)>,
}

impl Monitor {
    pub fn new(output: &Output) -> Monitor {
        let mut output_modes: Vec<Modes> =
            output.modes.iter().map(|m| Modes::new(output, m)).collect();
        // Headless and other virtual outputs have no fixed mode list in sway,
        // but Mutter's DisplayConfig API requires every monitor to have at
        // least one mode (gnome-control-center aborts otherwise). Report the
        // current mode as the only one.
        if output_modes.is_empty() {
            if let Some(current) = &output.current_mode {
                output_modes.push(Modes::new(output, current));
            }
        }
        let description = (
            output.name.clone(),   // connector
            output.make.clone(),   // vendor
            output.model.clone(),  // product
            output.serial.clone(), // serial
        );
        Monitor {
            description,
            modes: output_modes,
            properties: MonitorProperties::new(output),
        }
    }

    /// Report the modes `known` gives for this monitor, none of them
    /// current, if sway reports none: sway reports no mode for a headless or
    /// other virtual output that is off, and without one it couldn't be
    /// switched on again.
    pub fn keep_known_modes(&mut self, known: &[Monitor]) {
        if !self.modes.is_empty() {
            return;
        }
        if let Some(previous) = known.iter().find(|monitor| monitor.same_monitor(self)) {
            self.modes = previous
                .modes
                .iter()
                .map(|mode| mode.clone().with_current(false))
                .collect();
        }
    }

    pub fn search_modes(&self, mode_id: &str) -> Option<&Modes> {
        self.modes.iter().find(|&m| m.get_id() == mode_id)
    }

    pub fn get_dpy_name(&self) -> String {
        let desc = &self.description;
        format!("{} {} {}", desc.1, desc.2, desc.3)
    }

    pub fn is_builtin(&self) -> bool {
        self.properties.builtin == Some(true)
    }

    /// The area of the current mode, in pixels.
    pub fn current_area(&self) -> i64 {
        self.modes
            .iter()
            .find(|mode| mode.current())
            .map_or(0, |mode| mode.width() as i64 * mode.height() as i64)
    }

    /// The connector the monitor is plugged into (e.g. "DP-1").
    pub fn connector(&self) -> &str {
        &self.description.0
    }

    /// How a kanshi profile names this monitor, given all the `connected`
    /// monitors: by its description ("make model serial"), which follows the
    /// monitor to any port, unless another connected monitor has the same
    /// description. Then by its connector name (e.g. "DP-1"), because kanshi
    /// rejects the whole configuration when a profile names an output twice.
    pub fn kanshi_name(&self, connected: &[Monitor]) -> String {
        let description = self.get_dpy_name();
        let same = connected
            .iter()
            .filter(|monitor| monitor.get_dpy_name() == description)
            .count();
        if same > 1 {
            self.description.0.clone()
        } else {
            description
        }
    }

    pub fn get_current_mode(&self) -> &str {
        match self.modes.iter().find(|&mode| mode.current()) {
            Some(m) => m.get_modestr(),
            None => "Unknown",
        }
    }
}

impl Monitor {
    /// The monitor's state, as the change detector compares it: the same
    /// monitor (connector, vendor, product and serial) in the same mode.
    /// Equality and hashing both use it, so that equal values also hash the
    /// same, as `HashSet` requires. [`Monitor::same_monitor`] compares the
    /// identity alone.
    fn state(&self) -> (&(String, String, String, String), &str) {
        (&self.description, self.get_current_mode())
    }

    /// Whether `other` is the same monitor, whatever its mode.
    pub fn same_monitor(&self, other: &Monitor) -> bool {
        self.description == other.description
    }
}

impl PartialEq for Monitor {
    fn eq(&self, other: &Self) -> bool {
        self.state() == other.state()
    }
}

impl Eq for Monitor {}

impl Hash for Monitor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.state().hash(state);
    }
}

/// What `LogicalMonitor::state` returns: x, y, scale bits, transform and the
/// monitors shown.
type LogicalMonitorState<'a> = (i32, i32, u64, u32, &'a [(String, String, String, String)]);

impl LogicalMonitor {
    /// The logical monitor's state: where it is, its scale and rotation, and
    /// the monitors it shows on. Equality and hashing both use it. The scale
    /// is compared by its bits, so that equal values always hash the same.
    fn state(&self) -> LogicalMonitorState<'_> {
        (
            self.x_pos,
            self.y_pos,
            self.scale.to_bits(),
            self.transform,
            &self.monitors,
        )
    }
}

impl PartialEq for LogicalMonitor {
    fn eq(&self, other: &Self) -> bool {
        self.state() == other.state()
    }
}

impl Eq for LogicalMonitor {}

impl Hash for LogicalMonitor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.state().hash(state);
    }
}

impl MonitorProperties {
    pub fn new(output: &Output) -> MonitorProperties {
        MonitorProperties::with_physical_size(output, edid::physical_size_mm(&output.name))
    }

    /// `physical` is the monitor's size in millimetres, when known. Like
    /// Mutter, GetCurrentState gives it only through the display name:
    /// gnome-control-center appends the diagonal from "width-mm" and
    /// "height-mm" to the name when they are set, so they stay unset.
    /// (They used to carry `output.rect`, the logical size in pixels, which
    /// made a 27" monitor show as 116", or 58" at scale 2.)
    fn with_physical_size(output: &Output, physical: Option<(i32, i32)>) -> MonitorProperties {
        let builtin = is_laptop_panel(&output.name);
        MonitorProperties {
            width: None,
            height: None,
            name: Some(display_name(&output.make, builtin, physical)),
            builtin: Some(builtin),
            max_size: None,
            underscanning: None,
        }
    }
}

/// Connector types of laptop panels, as Mutter tells them apart.
fn is_laptop_panel(connector: &str) -> bool {
    ["eDP", "LVDS", "DSI"]
        .iter()
        .any(|t| connector.starts_with(t))
}

/// The name Mutter gives a monitor (`meta_monitor_make_display_name`):
/// "Built-in display" for a laptop panel, otherwise the vendor followed by
/// the diagonal in inches when the physical size is known. sway's `make` is
/// already the vendor name looked up from the EDID's PNP ID.
fn display_name(make: &str, builtin: bool, physical: Option<(i32, i32)>) -> String {
    if builtin {
        return String::from("Built-in display");
    }
    let inches = physical.map(|(w, h)| diagonal_to_str((f64::from(w).hypot(f64::from(h))) / 25.4));
    let vendor = match make.trim() {
        "" | "Unknown" | "unknown" => match inches {
            Some(_) => "Unknown",
            None => "Unknown Display",
        },
        vendor => vendor,
    };
    match inches {
        Some(inches) => format!("{vendor} {inches}"),
        None => vendor.to_string(),
    }
}

/// Mutter's `diagonal_to_str`: common laptop sizes keep their decimal,
/// anything else is rounded to whole inches.
fn diagonal_to_str(inches: f64) -> String {
    for known in [12.1, 13.3, 15.6] {
        if (known - inches).abs() < 0.1 {
            return format!("{known:.1}\"");
        }
    }
    format!("{}\"", (inches + 0.5) as i32)
}

impl MonitorTransform {
    pub fn from_u32(transform: u32) -> Option<MonitorTransform> {
        num::FromPrimitive::from_u32(transform)
    }
    pub fn from_sway(sway_transform: &Option<String>) -> MonitorTransform {
        match sway_transform {
            Some(str) => match str.as_str() {
                "90" => MonitorTransform::Left,
                "180" => MonitorTransform::Down,
                "270" => MonitorTransform::Right,
                "flipped" => MonitorTransform::Flipped,
                "flipped-90" => MonitorTransform::FlippedLeft,
                "flipped-180" => MonitorTransform::FlippedDown,
                "flipped-270" => MonitorTransform::FlippedRight,
                _ => MonitorTransform::Normal,
            },
            _ => MonitorTransform::Normal,
        }
    }

    /// The transform as a kanshi profile names it. kanshi sets it through
    /// wlr-output-management, which names the quarter turns the other way
    /// round from sway's IPC: kanshi's "270" is what sway reports as "90".
    pub fn to_kanshi(self) -> &'static str {
        use MonitorTransform::*;
        match self {
            Normal => "normal",
            Right => "90",
            Down => "180",
            Left => "270",
            Flipped => "flipped",
            FlippedRight => "flipped-90",
            FlippedDown => "flipped-180",
            FlippedLeft => "flipped-270",
        }
    }

    /// The transform as sway's IPC names it, in `get_outputs` and in the
    /// `output ... transform` command: the inverse of [`Self::from_sway`].
    pub fn to_sway(self) -> &'static str {
        use MonitorTransform::*;
        match self {
            Normal => "normal",
            Left => "90",
            Down => "180",
            Right => "270",
            Flipped => "flipped",
            FlippedLeft => "flipped-90",
            FlippedDown => "flipped-180",
            FlippedRight => "flipped-270",
        }
    }
}

impl LogicalMonitor {
    pub fn new(output: &Output) -> LogicalMonitor {
        let monitor = [(
            output.name.clone(),   // connector
            output.make.clone(),   // vendor
            output.model.clone(),  // product
            output.serial.clone(), // serial
        )];
        let scale = match output.scale {
            Some(s) => s,
            None => {
                warn!("Cannot get scale value.");
                1.0
            }
        };
        let transform = MonitorTransform::from_sway(&output.transform) as u32;
        LogicalMonitor {
            scale,
            monitors: monitor.to_vec(),
            primary: output.primary,
            transform,
            x_pos: output.rect.x,
            y_pos: output.rect.y,
            properties: LogicalMonitorProperties {
                // Dummy data to emulate a{sv}
                dummy: None,
                dummy2: None,
            },
        }
    }
    pub fn connector(&self) -> &str {
        &self.monitors[0].0
    }

    pub fn set_primary(&mut self, primary: bool) {
        self.primary = primary;
    }

    pub fn get_dpy_name(&self) -> String {
        let desc = &self.monitors[0];
        format!("{} {} {}", desc.1, desc.2, desc.3)
    }
}

impl MonitorApply {
    fn get_modestr(&self, monitor: &Monitor) -> Option<String> {
        let modestr = &self.monitors[0].1;
        monitor
            .search_modes(modestr)
            .map(|x| x.get_modestr().to_string())
    }

    pub fn search_monitor<'a>(&self, monitors: &'a [Monitor]) -> Option<&'a Monitor> {
        monitors
            .iter()
            .find(|mon| mon.description.0 == self.monitors[0].0)
    }

    pub fn search_logical_monitor<'a>(
        &self,
        logical_monitors: &'a [LogicalMonitor],
    ) -> Option<&'a LogicalMonitor> {
        logical_monitors
            .iter()
            .find(|mon| mon.monitors[0].0 == self.monitors[0].0)
    }

    pub fn save_kanshi(&self, kanshi_file: &mut Vec<u8>, monitor: &Monitor, connected: &[Monitor]) {
        let dpy_name = monitor.kanshi_name(connected);
        let mode = match self.get_modestr(monitor) {
            Some(x) => x,
            _ => return,
        };
        let transform =
            MonitorTransform::from_u32(self.transform).unwrap_or(MonitorTransform::Normal);
        let config = format!(
            "output \"{}\" mode {} position {},{} transform {} scale {} enable",
            dpy_name,
            mode,
            self.x_pos,
            self.y_pos,
            transform.to_kanshi(),
            self.scale
        );
        writeln!(kanshi_file, "\t{config}").unwrap();
    }

    /// The sway command that applies this logical monitor to `monitor`, or
    /// None if `monitor` has no such mode.
    pub fn sway_command(&self, monitor: &Monitor) -> Option<String> {
        let mode = monitor.search_modes(&self.monitors[0].1)?;
        let transform =
            MonitorTransform::from_u32(self.transform).unwrap_or(MonitorTransform::Normal);
        Some(format!(
            "output \"{}\" enable mode {} position {} {} scale {} transform {}",
            monitor.connector(),
            mode.sway_mode(),
            self.x_pos,
            self.y_pos,
            self.scale,
            transform.to_sway()
        ))
    }

    pub fn verify(
        &self,
        _sway_connect: &Arc<Mutex<Connection>>,
        monitors: &[Monitor],
        logical_monitors: &[LogicalMonitor],
    ) -> zbus::fdo::Result<()> {
        if self.monitors.is_empty() {
            return Err(ZError::InvalidArgs(String::from(
                "Logical monitor is empty",
            )));
        }
        // sway shows each output on its own: it cannot mirror one logical
        // monitor on several monitors.
        if self.monitors.len() > 1 {
            return Err(ZError::InvalidArgs(String::from(
                "Mirroring is not supported",
            )));
        }
        let monitor = self
            .search_monitor(monitors)
            .ok_or(Failed(String::from("Monitor not found")))?;

        // Check if position is valid
        if self.get_modestr(monitor).is_none() {
            return Err(ZError::InvalidArgs(String::from("Invalid position")));
        }

        // Check if mode is valid
        let mode = monitor
            .search_modes(&self.monitors[0].1)
            .ok_or(ZError::InvalidArgs(String::from(
                "Invalid resolution / refresh rate",
            )))?;

        // A scale set outside Settings (sway's configuration, an older
        // profile) is kept while the monitor keeps its mode, so that it doesn't
        // block every other change.
        let unchanged_scale = mode.current()
            && self
                .search_logical_monitor(logical_monitors)
                .is_some_and(|current| (current.scale - self.scale).abs() < 1e-6);
        if !mode.is_valid_scale(self.scale) && !unchanged_scale {
            return Err(ZError::InvalidArgs(format!(
                "Scale {} not valid for resolution {}x{}",
                self.scale,
                mode.width(),
                mode.height()
            )));
        }

        if MonitorTransform::from_u32(self.transform).is_none() {
            return Err(ZError::InvalidArgs(String::from("Invalid tranform")));
        }
        Ok(())
    }

    pub fn connector(&self) -> &str {
        &self.monitors[0].0
    }

    pub fn is_primary(&self) -> bool {
        self.primary
    }

    pub fn position(&self) -> (i32, i32) {
        (self.x_pos, self.y_pos)
    }

    /// The size of this logical monitor in the layout, in logical pixels: the
    /// mode, turned for a 90 or 270 degree transform, divided by the scale.
    /// Call it once [`MonitorApply::verify`] has accepted the monitor.
    pub fn logical_size(&self, monitors: &[Monitor]) -> (i32, i32) {
        let Some(mode) = self
            .search_monitor(monitors)
            .and_then(|monitor| monitor.search_modes(&self.monitors[0].1))
        else {
            return (0, 0);
        };
        let (mut width, mut height) = (mode.width(), mode.height());
        if self.transform % 2 == 1 {
            std::mem::swap(&mut width, &mut height);
        }
        (
            (width as f64 / self.scale).round() as i32,
            (height as f64 / self.scale).round() as i32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(modes: &str) -> Output {
        serde_json::from_str(&format!(
            r#"{{"id": 3, "name": "HEADLESS-1", "make": "Unknown", "model": "Unknown",
                "serial": "Unknown", "active": true, "dpms": true, "primary": false,
                "scale": 1.0, "transform": "normal", "modes": {modes},
                "current_mode": {{"width": 1600, "height": 1000, "refresh": 0}},
                "rect": {{"x": 0, "y": 0, "width": 1600, "height": 1000}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn output_without_modes_reports_current_mode() {
        let monitor = Monitor::new(&output("[]"));
        assert_eq!(monitor.modes.len(), 1);
        assert_eq!(monitor.modes[0].get_id(), "1600x1000@0Hz");
        assert!(monitor.modes[0].current());
        assert_eq!(monitor.get_current_mode(), "1600x1000@0Hz");
    }

    fn output_named(name: &str, make: &str) -> Output {
        serde_json::from_str(&format!(
            r#"{{"id": 3, "name": "{name}", "make": "{make}", "model": "U2720Q",
                "serial": "ABC123", "active": true, "dpms": true, "primary": false,
                "scale": 2.0, "transform": "normal", "modes": [],
                "current_mode": {{"width": 3840, "height": 2160, "refresh": 60000}},
                "rect": {{"x": 0, "y": 0, "width": 1920, "height": 1080}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn size_only_in_the_name() {
        let props = MonitorProperties::with_physical_size(
            &output_named("DP-1", "Dell Inc."),
            Some((600, 340)),
        );
        assert_eq!((props.width, props.height), (None, None));
        assert_eq!(props.name.as_deref(), Some("Dell Inc. 27\""));
        assert_eq!(props.builtin, Some(false));
    }

    #[test]
    fn no_size_when_unknown() {
        // sway's logical size (1920x1080 at scale 2) must not be reported
        // as millimetres.
        let props = MonitorProperties::with_physical_size(&output_named("DP-1", "Dell Inc."), None);
        assert_eq!((props.width, props.height), (None, None));
        assert_eq!(props.name.as_deref(), Some("Dell Inc."));
    }

    #[test]
    fn name_has_no_model_or_serial() {
        let props = MonitorProperties::with_physical_size(
            &output_named("Virtual-1", "Red Hat, Inc."),
            Some((330, 200)),
        );
        assert_eq!(props.name.as_deref(), Some("Red Hat, Inc. 15\""));
    }

    #[test]
    fn laptop_panel_is_built_in_display() {
        for connector in ["eDP-1", "LVDS-1", "DSI-1"] {
            let props = MonitorProperties::with_physical_size(
                &output_named(connector, "BOE"),
                Some((340, 190)),
            );
            assert_eq!(props.name.as_deref(), Some("Built-in display"));
            assert_eq!(props.builtin, Some(true));
            assert_eq!(props.width, None);
        }
    }

    #[test]
    fn unknown_vendor() {
        assert_eq!(
            display_name("Unknown", false, Some((330, 200))),
            "Unknown 15\""
        );
        assert_eq!(display_name("Unknown", false, None), "Unknown Display");
        assert_eq!(display_name("", false, None), "Unknown Display");
    }

    #[test]
    fn diagonals_like_mutter() {
        assert_eq!(diagonal_to_str(15.58), "15.6\"");
        assert_eq!(diagonal_to_str(13.3), "13.3\"");
        assert_eq!(diagonal_to_str(27.15), "27\"");
        assert_eq!(diagonal_to_str(23.5), "24\"");
    }

    fn hash_of<T: Hash>(value: &T) -> u64 {
        use std::hash::Hasher;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    /// `name` at `x`, in the mode `width`x1080.
    fn placed_output(name: &str, x: i32, width: i32) -> Output {
        serde_json::from_value(serde_json::json!({
            "id": 3, "name": name, "make": "Acme", "model": "M1", "serial": "1",
            "active": true, "dpms": true, "primary": false, "scale": 1.0,
            "transform": "normal",
            "modes": [
                {"width": 1920, "height": 1080, "refresh": 60000},
                {"width": 1280, "height": 1080, "refresh": 60000}
            ],
            "current_mode": {"width": width, "height": 1080, "refresh": 60000},
            "rect": {"x": x, "y": 0, "width": width, "height": 1080}
        }))
        .unwrap()
    }

    #[test]
    fn equal_monitors_hash_the_same() {
        let outputs = [
            placed_output("DP-1", 0, 1920),
            placed_output("DP-1", 0, 1280),
            placed_output("DP-2", 0, 1920),
            placed_output("DP-1", 1920, 1920),
        ];
        for a in &outputs {
            for b in &outputs {
                let (ma, mb) = (Monitor::new(a), Monitor::new(b));
                if ma == mb {
                    assert_eq!(hash_of(&ma), hash_of(&mb), "{ma:?} and {mb:?}");
                }
                let (la, lb) = (LogicalMonitor::new(a), LogicalMonitor::new(b));
                if la == lb {
                    assert_eq!(hash_of(&la), hash_of(&lb), "{la:?} and {lb:?}");
                }
            }
        }
    }

    #[test]
    fn a_monitor_in_another_mode_is_another_state() {
        let wide = Monitor::new(&placed_output("DP-1", 0, 1920));
        let narrow = Monitor::new(&placed_output("DP-1", 0, 1280));
        assert_ne!(wide, narrow);
    }

    #[test]
    fn the_same_monitor_in_any_mode() {
        let wide = Monitor::new(&placed_output("DP-1", 0, 1920));
        assert!(wide.same_monitor(&Monitor::new(&placed_output("DP-1", 0, 1280))));
        assert!(!wide.same_monitor(&Monitor::new(&placed_output("DP-2", 0, 1920))));
    }

    #[test]
    fn another_monitor_in_the_same_place_is_another_logical_monitor() {
        assert_ne!(
            LogicalMonitor::new(&placed_output("DP-1", 0, 1920)),
            LogicalMonitor::new(&placed_output("DP-2", 0, 1920))
        );
        assert_eq!(
            LogicalMonitor::new(&placed_output("DP-1", 0, 1920)),
            LogicalMonitor::new(&placed_output("DP-1", 0, 1920))
        );
    }

    #[test]
    fn sway_and_kanshi_name_quarter_turns_the_other_way_round() {
        for transform in 0..8 {
            let t = MonitorTransform::from_u32(transform).unwrap();
            let sway = Some(t.to_sway().to_string());
            assert_eq!(MonitorTransform::from_sway(&sway) as u32, transform);
        }
        assert_eq!(MonitorTransform::Left.to_sway(), "90");
        assert_eq!(MonitorTransform::Left.to_kanshi(), "270");
        assert_eq!(MonitorTransform::FlippedRight.to_sway(), "flipped-270");
        assert_eq!(MonitorTransform::FlippedRight.to_kanshi(), "flipped-90");
    }

    #[test]
    fn sway_command_applies_the_logical_monitor() {
        let monitor = Monitor::new(&output(
            r#"[{"width": 1920, "height": 1080, "refresh": 59951},
                {"width": 1600, "height": 1000, "refresh": 0}]"#,
        ));
        let apply = |mode: &str, transform| MonitorApply {
            x_pos: 100,
            y_pos: -20,
            scale: 1.5,
            transform,
            primary: false,
            monitors: vec![(
                "HEADLESS-1".into(),
                mode.into(),
                MonitorProperties::new(&output("[]")),
            )],
        };
        assert_eq!(
            apply("1920x1080@59.951Hz", 1).sway_command(&monitor).as_deref(),
            Some("output \"HEADLESS-1\" enable mode 1920x1080@59.951Hz position 100 -20 scale 1.5 transform 90")
        );
        assert_eq!(
            apply("1600x1000@0Hz", 0).sway_command(&monitor).as_deref(),
            Some("output \"HEADLESS-1\" enable mode 1600x1000 position 100 -20 scale 1.5 transform normal")
        );
        assert_eq!(apply("1x1@1Hz", 0).sway_command(&monitor), None);
    }

    #[test]
    fn output_switched_off_keeps_its_known_modes() {
        let on = Monitor::new(&output("[]"));
        let mut off = Monitor::new(
            &serde_json::from_str(
                r#"{"id": 3, "name": "HEADLESS-1", "make": "Unknown", "model": "Unknown",
                    "serial": "Unknown", "active": false, "dpms": false, "primary": false,
                    "modes": [], "rect": {"x": 0, "y": 0, "width": 0, "height": 0}}"#,
            )
            .unwrap(),
        );
        assert!(off.modes.is_empty());
        off.keep_known_modes(&[on]);
        assert_eq!(off.modes.len(), 1);
        assert_eq!(off.modes[0].get_id(), "1600x1000@0Hz");
        assert!(!off.modes[0].current());
        assert_eq!(off.get_current_mode(), "Unknown");
    }

    #[test]
    fn output_with_modes_is_unchanged() {
        let monitor = Monitor::new(&output(
            r#"[{"width": 1920, "height": 1080, "refresh": 60000},
                {"width": 1600, "height": 1000, "refresh": 0}]"#,
        ));
        let ids: Vec<&str> = monitor.modes.iter().map(|m| m.get_id()).collect();
        assert_eq!(ids, ["1920x1080@60Hz", "1600x1000@0Hz"]);
        assert_eq!(monitor.get_current_mode(), "1600x1000@0Hz");
    }
}
