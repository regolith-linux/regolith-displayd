use serde::{Deserialize, Serialize};
use swayipc_async::{Mode as SwayMode, Output};
use zvariant::{DeserializeDict, SerializeDict, Type};

#[derive(Debug, Clone, Deserialize, Serialize, Type, PartialEq)]
pub struct Modes {
    id: String,
    width: i32,
    height: i32,
    refresh_rate: f64,
    preferred_scale: f64,
    supported_scales: Vec<f64>,
    properties: ModeProperties,
}

#[derive(Debug, Clone, DeserializeDict, SerializeDict, Type, PartialEq)]
#[zvariant(signature = "dict")]
pub struct ModeProperties {
    #[zvariant(rename = "is-current")]
    current: Option<bool>,
    #[zvariant(rename = "is-preferred")]
    preferred: Option<bool>,
    #[zvariant(rename = "is-interlaced")]
    interlaced: Option<bool>,
}

impl Modes {
    pub fn get_id(&self) -> &str {
        &self.id
    }

    pub fn new(output: &Output, mode_info: &SwayMode) -> Modes {
        let SwayMode {
            height,
            width,
            refresh,
            ..
        } = *mode_info;
        let is_current = match &output.current_mode {
            Some(x) => Self::is_current_mode(x, mode_info),
            _ => false,
        };

        let properties = ModeProperties {
            current: Some(is_current),
            interlaced: Some(false),
            preferred: Some(false),
        };
        let supported_scales = [1.0, 2.0].to_vec();
        Modes {
            width,
            height,
            supported_scales,
            id: format!(
                "{}x{}@{}Hz",
                mode_info.width,
                mode_info.height,
                mode_info.refresh as f64 / 1000f64
            ),
            preferred_scale: 1f64,
            refresh_rate: refresh as f64 / 1000f64,
            properties,
        }
    }
    pub fn get_modestr(&self) -> &str {
        &self.id
    }
    pub fn is_valid_scale(&self, scale: f64) -> bool {
        self.supported_scales.contains(&scale)
    }
    pub fn is_current_mode(actual: &SwayMode, current: &SwayMode) -> bool {
        current.height == actual.height
            && current.width == actual.width
            && current.refresh == actual.refresh
    }
    pub fn current(&self) -> bool {
        self.properties.current == Some(true)
    }
}
