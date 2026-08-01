pub mod modes;
pub mod monitor;
pub mod wayland_observer;

use core::fmt;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use monitor::{KanshiProfileEntry, LogicalMonitor, Monitor, MonitorApply};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::Command;
use std::{error::Error, fs, path::PathBuf, sync::Arc, time::Duration};
use swayipc_async::Connection;
use tokio::sync::Mutex;
use zbus::{dbus_interface, ConnectionBuilder, SignalContext};
use zvariant::{DeserializeDict, SerializeDict, Type};

use crate::wayland_observer::OutputSnapshot;

lazy_static! {
    static ref ZBUS_CONNECTION: Arc<Mutex<Option<zbus::Connection>>> = Arc::new(Mutex::new(None));
}

/// Stores configrations, interacts with sway IPC and monitors hardware changes
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
pub struct DisplayManager {
    serial: u32,
    monitors: Vec<Monitor>,
    logical_monitors: Vec<LogicalMonitor>,
    properties: DisplayManagerProperties,
}

pub fn wayland_side_effects_required(state_changed: bool, pending_side_effects: bool) -> bool {
    state_changed || pending_side_effects
}

/// DBus Interface for providing bindings
pub struct DisplayServer {
    manager: Arc<Mutex<DisplayManager>>,
    // TODO: Make independent of sway
    sway_connection: Option<Arc<Mutex<Connection>>>,
}

#[derive(Debug, Clone, SerializeDict, DeserializeDict, Type, PartialEq)]
#[zvariant(signature = "dict")]
pub struct DisplayManagerProperties {
    #[zvariant(rename = "layout-mode")]
    layout: Option<u32>,
    #[zvariant(rename = "supports-changing-layout-mode")]
    support_layout_change: Option<bool>,
    #[zvariant(rename = "global-scale-required")]
    global_scale: Option<bool>,
    #[zvariant(rename = "legacy-ui-scaling-factor")]
    legacy_scale_factor: Option<i32>,
}

#[derive(Debug)]
pub struct ServerError {
    description: String,
}

pub struct KanshiPaths {
    profiles: PathBuf,
    config: PathBuf,
}

#[dbus_interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayServer {
    pub async fn get_current_state(&mut self) -> DisplayManager {
        info!("Recieved 'GetCurrentState' request from control-center");
        let manager_ref = self.manager.lock().await;
        manager_ref.clone()
    }

    pub async fn apply_monitors_config(
        &mut self,
        serial: u32,
        method: u32,
        mutter_logical_monitors: Vec<MonitorApply>,
        properties: DisplayManagerProperties,
    ) -> zbus::fdo::Result<()> {
        debug!("Configuration Method: {method}");
        let sway_connection = self.sway_connection.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed(String::from("Sway IPC backend is unavailable"))
        })?;
        let (current_serial, monitors) = {
            let manager_obj = self.manager.lock().await;
            (manager_obj.serial, manager_obj.monitors.clone())
        };
        debug!("Serial: {} {}", current_serial, serial);
        if serial != current_serial {
            error!("Invalid configuration recieved for method apply_monitors_config: Wrong serial");
            return Err(zbus::fdo::Error::InvalidArgs(String::from("Wrong serial")));
        }

        for mutter_logical_mointor in &mutter_logical_monitors {
            // If apply_monitors_config called with method == 0 (Verify configuration)
            if method == 0 {
                match mutter_logical_mointor.verify(sway_connection, &monitors) {
                    Ok(_) => {
                        continue;
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
        }
        if method == 0 {
            return Ok(());
        }

        let profile_name = profile_name_for_monitors(&monitors);
        let profile_text = kanshi_profile_text(&monitors, &mutter_logical_monitors);
        info!("Profile FileName: {profile_name}");

        self.manager.lock().await.properties = properties;

        let profile_changed =
            match write_kanshi_profile_if_changed(&profile_name, &profile_text).await {
                Ok(changed) => changed,
                Err(e) => {
                    error!("Error writing data to kanshi config file: {e}");
                    return Err(zbus::fdo::Error::IOError(e.to_string()));
                }
            };

        if profile_changed {
            if let Err(e) = reload_kanshi().await {
                error!("Error reloading kanshi configuration: {e}");
            }
        }
        if let Err(e) = DisplayManager::get_monitor_info(sway_connection).await {
            error!("Error getting output information from sway: {e}");
        }
        DisplayManager::emit_monitors_changed().await?;
        Ok(())
    }

    #[dbus_interface(property)]
    pub async fn apply_monitors_config_allowed(&self) -> bool {
        info!("Call to apply_monitors_config");
        self.sway_connection.is_some()
    }

    #[dbus_interface(signal)]
    pub async fn monitors_changed(&self, ctxt: &SignalContext<'_>) -> zbus::Result<()>;
}

impl DisplayServer {
    pub async fn new(
        manager: Arc<Mutex<DisplayManager>>,
        sway_connection: Option<Arc<Mutex<Connection>>>,
    ) -> DisplayServer {
        DisplayServer {
            manager,
            sway_connection,
        }
    }
    pub async fn run_server(self) -> Result<(), Box<dyn Error>> {
        info!("Starting display daemon");
        if let Some(sway_connection) = &self.sway_connection {
            DisplayManager::get_monitor_info(sway_connection).await?;
        }

        let mut connection = ZBUS_CONNECTION.lock().await;
        *connection = Some(
            ConnectionBuilder::session()?
                .name("org.gnome.Mutter.DisplayConfig")?
                .serve_at("/org/gnome/Mutter/DisplayConfig", self)?
                .build()
                .await?,
        );
        Ok(())
    }
}
impl DisplayManager {
    pub async fn new() -> DisplayManager {
        DisplayManager {
            serial: 0,
            monitors: Vec::new(),
            logical_monitors: Vec::new(),
            properties: DisplayManagerProperties::new(),
        }
    }

    pub async fn watch_changes(
        manager_obj: Arc<Mutex<DisplayManager>>,
        sway_connection: Option<Arc<Mutex<Connection>>>,
    ) -> Result<(), Box<dyn Error>> {
        let Some(sway_connection) = sway_connection else {
            return Ok(());
        };
        let display_info = DisplayManager::get_monitor_info(&sway_connection).await?;
        let mut prev_monitor_set: HashSet<Monitor> = display_info.0.iter().cloned().collect();
        let mut prev_logical_monitor_set: HashSet<LogicalMonitor> =
            display_info.1.iter().cloned().collect();
        {
            let mut manager_obj_lock = manager_obj.lock().await;
            manager_obj_lock.monitors = display_info.0;
            manager_obj_lock.logical_monitors = display_info.1;
        }
        loop {
            tokio::time::sleep(Duration::from_millis(700)).await;
            let display_info = DisplayManager::get_monitor_info(&sway_connection).await?;
            let mut monitor_set = HashSet::new();
            let mut logical_monitor_set = HashSet::new();
            for monitor in &display_info.0 {
                monitor_set.insert(monitor.clone());
            }
            for logical_monitor in &display_info.1 {
                logical_monitor_set.insert(logical_monitor.clone());
            }
            if display_state_changed(
                &prev_monitor_set,
                &prev_logical_monitor_set,
                &monitor_set,
                &logical_monitor_set,
            ) {
                let profile = observed_profile(&display_info.0, &display_info.1);
                {
                    let mut manager_obj_lock = manager_obj.lock().await;
                    manager_obj_lock.monitors = display_info.0;
                    manager_obj_lock.logical_monitors = display_info.1;
                    debug!("monitors info: {:#?}", manager_obj_lock.monitors);
                    debug!("logical monitors: {:#?}", manager_obj_lock.logical_monitors);
                }
                prev_monitor_set = monitor_set;
                prev_logical_monitor_set = logical_monitor_set;
                if let Some((profile_name, profile_text)) = profile {
                    if write_kanshi_profile_if_changed(&profile_name, &profile_text).await? {
                        reload_kanshi().await?;
                    }
                }
                Self::emit_monitors_changed().await?;
            }
        }
    }

    pub async fn emit_monitors_changed() -> zbus::Result<()> {
        let connection = ZBUS_CONNECTION.lock().await.clone();
        info!("Emiting monitor changed");
        if let Some(con) = connection.as_ref() {
            con.emit_signal(
                Option::<&str>::None,
                "/org/gnome/Mutter/DisplayConfig",
                "org.gnome.Mutter.DisplayConfig",
                "MonitorsChanged",
                &(),
            )
            .await?;
        }
        Ok(())
    }

    /// Returns list of all monitors and logical monitors
    pub async fn get_monitor_info(
        sway_connection: &Arc<Mutex<Connection>>,
    ) -> Result<(Vec<Monitor>, Vec<LogicalMonitor>), Box<dyn Error>> {
        let outputs = sway_connection.lock().await.get_outputs().await?;
        let monitors = outputs.iter().map(|o| Monitor::new(o)).collect();
        let logical_monitors = outputs
            .iter()
            .filter(|o| o.active)
            .map(|o| LogicalMonitor::new(o))
            .collect();
        Ok((monitors, logical_monitors))
    }

    pub async fn install_wayland_snapshot(
        manager_obj: Arc<Mutex<DisplayManager>>,
        snapshot: &OutputSnapshot,
    ) -> Result<bool, String> {
        let mut manager = manager_obj.lock().await;
        manager.replace_from_wayland_snapshot(snapshot)
    }

    pub async fn persist_wayland_state(manager_obj: Arc<Mutex<DisplayManager>>) -> bool {
        let profile = {
            let manager = manager_obj.lock().await;
            observed_profile(&manager.monitors, &manager.logical_monitors)
        };
        if let Some((name, text)) = profile {
            match write_kanshi_profile_if_changed(&name, &text).await {
                Ok(true) => {
                    if let Err(error) = reload_kanshi().await {
                        error!("Error reloading kanshi configuration: {}", error);
                        return true;
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    error!("Error writing data to kanshi config file: {}", error);
                    return true;
                }
            }
        }
        if let Err(error) = Self::emit_monitors_changed().await {
            error!("Error emitting MonitorsChanged: {}", error);
            return true;
        }
        false
    }

    pub fn replace_from_wayland_snapshot(
        &mut self,
        snapshot: &OutputSnapshot,
    ) -> Result<bool, String> {
        for head in snapshot.heads.iter().filter(|head| head.enabled) {
            if head.scale.is_none() {
                return Err(format!(
                    "Wayland snapshot missing scale for enabled output {}",
                    head.name
                ));
            }
            if head.position.is_none() {
                return Err(format!(
                    "Wayland snapshot missing position for enabled output {}",
                    head.name
                ));
            }
            if head.transform.is_none() {
                return Err(format!(
                    "Wayland snapshot missing transform for enabled output {}",
                    head.name
                ));
            }
        }

        let monitors = snapshot.heads.iter().map(Monitor::from_snapshot).collect();
        let logical_monitors = snapshot
            .heads
            .iter()
            .filter_map(LogicalMonitor::from_snapshot)
            .collect();

        let changed = self.monitors != monitors || self.logical_monitors != logical_monitors;
        self.serial = snapshot.serial;
        self.monitors = monitors;
        self.logical_monitors = logical_monitors;
        Ok(changed)
    }
}
impl DisplayManagerProperties {
    pub fn new() -> DisplayManagerProperties {
        DisplayManagerProperties {
            layout: Some(1),
            support_layout_change: Some(true),
            global_scale: Some(false),
            legacy_scale_factor: Some(1),
        }
    }
}

impl ServerError {
    fn _produce_error(err: &str) -> ServerError {
        ServerError {
            description: err.to_string(),
        }
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description)
    }
}

impl Error for ServerError {
    fn description(&self) -> &str {
        &self.description
    }
}

pub async fn get_kanshi_paths() -> zbus::Result<KanshiPaths> {
    let env_vars: HashMap<String, String> = std::env::vars().collect();
    let home_dir = env_vars.get("HOME").expect("$HOME not defined");
    let default_path = format!("{home_dir}/.config/regolith3/kanshi");
    let base: PathBuf = match trawlcat::rescat("kanshi.path", Some(default_path.clone())).await {
        Ok(path) => match path.try_into() {
            Ok(path_buf) => path_buf,
            Err(e) => {
                warn!("Error: {e}");
                default_path.into()
            }
        },
        Err(e) => {
            warn!("Error: {e}");
            default_path.into()
        }
    };
    let profiles = base.join("profiles");
    let config = base.join("config");
    return Ok(KanshiPaths { profiles, config });
}

pub async fn reload_kanshi() -> zbus::Result<()> {
    let KanshiPaths { config, .. } = get_kanshi_paths().await?;
    let default_config_path = String::from("~/.config/regolith3/kanshi/config");
    let config_path: String = config
        .into_os_string()
        .into_string()
        .unwrap_or(default_config_path);
    Command::new("killall").arg("kanshi").spawn()?;
    Command::new("kanshi").arg("-c").arg(&config_path).spawn()?;
    Ok(())
}

fn profile_name_for_monitors(monitors: &[Monitor]) -> String {
    let mut display_names = monitors
        .iter()
        .map(|monitor| monitor.get_dpy_name())
        .collect::<Vec<String>>();
    display_names.sort();
    display_names
        .into_iter()
        .map(|display_name| display_name.replace(" ", "_"))
        .collect::<Vec<String>>()
        .join("__")
}

fn kanshi_profile_text<T: KanshiProfileEntry>(
    monitors: &[Monitor],
    logical_monitors: &[T],
) -> String {
    let mut profile_buf = Vec::new();
    write_kanshi_profile(&mut profile_buf, monitors, logical_monitors);
    String::from_utf8(profile_buf).expect("Generated kanshi profile should be valid UTF-8")
}

fn write_kanshi_profile<T: KanshiProfileEntry>(
    profile_buf: &mut Vec<u8>,
    monitors: &[Monitor],
    logical_monitors: &[T],
) {
    let mut active_physical_monitors = Vec::new();

    writeln!(profile_buf, "profile {{").unwrap();
    for logical_monitor in logical_monitors {
        let Some(sway_physical_monitor) = logical_monitor.find_monitor(monitors) else {
            continue;
        };

        if logical_monitor.write_kanshi(profile_buf, sway_physical_monitor) {
            active_physical_monitors.push(sway_physical_monitor.clone());
        }
    }

    for disabled_mon in get_disabled_monitors(monitors, &active_physical_monitors) {
        writeln!(
            profile_buf,
            "\toutput \"{}\" disable",
            disabled_mon.get_dpy_name()
        )
        .expect("Failed to write to file");
    }
    writeln!(profile_buf, "}}").unwrap();
}

fn get_disabled_monitors<'a>(
    monitors: &'a [Monitor],
    active_physical_monitors: &[Monitor],
) -> Vec<&'a Monitor> {
    monitors
        .iter()
        .filter(|mon| !active_physical_monitors.contains(mon))
        .collect()
}

fn display_state_changed(
    prev_monitors: &HashSet<Monitor>,
    prev_logical_monitors: &HashSet<LogicalMonitor>,
    current_monitors: &HashSet<Monitor>,
    current_logical_monitors: &HashSet<LogicalMonitor>,
) -> bool {
    prev_monitors != current_monitors || prev_logical_monitors != current_logical_monitors
}

fn observed_profile<T: KanshiProfileEntry>(
    monitors: &[Monitor],
    logical_monitors: &[T],
) -> Option<(String, String)> {
    if monitors.is_empty() {
        return None;
    }

    Some((
        profile_name_for_monitors(monitors),
        kanshi_profile_text(monitors, logical_monitors),
    ))
}

async fn write_kanshi_profile_if_changed(
    profile_name: &str,
    profile_text: &str,
) -> Result<bool, Box<dyn Error>> {
    let kanshi_paths = get_kanshi_paths().await?;
    fs::create_dir_all(&kanshi_paths.profiles)?;
    let profile_path = kanshi_paths.profiles.join(profile_name);

    if let Ok(existing_profile) = fs::read_to_string(&profile_path) {
        if existing_profile == profile_text {
            debug!("Kanshi profile unchanged; skipping write");
            return Ok(false);
        }
    }

    fs::write(&profile_path, profile_text)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::Modes;
    use crate::monitor::{LogicalMonitor, Monitor, MonitorApply};
    use crate::wayland_observer::{OutputHeadSnapshot, OutputModeSnapshot, OutputSnapshot};

    #[tokio::test]
    async fn run_server_without_sway_connection_registers_dbus_server() {
        let manager = Arc::new(Mutex::new(DisplayManager::new().await));
        let server = DisplayServer::new(manager, None).await;

        assert!(server.run_server().await.is_ok());
    }

    #[tokio::test]
    async fn apply_without_sway_connection_returns_backend_error() {
        let manager = Arc::new(Mutex::new(DisplayManager::new().await));
        let mut server = DisplayServer::new(manager, None).await;

        let result = server
            .apply_monitors_config(0, 0, Vec::new(), DisplayManagerProperties::new())
            .await;

        assert!(matches!(
            result,
            Err(zbus::fdo::Error::Failed(message))
                if message == "Sway IPC backend is unavailable"
        ));
    }

    #[tokio::test]
    async fn apply_monitors_config_is_not_allowed_without_sway_connection() {
        let manager = Arc::new(Mutex::new(DisplayManager::new().await));
        let server = DisplayServer::new(manager, None).await;

        assert!(!server.apply_monitors_config_allowed().await);
    }

    fn build_manager(
        monitors: Vec<Monitor>,
        logical_monitors: Vec<LogicalMonitor>,
    ) -> DisplayManager {
        DisplayManager {
            serial: 1,
            monitors,
            logical_monitors,
            properties: DisplayManagerProperties::new(),
        }
    }

    fn snapshot_head(
        name: &str,
        enabled: bool,
        position: Option<(i32, i32)>,
        transform: Option<u32>,
        scale: Option<f64>,
        width: i32,
        height: i32,
        refresh_mhz: Option<i32>,
    ) -> OutputHeadSnapshot {
        OutputHeadSnapshot {
            name: name.to_string(),
            description: Some(format!("{name} description")),
            enabled,
            position,
            transform,
            scale,
            current_mode: Some(OutputModeSnapshot {
                width,
                height,
                refresh_mhz,
                preferred: true,
                current: true,
            }),
            modes: vec![OutputModeSnapshot {
                width,
                height,
                refresh_mhz,
                preferred: true,
                current: true,
            }],
        }
    }

    #[test]
    fn renders_active_and_disabled_outputs() {
        let active_mode = Modes::test_new("1024x768@60Hz");
        let disabled_mode = Modes::test_new("1920x1080@60Hz");
        let active_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![active_mode.clone()],
        );
        let disabled_monitor =
            Monitor::test_new("HDMI-A-1", "Projector", "Room", "B2", vec![disabled_mode]);
        let logical_monitors = vec![LogicalMonitor::test_new(
            "eDP-1",
            "1024x768@60Hz",
            10,
            20,
            1.0,
            0,
            true,
        )];
        let manager = build_manager(vec![active_monitor, disabled_monitor], logical_monitors);

        let profile = kanshi_profile_text(&manager.monitors, &manager.logical_monitors);

        assert_eq!(
            profile,
            "profile {\n\
\toutput \"Regolith Panel A1\" mode 1024x768@60Hz position 10,20 transform normal scale 1 enable\n\
\toutput \"Projector Room B2\" disable\n\
}\n"
        );
    }

    #[test]
    fn replaces_manager_state_from_single_wayland_snapshot() {
        let mut manager = build_manager(Vec::new(), Vec::new());
        let snapshot = OutputSnapshot {
            serial: 42,
            heads: vec![snapshot_head(
                "HDMI-A-1",
                true,
                Some((100, 200)),
                Some(0),
                Some(1.5),
                2560,
                1440,
                Some(144_000),
            )],
        };

        assert!(manager.replace_from_wayland_snapshot(&snapshot).unwrap());
        assert!(!manager.replace_from_wayland_snapshot(&snapshot).unwrap());

        assert_eq!(manager.serial, 42);
        assert_eq!(manager.monitors.len(), 1);
        assert_eq!(manager.logical_monitors.len(), 1);
        assert_eq!(manager.monitors[0].get_dpy_name(), "HDMI-A-1");
        assert_eq!(manager.monitors[0].get_current_mode(), "2560x1440@144Hz");
        assert_eq!(
            manager.logical_monitors[0],
            LogicalMonitor::test_new("HDMI-A-1", "", 100, 200, 1.5, 0, false)
        );
    }

    #[test]
    fn retries_side_effects_after_a_failed_prior_attempt() {
        assert!(!wayland_side_effects_required(false, false));
        assert!(wayland_side_effects_required(true, false));
        assert!(wayland_side_effects_required(false, true));
    }

    #[test]
    fn preserves_snapshot_head_order_when_replacing_manager_state() {
        let mut manager = build_manager(Vec::new(), Vec::new());
        let snapshot = OutputSnapshot {
            serial: 7,
            heads: vec![
                snapshot_head(
                    "DP-1",
                    true,
                    Some((0, 0)),
                    Some(0),
                    Some(1.0),
                    2256,
                    1504,
                    Some(60_000),
                ),
                snapshot_head(
                    "HDMI-A-1",
                    false,
                    None,
                    Some(0),
                    None,
                    1920,
                    1080,
                    Some(60_000),
                ),
                snapshot_head(
                    "eDP-1",
                    true,
                    Some((2256, 0)),
                    Some(0),
                    Some(1.25),
                    2880,
                    1800,
                    Some(90_000),
                ),
            ],
        };

        manager.replace_from_wayland_snapshot(&snapshot).unwrap();

        assert_eq!(
            manager
                .monitors
                .iter()
                .map(|monitor| monitor.get_dpy_name())
                .collect::<Vec<_>>(),
            vec![
                "DP-1".to_string(),
                "HDMI-A-1".to_string(),
                "eDP-1".to_string()
            ]
        );
        assert_eq!(
            manager
                .logical_monitors
                .iter()
                .map(|logical| logical.get_dpy_name())
                .collect::<Vec<_>>(),
            vec!["DP-1".to_string(), "eDP-1".to_string()]
        );
    }

    #[test]
    fn unknown_wayland_refresh_keeps_current_mode_unknown_and_omits_kanshi_mode_line() {
        let mut manager = build_manager(Vec::new(), Vec::new());
        let snapshot = OutputSnapshot {
            serial: 9,
            heads: vec![snapshot_head(
                "DP-6",
                true,
                Some((10, 20)),
                Some(0),
                Some(1.0),
                3440,
                1440,
                None,
            )],
        };

        manager.replace_from_wayland_snapshot(&snapshot).unwrap();

        assert_eq!(manager.monitors[0].get_current_mode(), "Unknown");
        assert_eq!(
            kanshi_profile_text(&manager.monitors, &manager.logical_monitors),
            "profile {\n\
\toutput \"DP-6\" position 10,20 transform normal scale 1 enable\n\
}\n"
        );
    }

    #[test]
    fn rejects_incomplete_enabled_wayland_snapshot_without_mutating_state_or_profile() {
        let active_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let disabled_monitor = Monitor::test_new(
            "HDMI-A-1",
            "Projector",
            "Room",
            "B2",
            vec![Modes::test_new("1920x1080@60Hz")],
        );
        let mut manager = build_manager(
            vec![active_monitor, disabled_monitor],
            vec![LogicalMonitor::test_new(
                "eDP-1",
                "1024x768@60Hz",
                10,
                20,
                1.0,
                0,
                true,
            )],
        );
        let previous_manager = manager.clone();
        let previous_profile = kanshi_profile_text(&manager.monitors, &manager.logical_monitors);
        let snapshot = OutputSnapshot {
            serial: 42,
            heads: vec![snapshot_head(
                "eDP-1",
                true,
                Some((100, 200)),
                Some(0),
                None,
                2560,
                1440,
                Some(144_000),
            )],
        };

        let result = manager.replace_from_wayland_snapshot(&snapshot);

        assert_eq!(
            result,
            Err("Wayland snapshot missing scale for enabled output eDP-1".to_string())
        );
        assert_eq!(manager, previous_manager);
        assert_eq!(
            kanshi_profile_text(&manager.monitors, &manager.logical_monitors),
            previous_profile
        );
        assert!(!previous_profile.contains("output \"Regolith Panel A1\" disable"));
    }

    #[test]
    fn renders_all_monitors_disabled_without_logical_state() {
        let monitor_a = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let monitor_b = Monitor::test_new(
            "HDMI-A-1",
            "Projector",
            "Room",
            "B2",
            vec![Modes::test_new("1920x1080@60Hz")],
        );
        let manager = build_manager(vec![monitor_a, monitor_b], Vec::new());

        let profile = kanshi_profile_text(&manager.monitors, &manager.logical_monitors);

        assert_eq!(
            profile,
            "profile {\n\
\toutput \"Regolith Panel A1\" disable\n\
\toutput \"Projector Room B2\" disable\n\
}\n"
        );
    }

    #[test]
    fn renders_apply_and_observed_state_with_one_helper() {
        let active_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let disabled_monitor = Monitor::test_new(
            "HDMI-A-1",
            "Projector",
            "Room",
            "B2",
            vec![Modes::test_new("1920x1080@60Hz")],
        );
        let manager = build_manager(
            vec![active_monitor, disabled_monitor],
            vec![LogicalMonitor::test_new(
                "eDP-1", "ignored", 10, 20, 1.0, 0, true,
            )],
        );
        let apply = MonitorApply::test_new("eDP-1", "1024x768@60Hz", 10, 20, 1.0, 0, true);

        let observed_profile = kanshi_profile_text(&manager.monitors, &manager.logical_monitors);
        let apply_profile = kanshi_profile_text(&manager.monitors, &[apply]);

        assert_eq!(apply_profile, observed_profile);
        assert_eq!(
            apply_profile,
            "profile {\n\
\toutput \"Regolith Panel A1\" mode 1024x768@60Hz position 10,20 transform normal scale 1 enable\n\
\toutput \"Projector Room B2\" disable\n\
}\n"
        );
    }

    #[test]
    fn detects_monitor_removal_as_change() {
        let prev_monitors = HashSet::from([
            Monitor::test_new(
                "eDP-1",
                "Regolith",
                "Panel",
                "A1",
                vec![Modes::test_new("1024x768@60Hz")],
            ),
            Monitor::test_new(
                "HDMI-A-1",
                "Projector",
                "Room",
                "B2",
                vec![Modes::test_new("1920x1080@60Hz")],
            ),
        ]);
        let prev_logical_monitors = HashSet::from([LogicalMonitor::test_new(
            "eDP-1",
            "1024x768@60Hz",
            10,
            20,
            1.0,
            0,
            true,
        )]);
        let current_monitors = HashSet::from([Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        )]);
        let current_logical_monitors = HashSet::from([LogicalMonitor::test_new(
            "eDP-1",
            "1024x768@60Hz",
            10,
            20,
            1.0,
            0,
            true,
        )]);

        assert!(display_state_changed(
            &prev_monitors,
            &prev_logical_monitors,
            &current_monitors,
            &current_logical_monitors,
        ));
    }

    #[test]
    fn detects_current_mode_change_for_same_output_identity() {
        let previous = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let current = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1920x1080@60Hz")],
        );
        let empty = HashSet::new();

        assert!(display_state_changed(
            &HashSet::from([previous]),
            &empty,
            &HashSet::from([current]),
            &empty,
        ));
    }

    #[test]
    fn equal_monitors_have_equal_hashes() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let left = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let right = left.clone();
        let mut left_hash = DefaultHasher::new();
        let mut right_hash = DefaultHasher::new();
        left.hash(&mut left_hash);
        right.hash(&mut right_hash);

        assert_eq!(left, right);
        assert_eq!(left_hash.finish(), right_hash.finish());
    }

    #[test]
    fn equal_logical_monitors_have_equal_hashes() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let left = LogicalMonitor::test_new("eDP-1", "ignored", 10, 20, 1.25, 0, true);
        let right = left.clone();
        let mut left_hash = DefaultHasher::new();
        let mut right_hash = DefaultHasher::new();
        left.hash(&mut left_hash);
        right.hash(&mut right_hash);

        assert_eq!(left, right);
        assert_eq!(left_hash.finish(), right_hash.finish());
    }

    #[test]
    fn detects_logical_monitor_identity_change_as_change() {
        let previous = LogicalMonitor::test_new("eDP-1", "ignored", 10, 20, 1.0, 0, true);
        let current = LogicalMonitor::test_new("HDMI-A-1", "ignored", 10, 20, 1.0, 0, true);
        let empty = HashSet::new();

        assert_ne!(previous, current);
        assert!(display_state_changed(
            &empty,
            &HashSet::from([previous]),
            &empty,
            &HashSet::from([current]),
        ));
    }

    #[test]
    fn fractional_scale_is_preserved_in_observed_profile() {
        let monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1920x1080@60Hz")],
        );
        let logical = LogicalMonitor::test_new("eDP-1", "ignored", 0, 0, 1.25, 0, true);

        let profile = kanshi_profile_text(&[monitor], &[logical]);

        assert!(profile.contains("scale 1.25 enable"));
    }

    #[test]
    fn empty_monitor_transition_does_not_persist_profile() {
        assert!(observed_profile::<LogicalMonitor>(&[], &[]).is_none());
        assert!(observed_profile::<LogicalMonitor>(
            &[Monitor::test_new(
                "eDP-1",
                "Regolith",
                "Panel",
                "A1",
                vec![Modes::test_new("1024x768@60Hz")],
            )],
            &[],
        )
        .is_some());
    }

    #[test]
    fn active_output_with_unknown_mode_is_enabled_not_disabled() {
        let monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new_without_current("1024x768@60Hz")],
        );
        let logical = LogicalMonitor::test_new("eDP-1", "ignored", 10, 20, 1.0, 0, true);

        let profile = kanshi_profile_text(&[monitor], &[logical]);

        assert!(profile.contains(
            "output \"Regolith Panel A1\" position 10,20 transform normal scale 1 enable"
        ));
        assert!(!profile.contains("output \"Regolith Panel A1\" disable"));
    }
}
