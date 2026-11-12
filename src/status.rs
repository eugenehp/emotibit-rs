//! Device status: battery, recording, power mode, error counters.
//!
//! The Oscilloscope's header bar is driven by a handful of packets that are not plotted:
//! `BV`/`B%` (battery), `SD` (card fill), `EM` (mode: recording and power state),
//! `DC`/`DO` (data clipping/overflow) and `BS`/`BL` (button presses). [`StatusTracker`]
//! folds them into one [`DeviceStatus`].
//!
//! An `EM` packet looks like `RS,RE,PS,MN` (recording ended, normal power) or
//! `RS,RB,<file>.csv,PS,MN` while recording.

use serde::Serialize;

use crate::protocol::{tag, Packet};
use crate::types::SensorReading;

/// Power modes the stock firmware understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PowerMode {
    Normal,
    Low,
    MaxLow,
    WirelessOff,
    Hibernate,
}

impl PowerMode {
    /// The control-packet tag that selects this mode.
    pub fn tag(self) -> &'static str {
        match self {
            PowerMode::Normal => tag::MODE_NORMAL_POWER,
            PowerMode::Low => tag::MODE_LOW_POWER,
            PowerMode::MaxLow => tag::MODE_MAX_LOW_POWER,
            PowerMode::WirelessOff => tag::MODE_WIRELESS_OFF,
            PowerMode::Hibernate => tag::MODE_HIBERNATE,
        }
    }

    pub fn from_tag(t: &str) -> Option<Self> {
        Some(match t {
            tag::MODE_NORMAL_POWER => PowerMode::Normal,
            tag::MODE_LOW_POWER => PowerMode::Low,
            tag::MODE_MAX_LOW_POWER => PowerMode::MaxLow,
            tag::MODE_WIRELESS_OFF => PowerMode::WirelessOff,
            tag::MODE_HIBERNATE => PowerMode::Hibernate,
            _ => return None,
        })
    }

    /// Parse a user-facing name: `normal`, `low`, `max-low`, `wireless-off`, `hibernate`.
    pub fn from_name(n: &str) -> Option<Self> {
        Some(match n.to_ascii_lowercase().replace('_', "-").as_str() {
            "normal" | "normal-power" => PowerMode::Normal,
            "low" | "low-power" => PowerMode::Low,
            "max-low" | "max-low-power" => PowerMode::MaxLow,
            "wireless-off" | "off" => PowerMode::WirelessOff,
            "hibernate" => PowerMode::Hibernate,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            PowerMode::Normal => "normal",
            PowerMode::Low => "low",
            PowerMode::MaxLow => "max-low",
            PowerMode::WirelessOff => "wireless-off",
            PowerMode::Hibernate => "hibernate",
        }
    }
}

/// Everything the header bar shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeviceStatus {
    pub battery_percent: Option<f64>,
    pub battery_voltage: Option<f64>,
    pub sd_percent: Option<f64>,
    /// `Some(true)` while the device records to its SD card.
    pub recording: Option<bool>,
    /// File being written, when the device reported it.
    pub recording_file: Option<String>,
    pub power_mode: Option<PowerMode>,
    /// Count of `DC` payload entries seen (each names a clipped channel).
    pub clipping_events: u32,
    /// Count of `DO` payload entries seen.
    pub overflow_events: u32,
    pub short_presses: u32,
    pub long_presses: u32,
}

/// Folds packets into a [`DeviceStatus`].
#[derive(Debug, Default)]
pub struct StatusTracker {
    status: DeviceStatus,
}

impl StatusTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn status(&self) -> &DeviceStatus {
        &self.status
    }

    /// Feed a sensor-style reading (battery and SD fill arrive as readings). True if it changed anything.
    pub fn update_reading(&mut self, r: &SensorReading) -> bool {
        let Some(&v) = r.samples.last() else { return false };
        let before = self.status.clone();
        match r.channel.tag {
            tag::BATTERY_PERCENT => self.status.battery_percent = Some(v),
            tag::BATTERY_VOLTAGE => self.status.battery_voltage = Some(v),
            tag::SD_CARD_PERCENT => self.status.sd_percent = Some(v),
            _ => {}
        }
        before != self.status
    }

    /// Feed any other packet. True if it changed the status.
    pub fn update_packet(&mut self, p: &Packet) -> bool {
        let before = self.status.clone();
        match p.tag.as_str() {
            tag::EMOTIBIT_MODE => self.apply_mode(p),
            tag::DATA_CLIPPING => self.status.clipping_events += p.payload.len().max(1) as u32,
            tag::DATA_OVERFLOW => self.status.overflow_events += p.payload.len().max(1) as u32,
            tag::BUTTON_PRESS_SHORT => self.status.short_presses += 1,
            tag::BUTTON_PRESS_LONG => self.status.long_presses += 1,
            _ => {}
        }
        before != self.status
    }

    fn apply_mode(&mut self, p: &Packet) {
        if let Some(i) = p.payload.iter().position(|e| e == "RS") {
            match p.payload.get(i + 1).map(String::as_str) {
                Some(tag::RECORD_BEGIN) => {
                    self.status.recording = Some(true);
                    // The element after RB is the file name unless it is the next key
                    // (`foo.csv` from the stock firmware, `EB000001.CSV`/`.EBB` from emotibit-ble).
                    self.status.recording_file = p.payload.get(i + 2).filter(|f| f.contains('.')).cloned();
                }
                Some(tag::RECORD_END) => {
                    self.status.recording = Some(false);
                    self.status.recording_file = None;
                }
                _ => {}
            }
        }
        if let Some(i) = p.payload.iter().position(|e| e == "PS")
            && let Some(mode) = p.payload.get(i + 1).and_then(|t| PowerMode::from_tag(t)) {
                self.status.power_mode = Some(mode);
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::to_event;
    use crate::types::EmotiBitEvent;

    fn pkt(s: &str) -> Packet {
        Packet::parse(s).unwrap()
    }

    #[test]
    fn mode_packet_idle() {
        let mut t = StatusTracker::new();
        assert!(t.update_packet(&pkt("10,1,4,EM,1,100,RS,RE,PS,MN")));
        assert_eq!(t.status().recording, Some(false));
        assert_eq!(t.status().power_mode, Some(PowerMode::Normal));
        // Same packet again changes nothing.
        assert!(!t.update_packet(&pkt("11,2,4,EM,1,100,RS,RE,PS,MN")));
    }

    #[test]
    fn mode_packet_recording_with_filename() {
        let mut t = StatusTracker::new();
        t.update_packet(&pkt("10,1,5,EM,1,100,RS,RB,2026-11-05_17-54-49-111.csv,PS,ML"));
        assert_eq!(t.status().recording, Some(true));
        assert_eq!(t.status().recording_file.as_deref(), Some("2026-11-05_17-54-49-111.csv"));
        assert_eq!(t.status().power_mode, Some(PowerMode::Low));
    }

    #[test]
    fn battery_and_counters() {
        let mut t = StatusTracker::new();
        if let EmotiBitEvent::Sensor(r) = to_event(pkt("1,1,1,B%,1,100,87"), 0.0) {
            assert!(t.update_reading(&r));
        }
        t.update_packet(&pkt("1,2,2,DC,1,100,PG,PI"));
        t.update_packet(&pkt("1,3,0,BS,1,100"));
        assert_eq!(t.status().battery_percent, Some(87.0));
        assert_eq!(t.status().clipping_events, 2);
        assert_eq!(t.status().short_presses, 1);
    }

    #[test]
    fn power_mode_names() {
        assert_eq!(PowerMode::from_name("Wireless_Off"), Some(PowerMode::WirelessOff));
        assert_eq!(PowerMode::Hibernate.tag(), "MH");
        assert_eq!(PowerMode::from_name("nope"), None);
    }
}
