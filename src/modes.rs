use serde::{Deserialize, Serialize};
use swayipc_async::{Mode as SwayMode, Output};
use zvariant::{DeserializeDict, SerializeDict, Type};

#[derive(Debug, Clone, Deserialize, Serialize, Type)]
pub struct Modes {
    id: String,
    width: i32,
    height: i32,
    refresh_rate: f64,
    preferred_scale: f64,
    supported_scales: Vec<f64>,
    properties: ModeProperties,
}

#[derive(Debug, Clone, DeserializeDict, SerializeDict, Type)]
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
    pub fn new(output: &Output, mode_info: &SwayMode) -> Modes {
        let SwayMode {
            height,
            width,
            refresh,
            ..
        } = *mode_info;
        let is_current = match &output.current_mode {
            Some(x) => x == mode_info,
            _ => false,
        };

        let properties = ModeProperties {
            current: Some(is_current),
            interlaced: Some(false),
            preferred: Some(false),
        };
        let supported_scales = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0].to_vec();
        Modes {
            width,
            height,
            supported_scales,
            id: format!(
                "{}x{} @ {}",
                mode_info.width,
                mode_info.height,
                mode_info.refresh as f64 / 1000f64
            ),
            preferred_scale: 1f64,
            refresh_rate: refresh as f64 / 1000f64,
            properties,
        }
    }
}
