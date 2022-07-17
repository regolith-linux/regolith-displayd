use serde::{Deserialize, Serialize};
use swayipc_async::Output;
use zvariant::{DeserializeDict, SerializeDict, Type};

use crate::modes::Modes;

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

// connector: String,
// vendor: String,
// product: String,
// serial: String,
impl Monitor {
    pub fn new(output: &Output) -> Monitor {
        let output_modes = output.modes.iter().map(|m| Modes::new(output, m)).collect();
        let description = (
            output.name.clone(),
            output.make.clone(),
            output.model.clone(),
            output.serial.clone(),
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
            output.name.clone(),
            output.make.clone(),
            output.model.clone(),
            output.serial.clone(),
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
}
