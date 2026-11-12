//! Data types produced by the client.

use std::net::IpAddr;

use crate::protocol::{tag, Packet};
use crate::status::DeviceStatus;

/// Static description of a sensor/metric packet tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    pub tag: &'static str,
    /// Human-readable name, as the Oscilloscope labels it.
    pub name: &'static str,
    /// Physical unit; empty when the stock firmware sends raw counts or the unit
    /// is not documented in the protocol sources.
    pub unit: &'static str,
}

const fn ch(tag: &'static str, name: &'static str, unit: &'static str) -> Channel {
    Channel { tag, name, unit }
}

/// Every numeric tag the stock firmware streams.
pub const CHANNELS: &[Channel] = &[
    ch(tag::EDA, "EDA", "µS"),
    ch(tag::EDL, "EDL", ""),
    ch(tag::EDR, "EDR", ""),
    ch(tag::PPG_INFRARED, "PPG:IR", ""),
    ch(tag::PPG_RED, "PPG:RED", ""),
    ch(tag::PPG_GREEN, "PPG:GRN", ""),
    ch(tag::SPO2, "SpO2", "%"),
    ch(tag::TEMPERATURE_0, "TEMP0", "°C"),
    ch(tag::TEMPERATURE_1, "TEMP1", "°C"),
    ch(tag::THERMOPILE, "THERM", "°C"),
    ch(tag::HUMIDITY_0, "HUMID0", "%"),
    ch(tag::ACCELEROMETER_X, "ACC:X", "g"),
    ch(tag::ACCELEROMETER_Y, "ACC:Y", "g"),
    ch(tag::ACCELEROMETER_Z, "ACC:Z", "g"),
    ch(tag::GYROSCOPE_X, "GYRO:X", "°/s"),
    ch(tag::GYROSCOPE_Y, "GYRO:Y", "°/s"),
    ch(tag::GYROSCOPE_Z, "GYRO:Z", "°/s"),
    ch(tag::MAGNETOMETER_X, "MAG:X", "µT"),
    ch(tag::MAGNETOMETER_Y, "MAG:Y", "µT"),
    ch(tag::MAGNETOMETER_Z, "MAG:Z", "µT"),
    ch(tag::HEART_RATE, "HR", "bpm"),
    ch(tag::INTER_BEAT_INTERVAL, "IBI", "ms"),
    ch(tag::SCR_AMPLITUDE, "SCR:AMP", ""),
    ch(tag::SCR_FREQUENCY, "SCR:FREQ", ""),
    ch(tag::SCR_RISE_TIME, "SCR:RIS", ""),
    ch(tag::BATTERY_VOLTAGE, "BATT:V", "V"),
    ch(tag::BATTERY_PERCENT, "BATT:%", "%"),
    ch(tag::SD_CARD_PERCENT, "SD:%", "%"),
];

/// Look up a tag in [`CHANNELS`].
pub fn channel(tag: &str) -> Option<Channel> {
    CHANNELS.iter().copied().find(|c| c.tag == tag)
}

/// A batch of samples from one channel: one packet.
#[derive(Debug, Clone, PartialEq)]
pub struct SensorReading {
    pub channel: Channel,
    /// Packet counter from the device.
    pub packet_number: u16,
    /// Device clock (ms since boot) at the packet's header.
    pub device_timestamp_ms: u32,
    /// Host wall clock (ms since Unix epoch) when the packet arrived.
    pub received_at_ms: f64,
    /// Values, oldest first. Stock firmware batches several samples per packet.
    pub samples: Vec<f64>,
}

/// A device seen during discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmotiBitDevice {
    /// Device id from the `DI` field (e.g. `MD-V6-0000356`); the IP when absent.
    pub id: String,
    pub ip: IpAddr,
    /// False when another host is already connected to it.
    pub available: bool,
}

impl EmotiBitDevice {
    /// A board running the custom BLE firmware (it has no IP; `ip` is unspecified).
    pub fn is_ble(&self) -> bool {
        self.id.starts_with("EmotiBit-BLE")
    }
}

/// What the client reports.
#[derive(Debug, Clone)]
pub enum EmotiBitEvent {
    /// The device accepted the connection.
    Connected(EmotiBitDevice),
    /// A numeric sensor, derived-metric or status packet.
    Sensor(SensorReading),
    /// Device status changed (battery, recording, power mode, error counters).
    Status(DeviceStatus),
    /// Any other packet: `EI` info, `DC`/`DO` clipping/overflow, button presses, ACKs, ...
    Message(Packet),
    /// The link ended (timeout, device hibernated, or requested disconnect).
    Disconnected,
}
