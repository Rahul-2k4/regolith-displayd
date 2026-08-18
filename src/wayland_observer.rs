//! Standalone Wayland output observer for wlroots output-management snapshots.
//!
//! This module is intentionally isolated from `DisplayManager` and the existing
//! Sway IPC path. It provides a bounded API that can collect immutable output
//! snapshots directly from a Wayland compositor that exposes
//! `zwlr_output_manager_v1`.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use wayland_client::{
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_head_v1, zwlr_output_head_v1::ZwlrOutputHeadV1, zwlr_output_manager_v1,
    zwlr_output_manager_v1::ZwlrOutputManagerV1, zwlr_output_mode_v1,
    zwlr_output_mode_v1::ZwlrOutputModeV1,
};

/// Immutable snapshot of the compositor output state published by one manager `done` event.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSnapshot {
    /// Output-management serial attached to the snapshot.
    pub serial: u32,
    /// Stable, name-sorted output heads present at publication time.
    pub heads: Vec<OutputHeadSnapshot>,
}

/// Immutable snapshot of one output head.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputHeadSnapshot {
    /// Stable compositor-defined head name such as `HDMI-A-1`.
    pub name: String,
    /// Human-readable description when the compositor provides one.
    pub description: Option<String>,
    /// Whether the head is currently enabled.
    pub enabled: bool,
    /// Current global compositor position for enabled heads.
    pub position: Option<(i32, i32)>,
    /// Current wl_output transform code for enabled heads.
    pub transform: Option<u32>,
    /// Normalized scale for enabled heads.
    pub scale: Option<f64>,
    /// The selected current mode snapshot when known.
    pub current_mode: Option<OutputModeSnapshot>,
    /// All known modes for the head in deterministic order.
    pub modes: Vec<OutputModeSnapshot>,
}

/// Immutable snapshot of one output mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputModeSnapshot {
    /// Mode width in hardware pixels.
    pub width: i32,
    /// Mode height in hardware pixels.
    pub height: i32,
    /// Refresh rate in mHz when provided by the compositor.
    pub refresh_mhz: Option<i32>,
    /// Whether the compositor marks this mode as preferred.
    pub preferred: bool,
    /// Whether this mode is selected as current in the published snapshot.
    pub current: bool,
}

/// Controlled failures returned by the standalone observer.
#[derive(Debug, Clone, PartialEq)]
pub enum WaylandObserverError {
    /// Connecting to the compositor failed.
    ConnectionFailed(String),
    /// The initial Wayland global discovery roundtrip failed.
    GlobalDiscoveryFailed(String),
    /// The compositor does not expose the required output-management global.
    UnsupportedGlobal { name: &'static str },
    /// Dispatching Wayland events failed.
    DispatchFailed(String),
    /// Spawning the dedicated observation thread failed.
    ThreadSpawnFailed(String),
    /// The compositor destroyed the output manager and observation cannot continue.
    ManagerFinished,
    /// Internal snapshot state became inconsistent while processing protocol events.
    StateViolation(String),
}

impl fmt::Display for WaylandObserverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionFailed(message) => write!(f, "Wayland connection failed: {message}"),
            Self::GlobalDiscoveryFailed(message) => {
                write!(f, "Wayland global discovery failed: {message}")
            }
            Self::UnsupportedGlobal { name } => {
                write!(f, "Required Wayland global is unavailable: {name}")
            }
            Self::DispatchFailed(message) => write!(f, "Wayland dispatch failed: {message}"),
            Self::ThreadSpawnFailed(message) => {
                write!(f, "Wayland observer thread spawn failed: {message}")
            }
            Self::ManagerFinished => write!(f, "Wayland output manager finished observation"),
            Self::StateViolation(message) => {
                write!(f, "Wayland observer state violation: {message}")
            }
        }
    }
}

impl std::error::Error for WaylandObserverError {}

/// Standalone observer entry point.
#[derive(Debug, Default)]
pub struct WaylandOutputObserver;

impl WaylandOutputObserver {
    /// Connect to the compositor referenced by the current environment and start
    /// a dedicated blocking observation thread that forwards every
    /// `zwlr_output_manager_v1.done` snapshot through the returned receiver.
    pub fn observe(
    ) -> Result<Receiver<Result<OutputSnapshot, WaylandObserverError>>, WaylandObserverError> {
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let (publication_tx, publication_rx) = mpsc::channel();

        thread::Builder::new()
            .name("wayland-output-observer".to_string())
            .spawn(move || {
                let startup = Self::connect_and_bind();
                let (connection, mut queue, mut state) = match startup {
                    Ok(parts) => {
                        let _ = startup_tx.send(Ok(()));
                        parts
                    }
                    Err(error) => {
                        let _ = startup_tx.send(Err(error));
                        return;
                    }
                };

                Self::run_observer_loop(connection, &mut queue, &mut state, publication_tx);
            })
            .map_err(|error| WaylandObserverError::ThreadSpawnFailed(error.to_string()))?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(publication_rx),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(WaylandObserverError::DispatchFailed(format!(
                "observer startup channel closed unexpectedly: {error}"
            ))),
        }
    }

    /// Connect to the compositor referenced by the current environment and return
    /// the first immutable snapshot published by `zwlr_output_manager_v1.done`.
    pub fn collect_current() -> Result<OutputSnapshot, WaylandObserverError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| WaylandObserverError::ConnectionFailed(error.to_string()))?;
        Self::collect_from_connection(&connection)
    }

    fn collect_from_connection(
        connection: &Connection,
    ) -> Result<OutputSnapshot, WaylandObserverError> {
        let (mut queue, mut state) = Self::connect_and_bind_from_connection(connection)?;

        loop {
            if let Some(result) = state.take_next_result() {
                return result;
            }
            queue
                .blocking_dispatch(&mut state)
                .map_err(|error| WaylandObserverError::DispatchFailed(error.to_string()))?;
        }
    }

    fn connect_and_bind(
    ) -> Result<(Connection, EventQueue<ObserverState>, ObserverState), WaylandObserverError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| WaylandObserverError::ConnectionFailed(error.to_string()))?;
        let (queue, state) = Self::connect_and_bind_from_connection(&connection)?;
        Ok((connection, queue, state))
    }

    fn connect_and_bind_from_connection(
        connection: &Connection,
    ) -> Result<(EventQueue<ObserverState>, ObserverState), WaylandObserverError> {
        let (globals, queue) = registry_queue_init::<ObserverState>(connection)
            .map_err(|error| WaylandObserverError::GlobalDiscoveryFailed(error.to_string()))?;
        let manager: ZwlrOutputManagerV1 =
            globals.bind(&queue.handle(), 1..=4, ()).map_err(|_| {
                WaylandObserverError::UnsupportedGlobal {
                    name: "zwlr_output_manager_v1",
                }
            })?;

        let state = ObserverState {
            _manager: Some(manager),
            ..ObserverState::default()
        };

        Ok((queue, state))
    }

    fn run_observer_loop(
        _connection: Connection,
        queue: &mut EventQueue<ObserverState>,
        state: &mut ObserverState,
        publication_tx: Sender<Result<OutputSnapshot, WaylandObserverError>>,
    ) {
        loop {
            let publish_status = publish_pending_results(state, &publication_tx);
            if publish_status.should_stop() {
                break;
            }

            if let Err(error) = queue.blocking_dispatch(state) {
                state.store_terminal(WaylandObserverError::DispatchFailed(error.to_string()));
            }
        }
    }
}

#[derive(Debug, Default)]
struct ObserverState {
    collector: SnapshotCollector,
    handles: OutputManagementHandles,
    _manager: Option<ZwlrOutputManagerV1>,
    terminal_error: Option<WaylandObserverError>,
    terminal_reached: bool,
}

impl ObserverState {
    fn store_error(&mut self, error: SnapshotStateError) {
        self.store_terminal(error.into());
    }

    fn store_terminal(&mut self, error: WaylandObserverError) {
        if self.terminal_reached {
            return;
        }

        self.terminal_reached = true;
        self.terminal_error = Some(error);
    }

    fn take_next_result(&mut self) -> Option<Result<OutputSnapshot, WaylandObserverError>> {
        if let Some(snapshot) = self.collector.take_publication() {
            return Some(Ok(snapshot));
        }

        self.terminal_error.take().map(Err)
    }

    fn terminal_drained(&self) -> bool {
        self.terminal_reached && self.terminal_error.is_none()
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ObserverState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for ObserverState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                let head_id = head.id().protocol_id();
                state.handles.retain_head(head);
                state.collector.note_head(head_id);
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                if let Err(error) = state.collector.publish_done(serial) {
                    state.store_error(error);
                }
            }
            zwlr_output_manager_v1::Event::Finished => {
                state.store_terminal(WaylandObserverError::ManagerFinished);
            }
            _ => {}
        }
    }

    event_created_child!(ObserverState, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for ObserverState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let head_id = proxy.id().protocol_id();
        state.collector.note_head(head_id);

        let result = match event {
            zwlr_output_head_v1::Event::Name { name } => {
                state.collector.set_head_name(head_id, name)
            }
            zwlr_output_head_v1::Event::Description { description } => {
                state.collector.set_head_description(head_id, description)
            }
            zwlr_output_head_v1::Event::Enabled { enabled } => {
                state.collector.set_head_enabled(head_id, enabled != 0)
            }
            zwlr_output_head_v1::Event::CurrentMode { mode } => state
                .collector
                .set_head_current_mode(head_id, mode.id().protocol_id()),
            zwlr_output_head_v1::Event::Position { x, y } => {
                state.collector.set_head_position(head_id, x, y)
            }
            zwlr_output_head_v1::Event::Transform { transform } => state
                .collector
                .set_head_transform(head_id, transform.into()),
            zwlr_output_head_v1::Event::Scale { scale } => {
                state.collector.set_head_scale(head_id, scale)
            }
            zwlr_output_head_v1::Event::Mode { mode } => {
                let mode_id = mode.id().protocol_id();
                state.handles.retain_mode(head_id, mode);
                state.collector.note_mode(head_id, mode_id)
            }
            zwlr_output_head_v1::Event::Finished => {
                let result = state.collector.finish_head(head_id);
                if result.is_ok() {
                    state.handles.release_head(head_id);
                }
                result
            }
            _ => Ok(()),
        };

        if let Err(error) = result {
            state.store_error(error);
        }
    }

    event_created_child!(ObserverState, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for ObserverState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mode_id = proxy.id().protocol_id();
        let result = match event {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                state.collector.set_mode_size(mode_id, width, height)
            }
            zwlr_output_mode_v1::Event::Refresh { refresh } => {
                state.collector.set_mode_refresh(mode_id, refresh)
            }
            zwlr_output_mode_v1::Event::Preferred => state.collector.set_mode_preferred(mode_id),
            zwlr_output_mode_v1::Event::Finished => {
                let result = state.collector.finish_mode(mode_id);
                if result.is_ok() {
                    state.handles.release_mode(mode_id);
                }
                result
            }
            _ => Ok(()),
        };

        if let Err(error) = result {
            state.store_error(error);
        }
    }
}

/// Keeps output-management proxies alive for a future configuration transaction.
///
/// Protocol object IDs remain valid only for the lifetime of this observer
/// connection. They are sufficient to associate handles with the immutable
/// snapshot state collected from the same connection.
#[derive(Debug, Default)]
pub(crate) struct OutputManagementHandles {
    heads: HashMap<u32, ZwlrOutputHeadV1>,
    modes: HashMap<u32, RetainedMode>,
}

#[derive(Debug)]
struct RetainedMode {
    head_id: u32,
    proxy: ZwlrOutputModeV1,
}

#[allow(dead_code)]
impl OutputManagementHandles {
    fn retain_head(&mut self, head: ZwlrOutputHeadV1) {
        self.heads.insert(head.id().protocol_id(), head);
    }

    fn retain_mode(&mut self, head_id: u32, mode: ZwlrOutputModeV1) {
        self.modes.insert(
            mode.id().protocol_id(),
            RetainedMode {
                head_id,
                proxy: mode,
            },
        );
    }

    fn release_head(&mut self, head_id: u32) {
        self.heads.remove(&head_id);
        self.modes.retain(|_, mode| mode.head_id != head_id);
    }

    fn release_mode(&mut self, mode_id: u32) {
        self.modes.remove(&mode_id);
    }

    /// Returns the retained head proxy for a protocol object ID.
    pub(crate) fn head(&self, head_id: u32) -> Option<&ZwlrOutputHeadV1> {
        self.heads.get(&head_id)
    }

    /// Returns the retained mode proxy when it belongs to the given head ID.
    pub(crate) fn mode(&self, head_id: u32, mode_id: u32) -> Option<&ZwlrOutputModeV1> {
        self.modes
            .get(&mode_id)
            .filter(|mode| mode.head_id == head_id)
            .map(|mode| &mode.proxy)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum SnapshotStateError {
    InvalidScale(f64),
    UnknownHead(u32),
    UnknownMode(u32),
}

impl fmt::Display for SnapshotStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScale(scale) => write!(f, "invalid scale value {scale}"),
            Self::UnknownHead(head_id) => write!(f, "unknown head id {head_id}"),
            Self::UnknownMode(mode_id) => write!(f, "unknown mode id {mode_id}"),
        }
    }
}

impl From<SnapshotStateError> for WaylandObserverError {
    fn from(error: SnapshotStateError) -> Self {
        Self::StateViolation(error.to_string())
    }
}

#[derive(Debug, Default)]
struct SnapshotCollector {
    heads: HashMap<u32, PendingHead>,
    mode_to_head: HashMap<u32, u32>,
    publications: VecDeque<OutputSnapshot>,
}

impl SnapshotCollector {
    fn note_head(&mut self, head_id: u32) {
        self.heads
            .entry(head_id)
            .or_insert_with(|| PendingHead::new(head_id));
    }

    fn set_head_name(&mut self, head_id: u32, name: String) -> Result<(), SnapshotStateError> {
        self.head_mut(head_id)?.name = Some(name);
        Ok(())
    }

    fn set_head_description(
        &mut self,
        head_id: u32,
        description: String,
    ) -> Result<(), SnapshotStateError> {
        self.head_mut(head_id)?.description = Some(description);
        Ok(())
    }

    fn set_head_enabled(&mut self, head_id: u32, enabled: bool) -> Result<(), SnapshotStateError> {
        let head = self.head_mut(head_id)?;
        head.enabled = enabled;
        if !enabled {
            head.current_mode_id = None;
            head.position = None;
            head.transform = None;
            head.scale = None;
        }
        Ok(())
    }

    fn set_head_current_mode(
        &mut self,
        head_id: u32,
        mode_id: u32,
    ) -> Result<(), SnapshotStateError> {
        let head = self.head_mut(head_id)?;
        head.current_mode_id = Some(mode_id);
        Ok(())
    }

    fn set_head_position(
        &mut self,
        head_id: u32,
        x: i32,
        y: i32,
    ) -> Result<(), SnapshotStateError> {
        self.head_mut(head_id)?.position = Some((x, y));
        Ok(())
    }

    fn set_head_transform(
        &mut self,
        head_id: u32,
        transform: u32,
    ) -> Result<(), SnapshotStateError> {
        self.head_mut(head_id)?.transform = Some(transform);
        Ok(())
    }

    fn set_head_scale(&mut self, head_id: u32, scale: f64) -> Result<(), SnapshotStateError> {
        self.head_mut(head_id)?.scale = Some(normalize_scale(scale)?);
        Ok(())
    }

    fn note_mode(&mut self, head_id: u32, mode_id: u32) -> Result<(), SnapshotStateError> {
        let head = self.head_mut(head_id)?;
        head.modes
            .entry(mode_id)
            .or_insert_with(|| PendingMode::new(mode_id));
        self.mode_to_head.insert(mode_id, head_id);
        Ok(())
    }

    fn set_mode_size(
        &mut self,
        mode_id: u32,
        width: i32,
        height: i32,
    ) -> Result<(), SnapshotStateError> {
        let mode = self.mode_mut(mode_id)?;
        mode.width = Some(width);
        mode.height = Some(height);
        Ok(())
    }

    fn set_mode_refresh(&mut self, mode_id: u32, refresh: i32) -> Result<(), SnapshotStateError> {
        self.mode_mut(mode_id)?.refresh_mhz = Some(refresh);
        Ok(())
    }

    fn set_mode_preferred(&mut self, mode_id: u32) -> Result<(), SnapshotStateError> {
        self.mode_mut(mode_id)?.preferred = true;
        Ok(())
    }

    fn finish_head(&mut self, head_id: u32) -> Result<(), SnapshotStateError> {
        let head = self
            .heads
            .remove(&head_id)
            .ok_or(SnapshotStateError::UnknownHead(head_id))?;
        for mode_id in head.modes.keys() {
            self.mode_to_head.remove(mode_id);
        }
        Ok(())
    }

    fn finish_mode(&mut self, mode_id: u32) -> Result<(), SnapshotStateError> {
        let head_id = self
            .mode_to_head
            .remove(&mode_id)
            .ok_or(SnapshotStateError::UnknownMode(mode_id))?;
        let head = self.head_mut(head_id)?;
        head.modes.remove(&mode_id);
        if head.current_mode_id == Some(mode_id) {
            head.current_mode_id = None;
        }
        Ok(())
    }

    fn publish_done(&mut self, serial: u32) -> Result<(), SnapshotStateError> {
        let mut heads = self
            .heads
            .values()
            .cloned()
            .map(PendingHead::into_snapshot)
            .collect::<Vec<_>>();
        heads.sort_by(|left, right| left.name.cmp(&right.name));
        self.publications
            .push_back(OutputSnapshot { serial, heads });
        Ok(())
    }
    fn take_publication(&mut self) -> Option<OutputSnapshot> {
        self.publications.pop_front()
    }

    fn head_mut(&mut self, head_id: u32) -> Result<&mut PendingHead, SnapshotStateError> {
        self.heads
            .get_mut(&head_id)
            .ok_or(SnapshotStateError::UnknownHead(head_id))
    }

    fn mode_mut(&mut self, mode_id: u32) -> Result<&mut PendingMode, SnapshotStateError> {
        let head_id = *self
            .mode_to_head
            .get(&mode_id)
            .ok_or(SnapshotStateError::UnknownMode(mode_id))?;
        self.head_mut(head_id)?
            .modes
            .get_mut(&mode_id)
            .ok_or(SnapshotStateError::UnknownMode(mode_id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishStatus {
    Continue,
    Stop,
}

impl PublishStatus {
    fn should_stop(self) -> bool {
        matches!(self, Self::Stop)
    }
}

fn publish_pending_results(
    state: &mut ObserverState,
    publication_tx: &Sender<Result<OutputSnapshot, WaylandObserverError>>,
) -> PublishStatus {
    while let Some(result) = state.take_next_result() {
        if publication_tx.send(result).is_err() {
            return PublishStatus::Stop;
        }
    }

    if state.terminal_drained() {
        PublishStatus::Stop
    } else {
        PublishStatus::Continue
    }
}

#[derive(Debug, Clone, Default)]
struct PendingHead {
    id: u32,
    name: Option<String>,
    description: Option<String>,
    enabled: bool,
    position: Option<(i32, i32)>,
    transform: Option<u32>,
    scale: Option<f64>,
    current_mode_id: Option<u32>,
    modes: HashMap<u32, PendingMode>,
}

impl PendingHead {
    fn new(id: u32) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }

    fn into_snapshot(self) -> OutputHeadSnapshot {
        let mut modes = self.modes.into_values().collect::<Vec<_>>();
        modes.sort_by(|left, right| {
            left.width
                .cmp(&right.width)
                .then(left.height.cmp(&right.height))
                .then(left.refresh_mhz.cmp(&right.refresh_mhz))
                .then(left.preferred.cmp(&right.preferred))
                .then(left.id.cmp(&right.id))
        });

        let selected_mode_id = self
            .current_mode_id
            .filter(|mode_id| modes.iter().any(|mode| mode.id == *mode_id))
            .or_else(|| modes.iter().find(|mode| mode.preferred).map(|mode| mode.id));

        let current_mode = selected_mode_id.and_then(|mode_id| {
            modes
                .iter()
                .find(|mode| mode.id == mode_id)
                .cloned()
                .map(|mode| mode.into_snapshot(true))
        });
        let modes = modes
            .into_iter()
            .map(|mode| {
                let is_current = Some(mode.id) == selected_mode_id;
                mode.into_snapshot(is_current)
            })
            .collect();

        OutputHeadSnapshot {
            name: self.name.unwrap_or_else(|| format!("head-{}", self.id)),
            description: self.description,
            enabled: self.enabled,
            position: self.position,
            transform: self.transform,
            scale: self.scale,
            current_mode,
            modes,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PendingMode {
    id: u32,
    width: Option<i32>,
    height: Option<i32>,
    refresh_mhz: Option<i32>,
    preferred: bool,
}

impl PendingMode {
    fn new(id: u32) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }

    fn into_snapshot(self, current: bool) -> OutputModeSnapshot {
        OutputModeSnapshot {
            width: self.width.unwrap_or_default(),
            height: self.height.unwrap_or_default(),
            refresh_mhz: self.refresh_mhz,
            preferred: self.preferred,
            current,
        }
    }
}

fn normalize_scale(scale: f64) -> Result<f64, SnapshotStateError> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err(SnapshotStateError::InvalidScale(scale));
    }
    let rounded = (scale * 10_000.0).round() / 10_000.0;
    if rounded <= 0.0 {
        return Err(SnapshotStateError::InvalidScale(scale));
    }
    Ok(rounded)
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_scale, publish_pending_results, ObserverState, OutputSnapshot, PublishStatus,
        SnapshotCollector, WaylandObserverError,
    };
    use std::sync::mpsc;

    #[test]
    fn normalizes_scale_without_losing_fractional_values() {
        assert_eq!(normalize_scale(1.24999999).unwrap(), 1.25);
        assert_eq!(normalize_scale(2.0).unwrap(), 2.0);
        assert!(normalize_scale(f64::NAN).is_err());
        assert!(normalize_scale(0.0).is_err());
    }

    #[test]
    fn publishes_heads_in_stable_name_order() {
        let mut collector = SnapshotCollector::default();
        collector.note_head(2);
        collector.set_head_name(2, "HDMI-A-1".to_string()).unwrap();
        collector.note_head(1);
        collector.set_head_name(1, "DP-1".to_string()).unwrap();

        collector.publish_done(9).unwrap();
        let snapshot = collector.take_publication().unwrap();
        let names = snapshot
            .heads
            .into_iter()
            .map(|head| head.name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["DP-1".to_string(), "HDMI-A-1".to_string()]);
    }

    #[test]
    fn selects_current_mode_before_preferred_fallback() {
        let mut collector = SnapshotCollector::default();
        collector.note_head(7);
        collector.set_head_name(7, "DP-2".to_string()).unwrap();
        collector.set_head_enabled(7, true).unwrap();

        collector.note_mode(7, 10).unwrap();
        collector.set_mode_size(10, 1920, 1080).unwrap();
        collector.set_mode_refresh(10, 60_000).unwrap();
        collector.set_mode_preferred(10).unwrap();

        collector.note_mode(7, 11).unwrap();
        collector.set_mode_size(11, 2560, 1440).unwrap();
        collector.set_mode_refresh(11, 144_000).unwrap();
        collector.set_head_current_mode(7, 11).unwrap();

        collector.publish_done(10).unwrap();
        let snapshot = collector.take_publication().unwrap();
        let head = &snapshot.heads[0];

        assert_eq!(head.current_mode.as_ref().unwrap().width, 2560);
        assert!(head.current_mode.as_ref().unwrap().current);
        assert!(head.modes.iter().any(|mode| mode.preferred));
        assert_eq!(head.modes.iter().filter(|mode| mode.current).count(), 1);
    }

    #[test]
    fn publishes_only_when_done_arrives() {
        let mut collector = SnapshotCollector::default();
        collector.note_head(1);
        collector.set_head_name(1, "eDP-1".to_string()).unwrap();
        assert_eq!(collector.publications.len(), 0);

        collector.note_mode(1, 20).unwrap();
        collector.set_mode_size(20, 2256, 1504).unwrap();
        assert_eq!(collector.publications.len(), 0);

        collector.publish_done(1).unwrap();
        assert_eq!(collector.publications.len(), 1);

        collector
            .set_head_description(1, "Internal display".to_string())
            .unwrap();
        assert_eq!(collector.publications.len(), 1);

        collector.publish_done(2).unwrap();
        assert_eq!(collector.publications.len(), 2);
    }

    #[test]
    fn preserves_repeated_done_publications_in_order() {
        let mut collector = SnapshotCollector::default();
        collector.note_head(1);
        collector.set_head_name(1, "eDP-1".to_string()).unwrap();

        collector.publish_done(5).unwrap();
        collector
            .set_head_description(1, "Internal display".to_string())
            .unwrap();
        collector.publish_done(6).unwrap();

        let first = collector.take_publication().unwrap();
        let second = collector.take_publication().unwrap();

        assert_eq!(first.serial, 5);
        assert_eq!(first.heads[0].description, None);
        assert_eq!(second.serial, 6);
        assert_eq!(
            second.heads[0].description.as_deref(),
            Some("Internal display")
        );
    }

    #[test]
    fn publication_helper_drains_snapshots_before_terminal_error() {
        let mut state = ObserverState::default();
        state.collector.note_head(1);
        state
            .collector
            .set_head_name(1, "eDP-1".to_string())
            .unwrap();
        state.collector.publish_done(5).unwrap();
        state.collector.publish_done(6).unwrap();
        state.store_terminal(WaylandObserverError::ManagerFinished);

        let (tx, rx) = mpsc::channel();
        let status = publish_pending_results(&mut state, &tx);
        let published = rx.try_iter().collect::<Vec<_>>();

        assert_eq!(status, PublishStatus::Stop);
        assert_eq!(published.len(), 3);
        assert_eq!(
            published[0],
            Ok(OutputSnapshot {
                serial: 5,
                heads: vec![first_head_snapshot("eDP-1")],
            })
        );
        assert_eq!(
            published[1],
            Ok(OutputSnapshot {
                serial: 6,
                heads: vec![first_head_snapshot("eDP-1")],
            })
        );
        assert_eq!(published[2], Err(WaylandObserverError::ManagerFinished));
    }

    #[test]
    fn publication_helper_stops_when_receiver_is_dropped() {
        let mut state = ObserverState::default();
        state.collector.note_head(3);
        state
            .collector
            .set_head_name(3, "HDMI-A-1".to_string())
            .unwrap();
        state.collector.publish_done(9).unwrap();

        let (tx, rx) = mpsc::channel();
        drop(rx);

        assert_eq!(
            publish_pending_results(&mut state, &tx),
            PublishStatus::Stop
        );
    }

    fn first_head_snapshot(name: &str) -> super::OutputHeadSnapshot {
        super::OutputHeadSnapshot {
            name: name.to_string(),
            description: None,
            enabled: false,
            position: None,
            transform: None,
            scale: None,
            current_mode: None,
            modes: Vec::new(),
        }
    }
}
