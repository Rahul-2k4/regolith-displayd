use crate::modes::Modes;
use crate::wayland_observer::OutputHeadSnapshot;
use log::warn;
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

pub(crate) trait KanshiProfileEntry {
    fn find_monitor<'a>(&self, monitors: &'a [Monitor]) -> Option<&'a Monitor>;
    fn write_kanshi(&self, kanshi_file: &mut Vec<u8>, monitor: &Monitor) -> bool;
}

impl Monitor {
    pub fn new(output: &Output) -> Monitor {
        let output_modes = output.modes.iter().map(|m| Modes::new(output, m)).collect();
        Monitor {
            description: (
                output.name.clone(),   // connector
                output.make.clone(),   // vendor
                output.model.clone(),  // product
                output.serial.clone(), // serial
            ),
            modes: output_modes,
            properties: MonitorProperties::new(output),
        }
    }

    pub fn from_snapshot(head: &OutputHeadSnapshot) -> Monitor {
        let modes = head
            .modes
            .iter()
            .filter_map(|mode| {
                let current = head.current_mode.as_ref().map_or(mode.current, |selected| {
                    mode.width == selected.width
                        && mode.height == selected.height
                        && mode.refresh_mhz == selected.refresh_mhz
                });
                Modes::from_snapshot_with_current(mode, current)
            })
            .collect();

        Monitor {
            // The wlroots output-management snapshot exposes a human-readable
            // description when available, with the connector name as fallback.
            description: snapshot_identity(&head.name, head.description.as_deref()),
            modes,
            properties: MonitorProperties::from_snapshot(head),
        }
    }

    pub fn search_modes(&self, mode_id: &str) -> Option<&Modes> {
        self.modes.iter().find(|&m| m.get_id() == mode_id)
    }

    pub fn get_dpy_name(&self) -> String {
        let desc = &self.description;
        if desc.1.is_empty() && desc.2.is_empty() && desc.3.is_empty() {
            desc.0.clone()
        } else {
            format!("{} {} {}", desc.1, desc.2, desc.3)
        }
    }

    pub fn get_current_mode(&self) -> &str {
        match self.modes.iter().find(|&mode| mode.current()) {
            Some(m) => m.get_modestr(),
            None => "Unknown",
        }
    }
}

#[cfg(test)]
impl Monitor {
    pub(crate) fn test_new(
        name: &str,
        make: &str,
        model: &str,
        serial: &str,
        modes: Vec<Modes>,
    ) -> Monitor {
        Monitor {
            description: (
                name.to_string(),
                make.to_string(),
                model.to_string(),
                serial.to_string(),
            ),
            modes,
            properties: MonitorProperties {
                width: None,
                height: None,
                underscanning: None,
                builtin: Some(false),
                max_size: None,
                name: Some(format!("{make} {model} {serial}")),
            },
        }
    }
}

impl PartialEq for Monitor {
    fn eq(&self, other: &Self) -> bool {
        self.description == other.description && self.get_current_mode() == other.get_current_mode()
    }
}

// Sway reports one physical monitor per logical monitor here, and profile
// generation resolves that ordered vector through its first element. Keep the
// watcher identity on the same first-element boundary.
impl PartialEq for LogicalMonitor {
    fn eq(&self, other: &Self) -> bool {
        self.x_pos == other.x_pos
            && self.y_pos == other.y_pos
            && self.scale.to_bits() == other.scale.to_bits()
            && self.transform == other.transform
            && self.monitors.first() == other.monitors.first()
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
        self.scale.to_bits().hash(state);
        self.monitors.first().hash(state);
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

    pub fn from_snapshot(head: &OutputHeadSnapshot) -> MonitorProperties {
        let dimensions = head
            .current_mode
            .as_ref()
            .map(|mode| (mode.width, mode.height))
            .or_else(|| head.modes.first().map(|mode| (mode.width, mode.height)));

        MonitorProperties {
            width: dimensions.map(|(width, _)| width),
            height: dimensions.map(|(_, height)| height),
            underscanning: None,
            builtin: Some(head.name.starts_with("eDP")),
            max_size: None,
            name: head.description.clone().or_else(|| Some(head.name.clone())),
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

    pub fn from_snapshot(head: &OutputHeadSnapshot) -> Option<LogicalMonitor> {
        if !head.enabled {
            return None;
        }

        let scale = match head.scale {
            Some(scale) => scale,
            None => {
                warn!(
                    "Wayland snapshot missing scale for enabled output {}",
                    head.name
                );
                return None;
            }
        };
        let (x_pos, y_pos) = match head.position {
            Some(position) => position,
            None => {
                warn!(
                    "Wayland snapshot missing position for enabled output {}",
                    head.name
                );
                return None;
            }
        };
        let transform = match head.transform {
            Some(transform) => transform,
            None => {
                warn!(
                    "Wayland snapshot missing transform for enabled output {}",
                    head.name
                );
                return None;
            }
        };

        Some(LogicalMonitor {
            x_pos,
            y_pos,
            scale,
            transform,
            primary: false,
            monitors: vec![snapshot_identity(&head.name, head.description.as_deref())],
            properties: LogicalMonitorProperties {
                dummy: None,
                dummy2: None,
            },
        })
    }

    pub fn get_dpy_name(&self) -> String {
        let desc = &self.monitors[0];
        if desc.1.is_empty() && desc.2.is_empty() && desc.3.is_empty() {
            desc.0.clone()
        } else {
            format!("{} {} {}", desc.1, desc.2, desc.3)
        }
    }
}

fn snapshot_identity(name: &str, description: Option<&str>) -> (String, String, String, String) {
    (
        description.unwrap_or(name).to_string(),
        String::new(),
        String::new(),
        String::new(),
    )
}

#[cfg(test)]
impl LogicalMonitor {
    pub(crate) fn test_new(
        name: &str,
        mode_id: &str,
        x_pos: i32,
        y_pos: i32,
        scale: f64,
        transform: u32,
        primary: bool,
    ) -> LogicalMonitor {
        LogicalMonitor {
            x_pos,
            y_pos,
            scale,
            transform,
            primary,
            monitors: vec![(
                name.to_string(),
                mode_id.to_string(),
                String::new(),
                String::new(),
            )],
            properties: LogicalMonitorProperties {
                dummy: None,
                dummy2: None,
            },
        }
    }
}

#[cfg(test)]
impl MonitorApply {
    pub(crate) fn test_new(
        name: &str,
        mode_id: &str,
        x_pos: i32,
        y_pos: i32,
        scale: f64,
        transform: u32,
        primary: bool,
    ) -> MonitorApply {
        MonitorApply {
            x_pos,
            y_pos,
            scale,
            transform,
            primary,
            monitors: vec![(
                name.to_string(),
                mode_id.to_string(),
                MonitorProperties {
                    width: None,
                    height: None,
                    underscanning: None,
                    builtin: Some(false),
                    max_size: None,
                    name: Some(String::new()),
                },
            )],
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

    pub fn search_monitor<'a>(&self, monitors: &'a [Monitor]) -> Option<&'a Monitor> {
        monitors
            .iter()
            .find(|mon| mon.description.0 == self.monitors[0].0)
    }

    pub fn search_logical_monitor<'a>(
        &self,
        logical_monitors: &'a [LogicalMonitor],
    ) -> Option<&'a LogicalMonitor> {
        logical_monitors
            .iter()
            .find(|mon| mon.monitors[0].0 == self.monitors[0].0)
    }

    pub fn save_kanshi(&self, kanshi_file: &mut Vec<u8>, monitor: &Monitor) {
        let _ = <Self as KanshiProfileEntry>::write_kanshi(self, kanshi_file, monitor);
    }

    pub fn verify(
        &self,
        _sway_connect: &Arc<Mutex<Connection>>,
        monitors: &[Monitor],
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

impl KanshiProfileEntry for MonitorApply {
    fn find_monitor<'a>(&self, monitors: &'a [Monitor]) -> Option<&'a Monitor> {
        self.search_monitor(monitors)
    }

    fn write_kanshi(&self, kanshi_file: &mut Vec<u8>, monitor: &Monitor) -> bool {
        let dpy_name = monitor.get_dpy_name();
        let mode = match self.get_modestr(monitor) {
            Some(x) => x,
            _ => return false,
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
        true
    }
}

impl KanshiProfileEntry for LogicalMonitor {
    fn find_monitor<'a>(&self, monitors: &'a [Monitor]) -> Option<&'a Monitor> {
        let monitor_name = &self.monitors[0].0;
        monitors
            .iter()
            .find(|mon| mon.description.0 == *monitor_name)
    }

    fn write_kanshi(&self, kanshi_file: &mut Vec<u8>, monitor: &Monitor) -> bool {
        let dpy_name = monitor.get_dpy_name();
        let mode = monitor.get_current_mode();
        let transform =
            MonitorTransform::from_u32(self.transform).unwrap_or(MonitorTransform::Normal);
        let config = if mode == "Unknown" {
            format!(
                "output \"{}\" position {},{} transform {} scale {} enable",
                dpy_name,
                self.x_pos,
                self.y_pos,
                transform.to_sway(),
                self.scale
            )
        } else {
            format!(
                "output \"{}\" mode {} position {},{} transform {} scale {} enable",
                dpy_name,
                mode,
                self.x_pos,
                self.y_pos,
                transform.to_sway(),
                self.scale
            )
        };
        writeln!(kanshi_file, "\t{config}").unwrap();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{LogicalMonitor, Monitor};
    use crate::wayland_observer::{OutputHeadSnapshot, OutputModeSnapshot};

    fn wayland_head(name: &str) -> OutputHeadSnapshot {
        OutputHeadSnapshot {
            name: name.to_string(),
            description: Some("Desk display".to_string()),
            enabled: true,
            position: Some((320, 180)),
            transform: Some(3),
            scale: Some(1.25),
            current_mode: Some(OutputModeSnapshot {
                width: 2560,
                height: 1440,
                refresh_mhz: Some(144_000),
                preferred: true,
                current: true,
            }),
            modes: vec![
                OutputModeSnapshot {
                    width: 1920,
                    height: 1080,
                    refresh_mhz: Some(60_000),
                    preferred: false,
                    current: false,
                },
                OutputModeSnapshot {
                    width: 2560,
                    height: 1440,
                    refresh_mhz: Some(144_000),
                    preferred: true,
                    current: true,
                },
            ],
        }
    }

    #[test]
    fn builds_monitor_from_wayland_snapshot_without_inventing_hardware_identity() {
        let monitor = Monitor::from_snapshot(&wayland_head("DP-1"));

        assert_eq!(
            monitor.description,
            (
                "Desk display".to_string(),
                String::new(),
                String::new(),
                String::new()
            )
        );
        assert_eq!(monitor.get_dpy_name(), "Desk display");
        assert_eq!(monitor.properties.name.as_deref(), Some("Desk display"));
    }

    #[test]
    fn selects_current_mode_from_wayland_snapshot() {
        let monitor = Monitor::from_snapshot(&wayland_head("DP-2"));

        assert_eq!(monitor.get_current_mode(), "2560x1440@144Hz");
        assert_eq!(
            monitor.modes.iter().filter(|mode| mode.current()).count(),
            1
        );
    }

    #[test]
    fn builds_logical_monitor_from_wayland_snapshot_with_fractional_scale_and_position() {
        let logical = LogicalMonitor::from_snapshot(&wayland_head("HDMI-A-1")).unwrap();

        assert_eq!(logical.get_dpy_name(), "Desk display");
        assert_eq!(logical.scale, 1.25);
        assert_eq!((logical.x_pos, logical.y_pos), (320, 180));
        assert_eq!(logical.transform, 3);
        assert_eq!(
            logical.monitors,
            vec![(
                "Desk display".to_string(),
                String::new(),
                String::new(),
                String::new()
            )]
        );
    }

    #[test]
    fn falls_back_to_connector_name_without_wayland_description() {
        let mut head = wayland_head("HDMI-A-2");
        head.description = None;

        let monitor = Monitor::from_snapshot(&head);
        let logical = LogicalMonitor::from_snapshot(&head).unwrap();

        assert_eq!(monitor.get_dpy_name(), "HDMI-A-2");
        assert_eq!(logical.get_dpy_name(), "HDMI-A-2");
    }

    #[test]
    fn skips_disabled_wayland_heads_for_logical_monitors() {
        let mut head = wayland_head("eDP-1");
        head.enabled = false;

        assert!(LogicalMonitor::from_snapshot(&head).is_none());
    }

    #[test]
    fn skips_enabled_wayland_head_missing_scale() {
        let mut head = wayland_head("DP-3");
        head.scale = None;

        assert!(LogicalMonitor::from_snapshot(&head).is_none());
    }

    #[test]
    fn skips_enabled_wayland_head_missing_position() {
        let mut head = wayland_head("DP-4");
        head.position = None;

        assert!(LogicalMonitor::from_snapshot(&head).is_none());
    }

    #[test]
    fn skips_enabled_wayland_head_missing_transform() {
        let mut head = wayland_head("DP-5");
        head.transform = None;

        assert!(LogicalMonitor::from_snapshot(&head).is_none());
    }
}
