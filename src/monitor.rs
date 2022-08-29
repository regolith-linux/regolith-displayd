use crate::modes::Modes;
use log::{debug, warn};
use num;
use num_derive::FromPrimitive;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::{error::Error, sync::Arc};
use swayipc_async::{Connection, Output};
use tokio::sync::Mutex;
use zbus::fdo::Error::{self as ZError, Failed};
use zvariant::{DeserializeDict, SerializeDict, Type};

trait Apply {
    fn apply() -> Result<(), Box<dyn Error>>;
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
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

#[derive(FromPrimitive)]
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
}

impl MonitorProperties {
    pub fn new(output: &Output) -> MonitorProperties {
        let name = Some(format!(
            "{} {} {}",
            &output.make, &output.model, &output.serial
        ));
        MonitorProperties {
            width: Some(output.rect.width),
            height: Some(output.rect.height),
            name,
            builtin: Some(false),
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
}

impl MonitorApply {
    fn get_modestr(&self, monitor: &Monitor) -> Option<String> {
        let modestr = &self.monitors[0].1;
        match monitor.search_modes(&modestr) {
            Some(x) => Some(x.get_modestr().to_string()),
            None => None,
        }
    }
    fn build_pos_cmd(&self, monitor: &Monitor) -> String {
        let dpy_name = monitor.get_dpy_name();
        format!(
            "output '{}' position {} {}",
            dpy_name, self.x_pos, self.y_pos
        )
    }

    fn build_mode_cmd(&self, monitor: &Monitor) -> Option<String> {
        let dpy_name = monitor.get_dpy_name();
        let mode = self.get_modestr(monitor)?;
        Some(format!("output '{}' mode {}", dpy_name, mode))
    }

    fn build_scale_cmd(&self, monitor: &Monitor) -> String {
        let dpy_name = monitor.get_dpy_name();
        format!("output '{dpy_name}' scale {}", self.scale)
    }

    fn build_transform_cmd(&self, monitor: &Monitor) -> Option<String> {
        let dpy_name = monitor.get_dpy_name();
        let transform = MonitorTransform::from_u32(self.transform)?;
        Some(format!(
            "output '{dpy_name}' transform {}",
            transform.to_sway()
        ))
    }

    pub fn search_monitor<'a>(&self, monitors: &'a Vec<Monitor>) -> Option<&'a Monitor> {
        monitors
            .iter()
            .find(|mon| mon.description.0 == self.monitors[0].0)
    }

    pub fn save_kanshi(&self, kanshi_file: &mut File, monitor: &Monitor) {
        let dpy_name = monitor.get_dpy_name();
        let mode = match self.get_modestr(&monitor) {
            Some(x) => x,
            _ => return,
        };
        let transform =
            MonitorTransform::from_u32(self.transform).unwrap_or(MonitorTransform::Normal);
        let config = format!(
            "output '\"{}\"' mode {} position {},{} transform {} scale {}",
            dpy_name,
            mode,
            self.x_pos,
            self.y_pos,
            transform.to_sway(),
            self.scale
        );
        writeln!(kanshi_file, "\t{config}").unwrap();
    }

    pub async fn apply(&self, sway_connect: &Arc<Mutex<Connection>>, monitor: &Monitor) {
        debug!("Entered fn apply for monitor - {}", monitor.description.0);
        let cmds = [
            self.build_pos_cmd(monitor),
            self.build_scale_cmd(monitor),
            self.build_mode_cmd(monitor).unwrap(),
            self.build_transform_cmd(monitor).unwrap(),
        ];
        let mut connection = sway_connect.lock().await;
        for cmd in cmds {
            warn!("Running command: {}", cmd);
            connection
                .run_command(cmd)
                .await
                .expect("Failed to run command {cmd}");
        }
    }

    pub fn verify(
        &self,
        _sway_connect: &Arc<Mutex<Connection>>,
        monitors: &Vec<Monitor>,
    ) -> zbus::fdo::Result<()> {
        let monitor = self
            .search_monitor(monitors)
            .ok_or(Failed(String::from("Monitor not found")))?;
        self.build_mode_cmd(monitor)
            .ok_or(ZError::InvalidArgs(String::from("Invalid position")))?;
        let mode = monitor
            .search_modes(&self.monitors[0].1)
            .ok_or(ZError::InvalidArgs(String::from(
                "Invalid resolution / refresh rate",
            )))?;
        if !mode.is_valid_scale(self.scale) {
            return Err(ZError::InvalidArgs(String::from("Invalid scale")));
        }
        if self.build_transform_cmd(monitor) == None {
            return Err(ZError::InvalidArgs(String::from("Invalid tranform")));
        }
        Ok(())
    }
}
