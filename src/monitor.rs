use std::sync::Arc;

use crate::modes::Modes;
use serde::{Deserialize, Serialize};
use swayipc_async::{Connection, Output};
use tokio::sync::Mutex;
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

#[derive(Debug, PartialEq, Eq, Clone, DeserializeDict, SerializeDict, Type)]
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
pub struct LogicalMonitorProperties;

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
}

impl MonitorProperties {
    pub fn new(output: &Output) -> MonitorProperties {
        let name = Some(format!(
            "{} '{} {} {}'",
            &output.name, &output.make, &output.model, &output.serial
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

impl LogicalMonitor {
    pub fn new(output: &Output) -> LogicalMonitor {
        let monitor = [(
            output.name.clone(),   // connector
            output.make.clone(),   // vendor
            output.model.clone(),  // product
            output.serial.clone(), // serial
        )];
        LogicalMonitor {
            monitors: monitor.to_vec(),
            scale: output.scale.unwrap_or(1f64),
            primary: output.primary,
            transform: 0,
            x_pos: output.rect.x,
            y_pos: output.rect.y,
            properties: LogicalMonitorProperties {},
        }
    }
    fn get_sway_dpy_name(&self) -> String {
        let curr_monitor = self
            .monitors
            .get(0)
            .expect("No monitors for logical monitor...");
        let display_name = format!(
            "\"{} {} {}\"",
            curr_monitor.1, curr_monitor.2, curr_monitor.3
        );
        display_name
    }

    fn build_pos_cmd(&self) -> String {
        let dpy_name = self.get_sway_dpy_name();
        format!("output {} pos {} {}", dpy_name, self.x_pos, self.y_pos)
    }

    fn _build_mode_cmd(&self) -> String {
        let dpy_name = self.get_sway_dpy_name();
        // format!("output {} mode {}x{}@{}", dpy_name,)
        dpy_name
    }

    fn build_scale_cmd(&self) -> String {
        let dpy_name = self.get_sway_dpy_name();
        format!("output {dpy_name} scale {}", self.scale)
    }

    pub async fn apply(&self, sway_connect: &Arc<Mutex<Connection>>) {
        let cmds = [self.build_pos_cmd(), self.build_scale_cmd()];
        for cmd in cmds {
            sway_connect
                .lock()
                .await
                .run_command(cmd)
                .await
                .expect("Failed to run command {cmd}");
        }
    }
}
