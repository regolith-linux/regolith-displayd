//! Night Light for the Wayland session.
//!
//! In GNOME, gnome-settings-daemon's color plugin (gsd-color) runs the Night
//! Light schedule and publishes the colour temperature the screens should
//! have as the `Temperature` property of `org.gnome.SettingsDaemon.Color`;
//! the compositor (Mutter) applies it. Under sway nothing applies it, so
//! regolith-displayd, which already stands in for Mutter's display
//! configuration, follows that property and sets a matching gamma table on
//! every output through wlr-gamma-control (see [`crate::gamma`]).
//!
//! Night Light runs on its own threads, so neither a missing color plugin
//! nor a missing compositor can disturb the `DisplayConfig` service.

use crate::gamma::{self, Command};
use log::{debug, info, warn};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;
use zbus::export::futures_util::StreamExt;
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::InterfaceName;
use zbus::CacheProperties;

/// The temperature at which the screen is left unchanged (identity ramps);
/// gsd-color reports it whenever Night Light is off.
pub const NEUTRAL_TEMPERATURE: u32 = 6500;

const GSD_COLOR: &str = "org.gnome.SettingsDaemon.Color";
const GSD_COLOR_PATH: &str = "/org/gnome/SettingsDaemon/Color";
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Chromaticity (x, y) of a black body at `kelvin`, from the cubic spline
/// approximation of the Planckian locus by Kim et al. (valid 1667-25000 K).
fn planckian_xy(kelvin: f64) -> (f64, f64) {
    let t = kelvin.clamp(1667.0, 25000.0);
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.2661239e9 / t3 - 0.2343589e6 / t2 + 0.8776956e3 / t + 0.179910
    } else {
        -3.0258469e9 / t3 + 2.1070379e6 / t2 + 0.2226347e3 / t + 0.240390
    };
    let (x2, x3) = (x * x, x * x * x);
    let y = if t <= 2222.0 {
        -1.1063814 * x3 - 1.34811020 * x2 + 2.18555832 * x - 0.20219683
    } else if t <= 4000.0 {
        -0.9549476 * x3 - 1.37418593 * x2 + 2.09137015 * x - 0.16748867
    } else {
        3.0817580 * x3 - 5.87338670 * x2 + 3.75112997 * x - 0.37001483
    };
    (x, y)
}

/// Linear sRGB of the chromaticity (x, y) at luminance 1.
fn xy_to_rgb((x, y): (f64, f64)) -> [f64; 3] {
    let (cx, cy, cz) = (x / y, 1.0, (1.0 - x - y) / y);
    [
        3.2404542 * cx - 1.5371385 * cy - 0.4985314 * cz,
        -0.9692660 * cx + 1.8760108 * cy + 0.0415560 * cz,
        0.0556434 * cx - 0.2040259 * cy + 1.0572252 * cz,
    ]
}

/// sRGB transfer function: gamma tables work on encoded values.
fn srgb_encode(linear: f64) -> f64 {
    if linear <= 0.0031308 {
        12.92 * linear
    } else {
        (1.055 * linear.powf(1.0 / 2.4) - 0.055).min(1.0)
    }
}

/// Per-channel scale (0..=1) that turns the neutral white into the colour of
/// a black body at `kelvin`, relative to the black body at the neutral
/// temperature, so it is exactly 1 for every channel there (and above:
/// Night Light only ever warms the screen). Within 0.025 of the table
/// Mutter uses (colord's Planckian blackbody colours) from 1700 K to 6500 K.
pub fn white_point(kelvin: u32) -> [f64; 3] {
    if kelvin >= NEUTRAL_TEMPERATURE {
        return [1.0; 3];
    }
    let target = xy_to_rgb(planckian_xy(kelvin as f64));
    let neutral = xy_to_rgb(planckian_xy(NEUTRAL_TEMPERATURE as f64));
    let mut scale = [0.0; 3];
    for c in 0..3 {
        scale[c] = (target[c] / neutral[c]).max(0.0);
    }
    let max = scale.iter().cloned().fold(f64::MIN, f64::max);
    scale.map(|s| srgb_encode(s / max))
}

/// A gamma table of `size` entries per channel for `kelvin`, laid out as
/// wlr-gamma-control expects: all red values, then green, then blue. Each
/// ramp is the identity ramp scaled by the channel's white point, so the
/// neutral temperature gives exactly the identity table.
pub fn gamma_table(kelvin: u32, size: usize) -> Vec<u16> {
    let white = white_point(kelvin);
    let mut table = Vec::with_capacity(size * 3);
    for scale in white {
        for i in 0..size {
            let level = if size > 1 {
                i as f64 / (size - 1) as f64
            } else {
                1.0
            };
            table.push((level * scale * 65535.0).round() as u16);
        }
    }
    table
}

async fn read_temperature(properties: &PropertiesProxy<'_>) -> Option<u32> {
    let interface = InterfaceName::try_from(GSD_COLOR).ok()?;
    match properties.get(interface, "Temperature").await {
        Ok(value) => u32::try_from(value).ok(),
        Err(e) => {
            debug!("Cannot read the Night Light temperature: {e}");
            None
        }
    }
}

/// Follow gsd-color's `Temperature` and send it to `sink`. While gsd-color
/// is not on the bus the neutral temperature is sent. Returns when the bus
/// connection fails.
pub async fn follow_gsd_color(
    connection: &zbus::Connection,
    sink: &Sender<Command>,
) -> zbus::Result<()> {
    let send = |temperature: Option<u32>| {
        let temperature = temperature.unwrap_or(NEUTRAL_TEMPERATURE);
        debug!("Night Light temperature {temperature} K");
        let _ = sink.send(Command::Temperature(temperature));
    };
    // Subscribe before the first read so no change is missed.
    let dbus = DBusProxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;
    let properties = PropertiesProxy::builder(connection)
        .destination(GSD_COLOR)?
        .path(GSD_COLOR_PATH)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let mut property_changes = properties.receive_properties_changed().await?;

    let temperature = read_temperature(&properties).await;
    match temperature {
        Some(t) => info!("Following the color plugin's Night Light temperature ({t} K)"),
        None => info!("The color plugin is not running; waiting for it"),
    }
    send(temperature);

    loop {
        tokio::select! {
            change = property_changes.next() => {
                let Some(signal) = change else { return Ok(()) };
                let Ok(args) = signal.args() else { continue };
                if args.interface_name().as_str() != GSD_COLOR {
                    continue;
                }
                if let Some(value) = args.changed_properties().get("Temperature") {
                    match u32::try_from(value.clone()) {
                        Ok(t) => send(Some(t)),
                        Err(_) => warn!("Ignoring a Night Light temperature that is not a uint32"),
                    }
                } else if args.invalidated_properties().contains(&"Temperature") {
                    send(read_temperature(&properties).await);
                }
            }
            change = owner_changes.next() => {
                let Some(signal) = change else { return Ok(()) };
                let Ok(args) = signal.args() else { continue };
                if args.name().as_str() != GSD_COLOR {
                    continue;
                }
                if args.new_owner().is_some() {
                    info!("The color plugin appeared on the bus");
                    send(read_temperature(&properties).await);
                } else {
                    info!("The color plugin left the bus; restoring neutral colours");
                    send(None);
                }
            }
        }
    }
}

/// Start Night Light: one thread applies gamma tables through the
/// compositor, another follows the color plugin on the session bus. Both
/// run for the lifetime of the daemon and never take it down.
pub fn spawn() {
    let (sink, commands) = mpsc::channel();
    let events = sink.clone();
    let spawned = std::thread::Builder::new()
        .name("night-light-gamma".into())
        .spawn(move || gamma::run(commands, events));
    if let Err(e) = spawned {
        warn!("Night Light is unavailable: cannot start its gamma thread: {e}");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("night-light-dbus".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    warn!("Night Light is unavailable: {e}");
                    return;
                }
            };
            runtime.block_on(async move {
                loop {
                    let result = async {
                        let connection = zbus::Connection::session().await?;
                        follow_gsd_color(&connection, &sink).await
                    }
                    .await;
                    if let Err(e) = result {
                        warn!("Cannot follow the Night Light temperature: {e}");
                    }
                    let _ = sink.send(Command::Temperature(NEUTRAL_TEMPERATURE));
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            });
        });
    if let Err(e) = spawned {
        warn!("Night Light is unavailable: cannot start its D-Bus thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_temperature_is_the_identity() {
        assert_eq!(white_point(6500), [1.0, 1.0, 1.0]);
        assert_eq!(white_point(7000), [1.0, 1.0, 1.0]);
        assert_eq!(gamma_table(6500, 4), [0, 21845, 43690, 65535].repeat(3));
    }

    #[test]
    fn lower_temperatures_are_warmer() {
        let mut previous = white_point(6500);
        for kelvin in (1000..6500).rev().step_by(100) {
            let w = white_point(kelvin);
            assert!((w[0] - 1.0).abs() < 1e-9, "{kelvin} K: red {w:?}");
            assert!(w[1] <= previous[1], "{kelvin} K: green {w:?}");
            assert!(w[2] <= previous[2], "{kelvin} K: blue {w:?}");
            assert!(w[2] < w[1], "{kelvin} K: {w:?}");
            previous = w;
        }
    }

    #[test]
    fn white_point_matches_gnome() {
        // Mutter's blackbody colours (colord, Planckian, D65); 2700 K is
        // GNOME's default Night Light temperature.
        let mutter = [
            (5000, [1.0, 0.9020, 0.8147]),
            (4000, [1.0, 0.8285, 0.6482]),
            (3500, [1.0, 0.7799, 0.5464]),
            (2700, [1.0, 0.6765, 0.3479]),
            (2000, [1.0, 0.5436, 0.0868]),
            (1700, [1.0, 0.4768, 0.0]),
        ];
        for (kelvin, expected) in mutter {
            let w = white_point(kelvin);
            for c in 0..3 {
                assert!((w[c] - expected[c]).abs() < 0.025, "{kelvin} K: {w:?}");
            }
        }
    }

    #[test]
    fn gamma_table_layout() {
        let table = gamma_table(3500, 256);
        assert_eq!(table.len(), 768);
        let w = white_point(3500);
        for (c, scale) in w.iter().enumerate() {
            assert_eq!(table[c * 256], 0);
            assert_eq!(table[c * 256 + 255], (65535.0 * scale).round() as u16);
        }
    }
}
