use serde::{Deserialize, Serialize};
use swayipc_async::{Mode as SwayMode, Output};
use zvariant::{DeserializeDict, SerializeDict, Type};

use crate::wayland_observer::OutputModeSnapshot;

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
        Modes {
            width,
            height,
            supported_scales: Self::supported_scales(width, height),
            id: Self::mode_id(width, height, (mode_info.refresh as f64) / 1000f64),
            preferred_scale: 1f64,
            refresh_rate: (refresh as f64) / 1000f64,
            properties,
        }
    }

    pub fn from_snapshot(mode_info: &OutputModeSnapshot) -> Option<Modes> {
        let width = mode_info.width;
        let height = mode_info.height;
        let refresh_rate = (mode_info.refresh_mhz? as f64) / 1000f64;

        Some(Modes {
            id: Self::mode_id(width, height, refresh_rate),
            width,
            height,
            refresh_rate,
            preferred_scale: 1f64,
            supported_scales: Self::supported_scales(width, height),
            properties: ModeProperties {
                current: Some(mode_info.current),
                preferred: Some(mode_info.preferred),
                interlaced: Some(false),
            },
        })
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

    fn mode_id(width: i32, height: i32, refresh_rate: f64) -> String {
        format!("{width}x{height}@{refresh_rate}Hz")
    }

    fn supported_scales(width: i32, height: i32) -> Vec<f64> {
        if width >= 1920 && height >= 1080 {
            [1.0, 1.25, 1.5, 1.75, 2.0].to_vec()
        } else {
            [1.0, 2.0].to_vec()
        }
    }
}

#[cfg(test)]
impl Modes {
    pub(crate) fn test_new(id: &str) -> Modes {
        Modes {
            id: id.to_string(),
            width: 1024,
            height: 768,
            refresh_rate: 60.0,
            preferred_scale: 1.0,
            supported_scales: vec![1.0],
            properties: ModeProperties {
                current: Some(true),
                preferred: Some(false),
                interlaced: Some(false),
            },
        }
    }

    pub(crate) fn test_new_without_current(id: &str) -> Modes {
        let mut mode = Self::test_new(id);
        mode.properties.current = Some(false);
        mode
    }
}

#[cfg(test)]
mod tests {
    use super::Modes;
    use crate::wayland_observer::OutputModeSnapshot;

    #[test]
    fn builds_wayland_mode_with_current_and_preferred_flags() {
        let mode = Modes::from_snapshot(&OutputModeSnapshot {
            width: 2560,
            height: 1440,
            refresh_mhz: Some(143_998),
            preferred: true,
            current: true,
        })
        .unwrap();

        assert_eq!(mode.get_id(), "2560x1440@143.998Hz");
        assert!(mode.current());
        assert!(mode.is_valid_scale(1.25));
        assert!(!mode.is_valid_scale(1.3));
    }

    #[test]
    fn drops_wayland_mode_without_refresh() {
        let mode = Modes::from_snapshot(&OutputModeSnapshot {
            width: 2560,
            height: 1440,
            refresh_mhz: None,
            preferred: true,
            current: true,
        });

        assert!(mode.is_none());
    }
}
