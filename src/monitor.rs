use crate::modes::Modes;
use log::{error, warn};
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
        let output_modes = output.modes.iter().map(|m| Modes::new(output, m)).collect();
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

    pub fn search_modes(&self, mode_id: &str) -> Option<&Modes> {
        self.modes.iter().find(|&m| m.get_id() == mode_id)
    }

    pub fn get_dpy_name(&self) -> String {
        let desc = &self.description;
        format!("{} {} {}", desc.1, desc.2, desc.3)
    }

    pub fn get_current_mode(&self) -> &str {
        match self.modes.iter().find(|&mode| mode.current()) {
            Some(m) => m.get_modestr(),
            None => "Unknown",
        }
    }
}

impl PartialEq for Monitor {
    fn eq(&self, other: &Self) -> bool {
        self.description == other.description
    }
}

impl PartialEq for LogicalMonitor {
    fn eq(&self, other: &Self) -> bool {
        self.x_pos == other.x_pos
            && self.y_pos == other.y_pos
            && self.scale == other.scale
            && self.transform == other.transform
    }
}

impl Eq for Monitor {}

impl Eq for LogicalMonitor {}

impl Hash for Monitor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.description.hash(state);
        self.get_current_mode().hash(state);
    }
}

impl Hash for LogicalMonitor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.y_pos.hash(state);
        self.x_pos.hash(state);
        self.transform.hash(state);
        let scale_int = (self.scale * 1000f64) as u32;
        scale_int.hash(state);
        self.monitors[0].hash(state);
    }
}

impl MonitorProperties {
    pub fn new(output: &Output) -> MonitorProperties {
        let name = Some(format!(
            "{} {} {}",
            &output.make, &output.model, &output.serial
        ));
        let builtin = output.name.starts_with("eDP");
        MonitorProperties {
            width: Some(output.rect.width),
            height: Some(output.rect.height),
            name,
            builtin: Some(builtin),
            max_size: None,
            underscanning: None,
        }
    }
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

    pub fn to_sway(self) -> &'static str {
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
    pub fn get_dpy_name(&self) -> String {
        let desc = &self.monitors[0];
        format!("{} {} {}", desc.1, desc.2, desc.3)
    }
}

impl MonitorApply {
    fn get_modestr(&self, monitor: &Monitor) -> Option<String> {
        let modestr = &self.monitors[0].1;
        match monitor.search_modes(&modestr) {
            Some(x) => Some(x.get_modestr().to_string()),
            None => None,
        }
    }

    pub fn search_monitor<'a>(&self, monitors: &'a Vec<Monitor>) -> Option<&'a Monitor> {
        monitors
            .iter()
            .find(|mon| mon.description.0 == self.monitors[0].0)
    }

    pub fn search_logical_monitor<'a>(
        &self,
        logical_monitors: &'a Vec<LogicalMonitor>,
    ) -> Option<&'a LogicalMonitor> {
        logical_monitors
            .iter()
            .find(|mon| mon.monitors[0].0 == self.monitors[0].0)
    }

    pub fn save_kanshi(&self, kanshi_file: &mut Vec<u8>, monitor: &Monitor) {
        let dpy_name = monitor.get_dpy_name();
        let mode = match self.get_modestr(&monitor) {
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
            transform.to_sway(),
            self.scale
        );
        writeln!(kanshi_file, "\t{config}").unwrap();
    }

    pub fn verify(
        &self,
        _sway_connect: &Arc<Mutex<Connection>>,
        monitors: &Vec<Monitor>,
    ) -> zbus::fdo::Result<()> {
        let monitor = self
            .search_monitor(monitors)
            .ok_or(Failed(String::from("Monitor not found")))?;

        // Check if position is valid
        if self.get_modestr(monitor) == None {
            return Err(ZError::InvalidArgs(String::from("Invalid position")));
        }

        // Check if mode is valid
        let mode = monitor
            .search_modes(&self.monitors[0].1)
            .ok_or(ZError::InvalidArgs(String::from(
                "Invalid resolution / refresh rate",
            )))?;

        if !mode.is_valid_scale(self.scale) {
            return Err(ZError::InvalidArgs(String::from("Invalid scale")));
        }

        if MonitorTransform::from_u32(self.transform) == None {
            return Err(ZError::InvalidArgs(String::from("Invalid tranform")));
        }
        Ok(())
    }
}
