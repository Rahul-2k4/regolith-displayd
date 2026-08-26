pub mod modes;
pub mod monitor;
pub mod wayland_observer;

use core::fmt;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use monitor::{KanshiProfileEntry, LogicalMonitor, Monitor, MonitorApply};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::process::Command;
use std::{error::Error, fs, path::PathBuf, sync::Arc, time::Duration};
use swayipc_async::Connection;
use tokio::sync::Mutex;
use zbus::{dbus_interface, ConnectionBuilder, SignalContext};
use zvariant::{DeserializeDict, SerializeDict, Type};

use crate::wayland_observer::{
    CosmicApplyRequest, CosmicHeadRequest, OutputSnapshot, WaylandApplyHandle,
};

lazy_static! {
    static ref ZBUS_CONNECTION: Arc<Mutex<Option<zbus::Connection>>> = Arc::new(Mutex::new(None));
}

const WATCH_RETRY_DELAY: Duration = Duration::from_millis(100);
const WATCH_RETRY_ATTEMPTS: usize = 3;
const WATCH_MAX_SIDE_EFFECT_ATTEMPTS: usize = WATCH_RETRY_ATTEMPTS * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaylandSideEffectStage {
    PersistProfile,
    ReloadKanshi,
    Signal,
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

pub fn wayland_reload_required(profile_changed: bool, pending_side_effects: bool) -> bool {
    profile_changed || pending_side_effects
}

pub fn wayland_side_effect_stage(
    state_changed: bool,
    pending_stage: Option<WaylandSideEffectStage>,
    profile_present: bool,
) -> Option<WaylandSideEffectStage> {
    if state_changed {
        Some(if profile_present {
            WaylandSideEffectStage::PersistProfile
        } else {
            WaylandSideEffectStage::Signal
        })
    } else {
        pending_stage
    }
}

pub fn wayland_stage_after_profile(_profile_changed: bool) -> WaylandSideEffectStage {
    WaylandSideEffectStage::Signal
}

pub fn wayland_stage_after_reload() -> WaylandSideEffectStage {
    WaylandSideEffectStage::Signal
}

/// The internal observer-thread builder now owns the protocol apply mechanics.
/// Keep the D-Bus boundary explicit until it is wired to that same thread.
pub fn cosmic_profile_apply_status() -> Result<(), &'static str> {
    Err("COSMIC profile apply is unavailable: the DisplayServer D-Bus boundary is not wired to the observer-thread OutputConfigurationRequest")
}

/// A validated output change that can be handed to a future Wayland apply
/// implementation. This is planning only; it does not contain protocol
/// objects and cannot change compositor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CosmicOutputPlan {
    pub name: String,
    pub position: (i32, i32),
    pub transform: u32,
    pub scale_milli: u32,
    pub mode: (i32, i32, Option<i32>),
}

/// Resolve a stored Mutter-style profile against an observed wlroots snapshot.
/// The returned plan is the explicit integration boundary for a future
/// `zwlr_output_manager_v1.create_configuration` implementation.
pub fn plan_cosmic_profile(
    snapshot: &OutputSnapshot,
    logical_monitors: &[MonitorApply],
) -> Result<Vec<CosmicOutputPlan>, String> {
    let mut plans = Vec::with_capacity(logical_monitors.len());

    for logical_monitor in logical_monitors {
        let (name, mode_id) = logical_monitor
            .output_name_and_mode()
            .ok_or_else(|| "COSMIC profile output has no physical monitor identity".to_string())?;
        let head = snapshot
            .heads
            .iter()
            .find(|head| head.name == *name)
            .ok_or_else(|| format!("COSMIC profile output is absent from snapshot: {name}"))?;
        if !head.enabled {
            return Err(format!("COSMIC profile output is disabled: {name}"));
        }
        if plans
            .iter()
            .any(|plan: &CosmicOutputPlan| plan.name == *name)
        {
            return Err(format!("COSMIC profile contains duplicate output: {name}"));
        }

        let dimensions = parse_mode_dimensions(mode_id)
            .ok_or_else(|| format!("COSMIC profile mode is malformed for {name}: {mode_id}"))?;
        let refresh_mhz = parse_mode_refresh(mode_id);
        let mode = head
            .modes
            .iter()
            .find(|mode| {
                (mode.width, mode.height) == dimensions
                    && (refresh_mhz.is_none() || mode.refresh_mhz == refresh_mhz)
            })
            .ok_or_else(|| {
                format!(
                    "COSMIC profile mode is unavailable for {name}: {}x{}",
                    dimensions.0, dimensions.1
                )
            })?;
        let scale_milli = (logical_monitor.scale() * 1000.0).round();
        if !scale_milli.is_finite() || scale_milli <= 0.0 || scale_milli > u32::MAX as f64 {
            return Err(format!("COSMIC profile scale is invalid for {name}"));
        }

        plans.push(CosmicOutputPlan {
            name: name.to_owned(),
            position: logical_monitor.position(),
            transform: logical_monitor.transform(),
            scale_milli: scale_milli as u32,
            mode: (mode.width, mode.height, mode.refresh_mhz),
        });
    }

    Ok(plans)
}

/// Builds a name-based COSMIC apply request from the requested logical
/// monitors, padding in every other known monitor as disabled so the
/// observer thread's `OutputConfigurationRequest::validate` (which requires
/// every retained head to be configured exactly once) can be satisfied.
/// Mirrors `write_kanshi_profile`'s `get_disabled_monitors` handling of the
/// same active/inactive split for the Sway path.
fn build_cosmic_apply_request(
    serial: u32,
    monitors: &[Monitor],
    mutter_logical_monitors: &[MonitorApply],
) -> Result<CosmicApplyRequest, String> {
    let mut heads = Vec::with_capacity(monitors.len());
    let mut configured_names = HashSet::new();

    for logical_monitor in mutter_logical_monitors {
        let (name, mode_id) = logical_monitor.output_name_and_mode().ok_or_else(|| {
            "COSMIC apply request output has no physical monitor identity".to_string()
        })?;
        if !configured_names.insert(name.to_string()) {
            return Err(format!(
                "COSMIC apply request contains duplicate output: {name}"
            ));
        }
        let dimensions = parse_mode_dimensions(mode_id).ok_or_else(|| {
            format!("COSMIC apply request mode is malformed for {name}: {mode_id}")
        })?;

        heads.push(CosmicHeadRequest {
            name: name.to_string(),
            enabled: true,
            mode: Some((dimensions.0, dimensions.1, parse_mode_refresh(mode_id))),
            position: logical_monitor.position(),
            transform: logical_monitor.transform(),
            scale: logical_monitor.scale(),
        });
    }

    for monitor in monitors {
        let name = monitor.get_dpy_name();
        if configured_names.contains(&name) {
            continue;
        }
        heads.push(CosmicHeadRequest {
            name,
            enabled: false,
            mode: None,
            position: (0, 0),
            transform: 0,
            scale: 1.0,
        });
    }

    Ok(CosmicApplyRequest { serial, heads })
}

fn parse_mode_dimensions(mode_id: &str) -> Option<(i32, i32)> {
    let dimensions = mode_id.split('@').next()?;
    let (width, height) = dimensions.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn parse_mode_refresh(mode_id: &str) -> Option<i32> {
    let refresh = mode_id.split_once("@")?.1.strip_suffix("Hz")?;
    Some((refresh.parse::<f64>().ok()? * 1000.0).round() as i32)
}

pub fn should_reload_kanshi(xdg_current_desktop: Option<&str>) -> bool {
    !xdg_current_desktop
        .map(|desktop| desktop.to_ascii_lowercase().contains("cosmic"))
        .unwrap_or(false)
}

/// DBus Interface for providing bindings
pub struct DisplayServer {
    manager: Arc<Mutex<DisplayManager>>,
    // TODO: Make independent of sway
    sway_connection: Option<Arc<Mutex<Connection>>>,
    /// Set explicitly via `with_cosmic_desktop` rather than read from
    /// `XDG_CURRENT_DESKTOP` here, to avoid a racy environment-variable read
    /// from within an async method under parallel tests.
    cosmic_desktop: bool,
    /// Handle for submitting apply/test requests into the dedicated Wayland
    /// observer thread. Only present when running under COSMIC.
    wayland_apply_handle: Option<WaylandApplyHandle>,
}
fn commit_refreshed_monitor_info(
    manager: &mut DisplayManager,
    properties: DisplayManagerProperties,
    refreshed: Result<(Vec<Monitor>, Vec<LogicalMonitor>), String>,
) -> Result<(), String> {
    let (monitors, logical_monitors) = refreshed?;
    manager.properties = properties;
    manager.monitors = monitors;
    manager.logical_monitors = logical_monitors;
    Ok(())
}

async fn apply_monitors_config_core<
    WriteProfile,
    WriteFuture,
    Reload,
    ReloadFuture,
    Refresh,
    RefreshFuture,
    Signal,
    SignalFuture,
>(
    manager: &mut DisplayManager,
    properties: DisplayManagerProperties,
    write_profile: WriteProfile,
    reload: Reload,
    refresh: Refresh,
    signal: Signal,
) -> zbus::fdo::Result<()>
where
    WriteProfile: FnOnce() -> WriteFuture,
    WriteFuture: Future<Output = Result<bool, Box<dyn Error>>>,
    Reload: FnOnce() -> ReloadFuture,
    ReloadFuture: Future<Output = zbus::fdo::Result<()>>,
    Refresh: FnOnce() -> RefreshFuture,
    RefreshFuture: Future<Output = Result<(Vec<Monitor>, Vec<LogicalMonitor>), String>>,
    Signal: FnOnce() -> SignalFuture,
    SignalFuture: Future<Output = zbus::fdo::Result<()>>,
{
    let profile_changed = write_profile()
        .await
        .map_err(|error| zbus::fdo::Error::IOError(error.to_string()))?;

    if profile_changed {
        reload().await?;
    }

    let refreshed = refresh().await.map_err(|error| {
        zbus::fdo::Error::Failed(format!(
            "Unable to refresh output information from sway: {error}"
        ))
    })?;
    commit_refreshed_monitor_info(manager, properties, Ok(refreshed)).map_err(|error| {
        zbus::fdo::Error::Failed(format!(
            "Unable to refresh output information from sway: {error}"
        ))
    })?;
    signal().await?;
    Ok(())
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
        if self.cosmic_desktop {
            return self
                .apply_monitors_config_cosmic(serial, method, mutter_logical_monitors)
                .await;
        }
        let sway_connection = self.sway_connection.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed(String::from("Sway IPC backend is unavailable"))
        })?;
        // Serialize validation, persistence, and refresh so watch_changes cannot
        // publish stale state while this request is being applied.
        let mut manager_obj = self.manager.lock().await;
        debug!("Serial: {} {}", manager_obj.serial, serial);
        if serial != manager_obj.serial {
            error!("Invalid configuration recieved for method apply_monitors_config: Wrong serial");
            return Err(zbus::fdo::Error::InvalidArgs(String::from("Wrong serial")));
        }

        for mutter_logical_mointor in &mutter_logical_monitors {
            // If apply_monitors_config called with method == 0 (Verify configuration)
            if method == 0 {
                match mutter_logical_mointor.verify(sway_connection, &manager_obj.monitors) {
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

        let profile_name = profile_name_for_monitors(&manager_obj.monitors);
        let profile_text = kanshi_profile_text(&manager_obj.monitors, &mutter_logical_monitors);
        info!("Profile FileName: {profile_name}");

        apply_monitors_config_core(
            &mut manager_obj,
            properties,
            || write_kanshi_profile_if_changed(&profile_name, &profile_text),
            || async {
                reload_kanshi().await.map_err(|error| {
                    zbus::fdo::Error::Failed(format!(
                        "Unable to reload kanshi configuration: {error}"
                    ))
                })
            },
            || async {
                DisplayManager::get_monitor_info(sway_connection)
                    .await
                    .map_err(|error| error.to_string())
            },
            || async {
                DisplayManager::emit_monitors_changed()
                    .await
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
            },
        )
        .await
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
            cosmic_desktop: false,
            wayland_apply_handle: None,
        }
    }

    /// Marks this server as running under COSMIC, routing
    /// `apply_monitors_config` through the Wayland observer thread instead
    /// of Sway/Kanshi.
    pub fn with_cosmic_desktop(mut self, cosmic_desktop: bool) -> DisplayServer {
        self.cosmic_desktop = cosmic_desktop;
        self
    }

    /// Attaches the handle used to submit apply/test requests into the
    /// dedicated Wayland observer thread.
    pub fn with_wayland_apply_handle(
        mut self,
        wayland_apply_handle: Option<WaylandApplyHandle>,
    ) -> DisplayServer {
        self.wayland_apply_handle = wayland_apply_handle;
        self
    }

    /// COSMIC counterpart of `apply_monitors_config`: builds a name-based
    /// `CosmicApplyRequest` from the requested logical monitors (padding in
    /// the remaining known outputs as disabled, mirroring the Sway path's
    /// Kanshi profile generation), then submits it to the Wayland observer
    /// thread and blocks (off the async executor) for the compositor's
    /// Succeeded/Failed/Cancelled outcome.
    async fn apply_monitors_config_cosmic(
        &mut self,
        serial: u32,
        method: u32,
        mutter_logical_monitors: Vec<MonitorApply>,
    ) -> zbus::fdo::Result<()> {
        let apply_handle = self.wayland_apply_handle.clone().ok_or_else(|| {
            zbus::fdo::Error::Failed("COSMIC Wayland apply handle is unavailable".to_string())
        })?;

        let manager_obj = self.manager.lock().await;
        if serial != manager_obj.serial {
            error!("Invalid configuration recieved for method apply_monitors_config: Wrong serial");
            return Err(zbus::fdo::Error::InvalidArgs(String::from("Wrong serial")));
        }

        let request =
            build_cosmic_apply_request(serial, &manager_obj.monitors, &mutter_logical_monitors)
                .map_err(zbus::fdo::Error::InvalidArgs)?;
        drop(manager_obj);

        let verify_only = method == 0;
        let outcome = tokio::task::spawn_blocking(move || apply_handle.apply(request, verify_only))
            .await
            .map_err(|error| {
                zbus::fdo::Error::Failed(format!("COSMIC apply task failed to join: {error}"))
            })?;
        outcome.map_err(zbus::fdo::Error::Failed)?;

        if verify_only {
            return Ok(());
        }

        // The Wayland observer thread's own `done` snapshot (installed via
        // `install_wayland_snapshot`) is the source of truth for refreshed
        // monitor state under COSMIC, so only the change signal is emitted
        // here, mirroring `wayland_stage_after_reload`/`Signal`.
        DisplayManager::emit_monitors_changed()
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }
    pub async fn run_server(self) -> Result<(), Box<dyn Error>> {
        info!("Starting display daemon");
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
    pub fn has_observed_outputs(&self) -> bool {
        !self.monitors.is_empty()
    }

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
            manager_obj_lock.replace_observed_state(display_info.0, display_info.1);
        }
        loop {
            tokio::time::sleep(Duration::from_millis(700)).await;
            let display_info = match DisplayManager::get_monitor_info(&sway_connection).await {
                Ok(display_info) => display_info,
                Err(error) => {
                    warn!("Unable to refresh output information from sway: {error}");
                    continue;
                }
            };
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
                let observed_monitors = display_info.0.clone();
                let observed_logical_monitors = display_info.1.clone();
                let manager_for_commit = Arc::clone(&manager_obj);
                let side_effects = retry_watch_side_effects(
                    profile
                        .as_ref()
                        .map(|(name, text)| (name.as_str(), text.as_str())),
                    || async {
                        write_kanshi_profile_if_changed(
                            profile
                                .as_ref()
                                .map(|(name, _)| name.as_str())
                                .unwrap_or_default(),
                            profile
                                .as_ref()
                                .map(|(_, text)| text.as_str())
                                .unwrap_or_default(),
                        )
                        .await
                        .map_err(|error| error.to_string())
                    },
                    || async { reload_kanshi().await.map_err(|error| error.to_string()) },
                    || {
                        let manager_for_commit = Arc::clone(&manager_for_commit);
                        let observed_monitors = observed_monitors.clone();
                        let observed_logical_monitors = observed_logical_monitors.clone();
                        async move {
                            let mut manager_obj_lock = manager_for_commit.lock().await;
                            manager_obj_lock.replace_observed_state(
                                observed_monitors,
                                observed_logical_monitors,
                            );
                            debug!("monitors info: {:#?}", manager_obj_lock.monitors);
                            debug!("logical monitors: {:#?}", manager_obj_lock.logical_monitors);
                            Ok(())
                        }
                    },
                    || async {
                        Self::emit_monitors_changed()
                            .await
                            .map_err(|error| error.to_string())
                    },
                )
                .await;
                if side_effects.is_err() {
                    continue;
                }
                prev_monitor_set = monitor_set;
                prev_logical_monitor_set = logical_monitor_set;
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

    pub async fn advance_wayland_side_effect(
        manager_obj: Arc<Mutex<DisplayManager>>,
        stage: WaylandSideEffectStage,
    ) -> Result<WaylandSideEffectStage, String> {
        if stage == WaylandSideEffectStage::ReloadKanshi {
            reload_kanshi().await.map_err(|error| error.to_string())?;
            return Ok(wayland_stage_after_reload());
        }
        if stage == WaylandSideEffectStage::Signal {
            return Ok(WaylandSideEffectStage::Signal);
        }

        let profile = {
            let manager = manager_obj.lock().await;
            observed_profile(&manager.monitors, &manager.logical_monitors)
        };
        let Some((name, text)) = profile else {
            return Ok(WaylandSideEffectStage::Signal);
        };
        let profile_changed = write_kanshi_profile_if_changed(&name, &text)
            .await
            .map_err(|error| error.to_string())?;
        Ok(wayland_stage_after_profile(profile_changed))
    }

    /// Replace only compositor-observed output state. `properties` is invariant
    /// protocol metadata and is never inferred from Sway or Wayland outputs.
    fn replace_observed_state(
        &mut self,
        monitors: Vec<Monitor>,
        logical_monitors: Vec<LogicalMonitor>,
    ) {
        self.monitors = monitors;
        self.logical_monitors = logical_monitors;
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
        self.replace_observed_state(monitors, logical_monitors);
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
    if !should_reload_kanshi(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref()) {
        info!("Skipping Kanshi reload for COSMIC desktop");
        return Ok(());
    }

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

async fn retry_watch_side_effects<
    Write,
    WriteFuture,
    Reload,
    ReloadFuture,
    Commit,
    CommitFuture,
    Signal,
    SignalFuture,
>(
    profile: Option<(&str, &str)>,
    mut write_profile: Write,
    mut reload: Reload,
    mut commit: Commit,
    mut signal: Signal,
) -> Result<(), String>
where
    Write: FnMut() -> WriteFuture,
    WriteFuture: Future<Output = Result<bool, String>>,
    Reload: FnMut() -> ReloadFuture,
    ReloadFuture: Future<Output = Result<(), String>>,
    Commit: FnMut() -> CommitFuture,
    CommitFuture: Future<Output = Result<(), String>>,
    Signal: FnMut() -> SignalFuture,
    SignalFuture: Future<Output = Result<(), String>>,
{
    let mut profile_ready = profile.is_none();
    let mut reload_ready = profile.is_none();
    let mut state_committed = false;

    let mut attempts = 0;
    loop {
        attempts += 1;
        if !profile_ready {
            let profile_changed = match write_profile().await {
                Ok(changed) => changed,
                Err(error) => {
                    warn!("Unable to persist observed kanshi profile: {error}");
                    if attempts < WATCH_MAX_SIDE_EFFECT_ATTEMPTS {
                        tokio::time::sleep(WATCH_RETRY_DELAY).await;
                        continue;
                    }
                    return Err(format!(
                        "watch side effect failed after {WATCH_MAX_SIDE_EFFECT_ATTEMPTS} attempts"
                    ));
                }
            };
            profile_ready = true;
            reload_ready = !profile_changed;
        }
        if !reload_ready {
            if let Err(error) = reload().await {
                warn!("Unable to reload kanshi after observed profile write: {error}");
                if attempts < WATCH_MAX_SIDE_EFFECT_ATTEMPTS {
                    tokio::time::sleep(WATCH_RETRY_DELAY).await;
                    continue;
                }
                return Err(format!(
                    "watch side effect failed after {WATCH_MAX_SIDE_EFFECT_ATTEMPTS} attempts"
                ));
            }
            reload_ready = true;
        }
        if !state_committed {
            if let Err(error) = commit().await {
                warn!("Unable to commit observed monitor state: {error}");
                if attempts < WATCH_MAX_SIDE_EFFECT_ATTEMPTS {
                    tokio::time::sleep(WATCH_RETRY_DELAY).await;
                    continue;
                }
                return Err(format!(
                    "watch side effect failed after {WATCH_MAX_SIDE_EFFECT_ATTEMPTS} attempts"
                ));
            }
            state_committed = true;
        }
        if let Err(error) = signal().await {
            warn!("Unable to emit MonitorsChanged after observed state update: {error}");
            if attempts < WATCH_MAX_SIDE_EFFECT_ATTEMPTS {
                tokio::time::sleep(WATCH_RETRY_DELAY).await;
                continue;
            }
            return Err(format!(
                "watch side effect failed after {WATCH_MAX_SIDE_EFFECT_ATTEMPTS} attempts"
            ));
        }
        return Ok(());
    }
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
    write_profile_if_changed(&kanshi_paths.profiles, profile_name, profile_text)
}

fn write_profile_if_changed(
    profile_dir: &std::path::Path,
    profile_name: &str,
    profile_text: &str,
) -> Result<bool, Box<dyn Error>> {
    fs::create_dir_all(profile_dir)?;
    let profile_path = profile_dir.join(profile_name);

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
    use crate::wayland_observer::{
        ApplyRequestMessage, OutputHeadSnapshot, OutputModeSnapshot, OutputSnapshot,
        WaylandApplyHandle,
    };

    #[tokio::test]
    async fn run_server_registers_dbus_without_sway_monitor_preflight() {
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

    /// Fake responder standing in for the observer thread's Wayland-specific
    /// handling: it receives the apply request through a real `calloop`
    /// event loop (exercising the actual bridge mechanism) and replies with
    /// a caller-supplied outcome, without touching any Wayland type.
    fn spawn_fake_wayland_apply_responder(
        outcome: Result<(), String>,
    ) -> (WaylandApplyHandle, std::thread::JoinHandle<()>) {
        let (sender, apply_rx) = calloop::channel::channel::<ApplyRequestMessage>();
        let handle = WaylandApplyHandle::for_test(sender);
        let worker = std::thread::spawn(move || {
            let mut event_loop: calloop::EventLoop<()> = calloop::EventLoop::try_new().unwrap();
            let loop_handle = event_loop.handle();
            loop_handle
                .insert_source(apply_rx, move |event, _, _| {
                    if let calloop::channel::Event::Msg(message) = event {
                        let _ = message.response.send(outcome.clone());
                    }
                })
                .unwrap();
            event_loop
                .dispatch(std::time::Duration::from_secs(2), &mut ())
                .unwrap();
        });
        (handle, worker)
    }

    #[tokio::test]
    async fn cosmic_desktop_takes_the_wayland_apply_path_without_sway_connection() {
        let manager = Arc::new(Mutex::new(build_manager(
            vec![Monitor::test_new(
                "eDP-1",
                "Regolith",
                "Panel",
                "A1",
                vec![Modes::test_new("1920x1080@60Hz")],
            )],
            Vec::new(),
        )));
        let (apply_handle, worker) = spawn_fake_wayland_apply_responder(Ok(()));
        let mut server = DisplayServer::new(manager, None)
            .await
            .with_cosmic_desktop(true)
            .with_wayland_apply_handle(Some(apply_handle));

        let result = server
            .apply_monitors_config(
                1,
                1,
                vec![MonitorApply::test_new(
                    "eDP-1",
                    "1920x1080@60Hz",
                    0,
                    0,
                    1.0,
                    0,
                    true,
                )],
                DisplayManagerProperties::new(),
            )
            .await;

        assert!(result.is_ok());
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn cosmic_desktop_verify_only_does_not_signal_or_require_reload() {
        let manager = Arc::new(Mutex::new(build_manager(Vec::new(), Vec::new())));
        let (apply_handle, worker) = spawn_fake_wayland_apply_responder(Ok(()));
        let mut server = DisplayServer::new(manager, None)
            .await
            .with_cosmic_desktop(true)
            .with_wayland_apply_handle(Some(apply_handle));

        let result = server
            .apply_monitors_config(1, 0, Vec::new(), DisplayManagerProperties::new())
            .await;

        assert!(result.is_ok());
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn cosmic_desktop_surfaces_compositor_failure() {
        let manager = Arc::new(Mutex::new(build_manager(Vec::new(), Vec::new())));
        let (apply_handle, worker) = spawn_fake_wayland_apply_responder(Err(
            "COSMIC output configuration failed".to_string(),
        ));
        let mut server = DisplayServer::new(manager, None)
            .await
            .with_cosmic_desktop(true)
            .with_wayland_apply_handle(Some(apply_handle));

        let result = server
            .apply_monitors_config(1, 1, Vec::new(), DisplayManagerProperties::new())
            .await;

        assert!(matches!(
            result,
            Err(zbus::fdo::Error::Failed(message))
                if message == "COSMIC output configuration failed"
        ));
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn cosmic_desktop_without_apply_handle_returns_backend_error() {
        let manager = Arc::new(Mutex::new(build_manager(Vec::new(), Vec::new())));
        let mut server = DisplayServer::new(manager, None)
            .await
            .with_cosmic_desktop(true);

        let result = server
            .apply_monitors_config(1, 1, Vec::new(), DisplayManagerProperties::new())
            .await;

        assert!(matches!(
            result,
            Err(zbus::fdo::Error::Failed(message))
                if message == "COSMIC Wayland apply handle is unavailable"
        ));
    }

    #[test]
    fn cosmic_apply_request_pads_unconfigured_outputs_as_disabled() {
        // COSMIC-observed monitors carry the connector name as their display
        // name (vendor/model/serial are unknown over wlr-output-management),
        // matching `Monitor::from_snapshot`'s `snapshot_identity` mapping.
        let monitors = vec![
            Monitor::test_new("eDP-1", "", "", "", vec![Modes::test_new("1920x1080@60Hz")]),
            Monitor::test_new(
                "HDMI-A-1",
                "",
                "",
                "",
                vec![Modes::test_new("1920x1080@60Hz")],
            ),
        ];
        let requested = vec![MonitorApply::test_new(
            "eDP-1",
            "1920x1080@60Hz",
            10,
            20,
            1.0,
            0,
            true,
        )];

        let request = build_cosmic_apply_request(7, &monitors, &requested).unwrap();

        assert_eq!(request.serial, 7);
        assert_eq!(request.heads.len(), 2);
        let enabled = request
            .heads
            .iter()
            .find(|head| head.name == "eDP-1")
            .unwrap();
        assert!(enabled.enabled);
        assert_eq!(enabled.mode, Some((1920, 1080, Some(60_000))));
        let disabled = request
            .heads
            .iter()
            .find(|head| head.name == "HDMI-A-1")
            .unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.mode, None);
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

    #[test]
    fn cosmic_desktop_skips_kanshi_reload() {
        assert!(!should_reload_kanshi(Some("COSMIC")));
        assert!(!should_reload_kanshi(Some("cosmic:GNOME")));
    }

    #[test]
    fn non_cosmic_desktops_preserve_kanshi_reload() {
        assert!(should_reload_kanshi(Some("GNOME")));
        assert!(should_reload_kanshi(Some("sway")));
        assert!(should_reload_kanshi(None));
    }

    #[test]
    fn cosmic_profile_apply_reports_missing_wayland_configuration_handles() {
        assert_eq!(
            cosmic_profile_apply_status(),
            Err("COSMIC profile apply is unavailable: the DisplayServer D-Bus boundary is not wired to the observer-thread OutputConfigurationRequest")
        );
    }

    #[test]
    fn plans_cosmic_profile_against_observed_output_without_applying_it() {
        let snapshot = OutputSnapshot {
            serial: 11,
            heads: vec![snapshot_head(
                "DP-1",
                true,
                Some((320, 180)),
                Some(3),
                Some(1.25),
                2560,
                1440,
                Some(144_000),
            )],
        };
        let profile = vec![MonitorApply::test_new(
            "DP-1",
            "2560x1440@144Hz",
            10,
            20,
            1.5,
            0,
            true,
        )];

        assert_eq!(
            plan_cosmic_profile(&snapshot, &profile).unwrap(),
            vec![CosmicOutputPlan {
                name: "DP-1".to_string(),
                position: (10, 20),
                transform: 0,
                scale_milli: 1500,
                mode: (2560, 1440, Some(144_000)),
            }]
        );
        assert_eq!(cosmic_profile_apply_status().is_err(), true);
    }

    #[test]
    fn plans_cosmic_profile_with_exact_refresh_for_same_resolution_modes() {
        let mut head = snapshot_head(
            "DP-1",
            true,
            Some((0, 0)),
            Some(0),
            Some(1.0),
            1920,
            1080,
            Some(50_000),
        );
        head.modes.push(OutputModeSnapshot {
            width: 1920,
            height: 1080,
            refresh_mhz: Some(60_000),
            preferred: false,
            current: false,
        });
        let snapshot = OutputSnapshot {
            serial: 11,
            heads: vec![head],
        };
        let profile = vec![MonitorApply::test_new(
            "DP-1",
            "1920x1080@60Hz",
            0,
            0,
            1.0,
            0,
            true,
        )];

        let plans = plan_cosmic_profile(&snapshot, &profile).unwrap();
        assert_eq!(plans[0].mode, (1920, 1080, Some(60_000)));
    }

    #[test]
    fn rejects_cosmic_profile_when_output_mode_is_not_observed() {
        let snapshot = OutputSnapshot {
            serial: 11,
            heads: vec![snapshot_head(
                "DP-1",
                true,
                Some((0, 0)),
                Some(0),
                Some(1.0),
                1920,
                1080,
                Some(60_000),
            )],
        };
        let profile = vec![MonitorApply::test_new(
            "DP-1",
            "3840x2160@60Hz",
            0,
            0,
            1.0,
            0,
            true,
        )];

        assert_eq!(
            plan_cosmic_profile(&snapshot, &profile),
            Err("COSMIC profile mode is unavailable for DP-1: 3840x2160".to_string())
        );
    }

    #[test]
    fn profile_storage_writes_new_content_and_skips_identical_content() {
        let profile_dir = std::env::temp_dir().join(format!(
            "regolith-displayd-profile-test-{}",
            std::process::id()
        ));
        let profile_name = "eDP-1";
        let profile_text = "profile {\n}\n";

        assert!(write_profile_if_changed(&profile_dir, profile_name, profile_text).unwrap());
        assert_eq!(
            std::fs::read_to_string(profile_dir.join(profile_name)).unwrap(),
            profile_text
        );
        assert!(!write_profile_if_changed(&profile_dir, profile_name, profile_text).unwrap());
        assert!(write_profile_if_changed(&profile_dir, profile_name, "profile { }\n").unwrap());
        let _ = std::fs::remove_dir_all(profile_dir);
    }

    fn commits_refreshed_monitor_info_before_signal_state_is_observable() {
        let previous_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let refreshed_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1920x1080@60Hz")],
        );
        let refreshed_logical =
            LogicalMonitor::test_new("eDP-1", "1920x1080@60Hz", 0, 0, 1.0, 0, true);
        let mut manager = build_manager(vec![previous_monitor], Vec::new());

        let properties = DisplayManagerProperties {
            layout: Some(1),
            ..DisplayManagerProperties::new()
        };
        commit_refreshed_monitor_info(
            &mut manager,
            properties.clone(),
            Ok((
                vec![refreshed_monitor.clone()],
                vec![refreshed_logical.clone()],
            )),
        )
        .unwrap();

        assert_eq!(manager.properties, properties);
        assert_eq!(manager.monitors, vec![refreshed_monitor]);
        assert_eq!(manager.logical_monitors, vec![refreshed_logical]);
    }

    #[test]
    fn refresh_failure_does_not_partially_mutate_manager_state() {
        let previous_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let mut manager = build_manager(vec![previous_monitor], Vec::new());
        let previous = manager.clone();
        let properties = DisplayManagerProperties {
            layout: Some(1),
            ..DisplayManagerProperties::new()
        };

        let result = commit_refreshed_monitor_info(
            &mut manager,
            properties,
            Err("refresh failed".to_string()),
        );

        assert_eq!(result, Err("refresh failed".to_string()));
        assert_eq!(manager, previous);
    }

    #[tokio::test]
    async fn apply_core_runs_reload_refresh_commit_and_signal_in_order() {
        let previous_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let refreshed_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1920x1080@60Hz")],
        );
        let refreshed_logical =
            LogicalMonitor::test_new("eDP-1", "1920x1080@60Hz", 0, 0, 1.0, 0, true);
        let mut manager = build_manager(vec![previous_monitor], Vec::new());
        let properties = DisplayManagerProperties {
            layout: Some(7),
            ..DisplayManagerProperties::new()
        };
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));

        let write_events = Arc::clone(&events);
        let reload_events = Arc::clone(&events);
        let refresh_events = Arc::clone(&events);
        let signal_events = Arc::clone(&events);
        apply_monitors_config_core(
            &mut manager,
            properties.clone(),
            move || {
                write_events.lock().unwrap().push("write");
                async { Ok(true) }
            },
            move || {
                reload_events.lock().unwrap().push("reload");
                async { Ok(()) }
            },
            move || {
                refresh_events.lock().unwrap().push("refresh");
                async { Ok((vec![refreshed_monitor], vec![refreshed_logical])) }
            },
            move || {
                signal_events.lock().unwrap().push("signal");
                async { Ok(()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            vec!["write", "reload", "refresh", "signal"]
        );
        assert_eq!(manager.properties, properties);
        assert_eq!(manager.monitors[0].get_current_mode(), "1920x1080@60Hz");
        assert_eq!(manager.logical_monitors.len(), 1);
    }

    #[tokio::test]
    async fn apply_core_refresh_failure_keeps_manager_unchanged_and_skips_signal() {
        let previous_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let mut manager = build_manager(vec![previous_monitor], Vec::new());
        let previous = manager.clone();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let write_events = Arc::clone(&events);
        let reload_events = Arc::clone(&events);
        let refresh_events = Arc::clone(&events);
        let signal_events = Arc::clone(&events);

        let result = apply_monitors_config_core(
            &mut manager,
            DisplayManagerProperties::new(),
            move || {
                write_events.lock().unwrap().push("write");
                async { Ok(true) }
            },
            move || {
                reload_events.lock().unwrap().push("reload");
                async { Ok(()) }
            },
            move || {
                refresh_events.lock().unwrap().push("refresh");
                async { Err("refresh failed".to_string()) }
            },
            move || {
                signal_events.lock().unwrap().push("signal");
                async { Ok(()) }
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(zbus::fdo::Error::Failed(message)) if message.contains("refresh failed")
        ));
        assert_eq!(manager, previous);
        assert_eq!(*events.lock().unwrap(), vec!["write", "reload", "refresh"]);
    }

    #[tokio::test]
    async fn apply_core_reload_failure_keeps_manager_unchanged_and_skips_refresh_and_signal() {
        let previous_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let mut manager = build_manager(vec![previous_monitor], Vec::new());
        let previous = manager.clone();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let write_events = Arc::clone(&events);
        let reload_events = Arc::clone(&events);
        let refresh_events = Arc::clone(&events);
        let signal_events = Arc::clone(&events);

        let result = apply_monitors_config_core(
            &mut manager,
            DisplayManagerProperties::new(),
            move || {
                write_events.lock().unwrap().push("write");
                async { Ok(true) }
            },
            move || {
                reload_events.lock().unwrap().push("reload");
                async { Err(zbus::fdo::Error::Failed("reload failed".to_string())) }
            },
            move || {
                refresh_events.lock().unwrap().push("refresh");
                async { Ok((Vec::new(), Vec::new())) }
            },
            move || {
                signal_events.lock().unwrap().push("signal");
                async { Ok(()) }
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(zbus::fdo::Error::Failed(message)) if message == "reload failed"
        ));
        assert_eq!(manager, previous);
        assert_eq!(*events.lock().unwrap(), vec!["write", "reload"]);
    }

    #[test]
    fn observed_state_reconciliation_preserves_invariant_properties() {
        let mut manager = build_manager(Vec::new(), Vec::new());
        manager.properties = DisplayManagerProperties {
            layout: Some(7),
            support_layout_change: Some(false),
            global_scale: Some(true),
            legacy_scale_factor: Some(2),
        };
        let properties = manager.properties.clone();

        manager.replace_observed_state(
            vec![Monitor::test_new(
                "HDMI-A-1",
                "Projector",
                "Room",
                "B2",
                vec![Modes::test_new("1920x1080@60Hz")],
            )],
            vec![LogicalMonitor::from_snapshot(&snapshot_head(
                "HDMI-A-1",
                true,
                Some((100, 200)),
                Some(0),
                Some(1.5),
                1920,
                1080,
                Some(60_000),
            ))
            .unwrap()],
        );

        assert_eq!(manager.properties, properties);
        assert_eq!(manager.monitors[0].get_dpy_name(), "Projector Room B2");
        assert_eq!(manager.logical_monitors[0].get_dpy_name(), "HDMI-A-1");
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
        assert!(wayland_reload_required(false, true));
    }

    #[test]
    fn unchanged_pending_wayland_retry_keeps_its_stage() {
        assert_eq!(
            wayland_side_effect_stage(false, Some(WaylandSideEffectStage::Signal), true),
            Some(WaylandSideEffectStage::Signal)
        );
        assert_eq!(
            wayland_side_effect_stage(false, Some(WaylandSideEffectStage::ReloadKanshi), true),
            Some(WaylandSideEffectStage::ReloadKanshi)
        );
    }

    #[test]
    fn changed_wayland_snapshot_starts_profile_side_effects() {
        assert_eq!(
            wayland_side_effect_stage(true, None, true),
            Some(WaylandSideEffectStage::PersistProfile)
        );
        assert_eq!(
            wayland_side_effect_stage(true, None, false),
            Some(WaylandSideEffectStage::Signal)
        );
    }

    #[test]
    fn cosmic_wayland_profile_persistence_advances_directly_to_signal() {
        assert_eq!(
            wayland_stage_after_profile(true),
            WaylandSideEffectStage::Signal
        );
        assert_eq!(
            wayland_stage_after_profile(false),
            WaylandSideEffectStage::Signal
        );
        assert_eq!(wayland_stage_after_reload(), WaylandSideEffectStage::Signal);
    }

    #[tokio::test]
    async fn retries_transient_watch_side_effect_failures_before_publishing_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let writes = Arc::new(AtomicUsize::new(0));
        let reloads = Arc::new(AtomicUsize::new(0));
        let signals = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let write_count = Arc::clone(&writes);
        let write_events = Arc::clone(&events);
        let reload_count = Arc::clone(&reloads);
        let reload_events = Arc::clone(&events);
        let signal_count = Arc::clone(&signals);
        let signal_events = Arc::clone(&events);
        retry_watch_side_effects(
            Some(("profile", "text")),
            move || {
                let attempt = write_count.fetch_add(1, Ordering::SeqCst);
                write_events.lock().unwrap().push("write");
                async move {
                    if attempt == 0 {
                        Err("write failed".to_string())
                    } else {
                        Ok(true)
                    }
                }
            },
            move || {
                let attempt = reload_count.fetch_add(1, Ordering::SeqCst);
                reload_events.lock().unwrap().push("reload");
                async move {
                    if attempt == 0 {
                        Err("reload failed".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
            || async { Ok(()) },
            move || {
                let attempt = signal_count.fetch_add(1, Ordering::SeqCst);
                signal_events.lock().unwrap().push("signal");
                async move {
                    if attempt == 0 {
                        Err("signal failed".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), 2);
        assert_eq!(reloads.load(Ordering::SeqCst), 2);
        assert_eq!(signals.load(Ordering::SeqCst), 2);
        assert_eq!(
            *events.lock().unwrap(),
            vec!["write", "write", "reload", "reload", "signal", "signal"]
        );
    }

    #[tokio::test]
    async fn persistent_watch_side_effect_failure_is_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&attempts);
        let result = retry_watch_side_effects(
            None,
            || async { Ok(false) },
            || async { Ok(()) },
            || async { Ok(()) },
            move || {
                count.fetch_add(1, Ordering::SeqCst);
                async { Err("signal unavailable".to_string()) }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            WATCH_MAX_SIDE_EFFECT_ATTEMPTS
        );
    }

    #[tokio::test]
    async fn signal_only_retry_does_not_replay_successful_reload() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reloads = Arc::new(AtomicUsize::new(0));
        let signals = Arc::new(AtomicUsize::new(0));
        let reload_count = Arc::clone(&reloads);
        let signal_count = Arc::clone(&signals);

        retry_watch_side_effects(
            Some(("profile", "text")),
            || async { Ok(true) },
            move || {
                reload_count.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            },
            || async { Ok(()) },
            move || {
                let attempt = signal_count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err("signal failed".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(reloads.load(Ordering::SeqCst), 1);
        assert_eq!(signals.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn commits_observed_state_before_signal_and_retries_signal_without_replaying_persistence()
    {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let previous_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1024x768@60Hz")],
        );
        let observed_monitor = Monitor::test_new(
            "eDP-1",
            "Regolith",
            "Panel",
            "A1",
            vec![Modes::test_new("1920x1080@60Hz")],
        );
        let observed_logical =
            LogicalMonitor::test_new("eDP-1", "1920x1080@60Hz", 0, 0, 1.0, 0, true);
        let manager = Arc::new(Mutex::new(build_manager(
            vec![previous_monitor],
            Vec::new(),
        )));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writes = Arc::new(AtomicUsize::new(0));
        let reloads = Arc::new(AtomicUsize::new(0));
        let signals = Arc::new(AtomicUsize::new(0));

        let write_events = Arc::clone(&events);
        let write_count = Arc::clone(&writes);
        let reload_events = Arc::clone(&events);
        let reload_count = Arc::clone(&reloads);
        let commit_events = Arc::clone(&events);
        let commit_manager = Arc::clone(&manager);
        let signal_events = Arc::clone(&events);
        let signal_manager = Arc::clone(&manager);
        let signal_count = Arc::clone(&signals);
        let expected_monitor = observed_monitor.clone();
        let expected_logical = observed_logical.clone();

        retry_watch_side_effects(
            Some(("profile", "text")),
            move || {
                write_count.fetch_add(1, Ordering::SeqCst);
                write_events.lock().unwrap().push("write");
                async { Ok(true) }
            },
            move || {
                reload_count.fetch_add(1, Ordering::SeqCst);
                reload_events.lock().unwrap().push("reload");
                async { Ok(()) }
            },
            move || {
                let commit_manager = Arc::clone(&commit_manager);
                let expected_monitor = expected_monitor.clone();
                let expected_logical = expected_logical.clone();
                commit_events.lock().unwrap().push("commit");
                async move {
                    let mut manager = commit_manager.lock().await;
                    manager.replace_observed_state(vec![expected_monitor], vec![expected_logical]);
                    Ok(())
                }
            },
            move || {
                let signal_manager = Arc::clone(&signal_manager);
                let expected_monitor = observed_monitor.clone();
                let expected_logical = observed_logical.clone();
                let attempt = signal_count.fetch_add(1, Ordering::SeqCst);
                signal_events.lock().unwrap().push("signal");
                async move {
                    let manager = signal_manager.lock().await;
                    assert_eq!(manager.monitors, vec![expected_monitor]);
                    assert_eq!(manager.logical_monitors, vec![expected_logical]);
                    if attempt == 0 {
                        Err("signal failed".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert_eq!(reloads.load(Ordering::SeqCst), 1);
        assert_eq!(signals.load(Ordering::SeqCst), 2);
        assert_eq!(
            *events.lock().unwrap(),
            vec!["write", "reload", "commit", "signal", "signal"]
        );
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
