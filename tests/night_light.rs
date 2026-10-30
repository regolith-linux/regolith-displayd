//! regolith-displayd follows the color plugin's Night Light temperature, as
//! Mutter does in GNOME. These tests run the follower against a fake color
//! plugin on a private bus.

use regolith_displayd::night_light::{follow_gsd_color, NEUTRAL_TEMPERATURE};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::time::Duration;
use tokio::sync::watch;
use zbus::names::InterfaceName;
use zbus::{dbus_interface, ConnectionBuilder, SignalContext};

const NAME: &str = "org.gnome.SettingsDaemon.Color";
const PATH: &str = "/org/gnome/SettingsDaemon/Color";

struct FakeColorPlugin {
    temperature: u32,
}

#[dbus_interface(name = "org.gnome.SettingsDaemon.Color")]
impl FakeColorPlugin {
    #[dbus_interface(property)]
    fn temperature(&self) -> u32 {
        self.temperature
    }
}

struct Bus(Child);

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn private_bus() -> (Bus, String) {
    let mut child = std::process::Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon is needed to run this test");
    let mut address = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    (Bus(child), address.trim().to_string())
}

async fn start_color_plugin(address: &str, temperature: u32) -> zbus::Connection {
    ConnectionBuilder::address(address)
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, FakeColorPlugin { temperature })
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// Change the temperature and announce it, value included, as gsd-color does.
async fn set_temperature(plugin: &zbus::Connection, temperature: u32) {
    let iface = plugin
        .object_server()
        .interface::<_, FakeColorPlugin>(PATH)
        .await
        .unwrap();
    let mut fake = iface.get_mut().await;
    fake.temperature = temperature;
    let ctxt: &SignalContext<'_> = iface.signal_context();
    fake.temperature_changed(ctxt).await.unwrap();
}

/// Change the temperature but only invalidate the property.
async fn invalidate_temperature(plugin: &zbus::Connection, temperature: u32) {
    let iface = plugin
        .object_server()
        .interface::<_, FakeColorPlugin>(PATH)
        .await
        .unwrap();
    iface.get_mut().await.temperature = temperature;
    zbus::fdo::Properties::properties_changed(
        iface.signal_context(),
        InterfaceName::try_from(NAME).unwrap(),
        &HashMap::new(),
        &["Temperature"],
    )
    .await
    .unwrap();
}

/// Wait until the follower sends `expected`.
async fn expect(temperature: &mut watch::Receiver<u32>, expected: u32) {
    let wait = async {
        while *temperature.borrow_and_update() != expected {
            temperature.changed().await.unwrap();
        }
    };
    if tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .is_err()
    {
        panic!("expected {expected} K, last sent {}", *temperature.borrow());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_color_plugin_temperature() {
    let (_bus, address) = private_bus();
    // Start from a value no step expects, so each step needs a send.
    let (sink, mut temperature) = watch::channel(0);
    let follower_address = address.clone();
    tokio::spawn(async move {
        let connection = ConnectionBuilder::address(follower_address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        follow_gsd_color(&connection, &sink).await
    });

    // No color plugin: neutral.
    expect(&mut temperature, NEUTRAL_TEMPERATURE).await;

    // The plugin starts with Night Light active.
    let plugin = start_color_plugin(&address, 3500).await;
    expect(&mut temperature, 3500).await;

    // The schedule or the Settings slider changes the temperature.
    for kelvin in [3200, 2700, 6500, 4000] {
        set_temperature(&plugin, kelvin).await;
        expect(&mut temperature, kelvin).await;
    }

    // An invalidated property is read again.
    invalidate_temperature(&plugin, 3000).await;
    expect(&mut temperature, 3000).await;

    // The plugin stops (its bus name goes away): back to neutral rather
    // than a stuck tint.
    plugin.release_name(NAME).await.unwrap();
    expect(&mut temperature, NEUTRAL_TEMPERATURE).await;

    // It comes back.
    let _plugin = start_color_plugin(&address, 2700).await;
    expect(&mut temperature, 2700).await;
}
