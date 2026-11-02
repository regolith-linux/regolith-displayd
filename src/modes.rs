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
        let supported_scales = supported_scales(width, height);
        Modes {
            width,
            height,
            supported_scales,
            id: format!(
                "{}x{}@{}Hz",
                mode_info.width,
                mode_info.height,
                (mode_info.refresh as f64) / 1000f64
            ),
            preferred_scale: 1f64,
            refresh_rate: (refresh as f64) / 1000f64,
            properties,
        }
    }
    pub fn get_modestr(&self) -> &str {
        &self.id
    }
    /// The mode as sway's `output ... mode` command takes it.
    pub fn sway_mode(&self) -> String {
        if self.refresh_rate > 0.0 {
            format!("{}x{}@{}Hz", self.width, self.height, self.refresh_rate)
        } else {
            format!("{}x{}", self.width, self.height)
        }
    }
    /// Whether `scale` is one of this mode's supported scales, within 1e-6:
    /// clients send either the listed (single precision) value or the exact
    /// fraction.
    pub fn is_valid_scale(&self, scale: f64) -> bool {
        self.supported_scales
            .iter()
            .any(|&s| (s - scale).abs() <= SCALE_TOLERANCE)
    }
    pub fn width(&self) -> i32 {
        self.width
    }
    pub fn height(&self) -> i32 {
        self.height
    }
    pub fn is_current_mode(actual: &SwayMode, current: &SwayMode) -> bool {
        current.height == actual.height
            && current.width == actual.width
            && current.refresh == actual.refresh
    }
    /// This mode, marked current or not.
    pub fn with_current(mut self, current: bool) -> Modes {
        self.properties.current = Some(current);
        self
    }
    pub fn current(&self) -> bool {
        self.properties.current == Some(true)
    }
}

/// How far a requested scale may be from a listed one and still match it.
const SCALE_TOLERANCE: f64 = 1e-6;

/// Smallest logical area (in logical pixels) a scale may leave, as in Mutter
/// 50.2 and later (600x600; 800x480 before).
const MINIMUM_LOGICAL_AREA: i64 = 600 * 600;

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// The scales Mutter offers for a `width`x`height` mode
/// (`meta_monitor_calculate_supported_scales`): every fraction n/d in lowest
/// terms with d from 1 to 4 and 1 <= n/d <= 4 that divides both dimensions into
/// whole logical pixels and leaves at least 600x600 logical pixels of area.
///
/// Each scale is rounded to single precision, as Mutter stores it and as sway
/// reports an output's scale, so the scale of a logical monitor compares equal
/// to one of these values.
pub fn supported_scales(width: i32, height: i32) -> Vec<f64> {
    let (w, h) = (width.max(0) as i64, height.max(0) as i64);
    let mut scales: Vec<f64> = Vec::new();
    for d in 1..=4i64 {
        for n in d..=4 * d {
            if gcd(n, d) != 1 || (w * d) % n != 0 || (h * d) % n != 0 {
                continue;
            }
            if (w * d / n) * (h * d / n) < MINIMUM_LOGICAL_AREA {
                continue;
            }
            scales.push((n as f32 / d as f32) as f64);
        }
    }
    scales.sort_by(|a, b| a.partial_cmp(b).unwrap());
    scales.dedup();
    if scales.is_empty() {
        scales.push(1.0);
    }
    scales
}

#[cfg(test)]
mod tests {
    use super::supported_scales;

    fn approx(scales: Vec<f64>) -> Vec<String> {
        scales.iter().map(|s| format!("{s:.4}")).collect()
    }

    #[test]
    fn scales_follow_mutter() {
        assert_eq!(approx(supported_scales(1366, 768)), ["1.0000"]);
        assert_eq!(
            approx(supported_scales(1280, 800)),
            ["1.0000", "1.2500", "1.3333", "1.6667"]
        );
        assert_eq!(approx(supported_scales(1024, 768)), ["1.0000", "1.3333"]);
        assert_eq!(
            approx(supported_scales(1920, 1080)),
            ["1.0000", "1.2500", "1.3333", "1.5000", "1.6667", "2.0000"]
        );
        assert_eq!(
            approx(supported_scales(3840, 2160)),
            [
                "1.0000", "1.2500", "1.3333", "1.5000", "1.6667", "2.0000", "2.5000", "2.6667",
                "3.0000", "3.3333", "3.7500", "4.0000"
            ]
        );
        assert_eq!(supported_scales(640, 480), [1.0]);
    }
}
