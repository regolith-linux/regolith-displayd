mod modes;
mod monitor;
mod properties;
mod resources;

use core::fmt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::Command;
use std::{
    error::Error,
    fs::{self, File},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};
use tokio::sync::Mutex;
use zbus::{dbus_interface, ConnectionBuilder, SignalContext};
use zvariant::{DeserializeDict, SerializeDict, Type};

use self::monitor::{LogicalMonitor, Monitor};
use self::properties::DisplayManagerProperties;

#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
pub struct DisplayManager {
    serial: u32,
    monitors: Vec<Monitor>,
    logical_monitors: Vec<LogicalMonitor>,
    properties: DisplayManagerProperties,
}
