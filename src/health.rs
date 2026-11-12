//! Stream health: measured rates, lost packets and buffer underruns.
//!
//! The Oscilloscope flags a "possible buffer underrun" when one packet carries
//! [`MAX_BUFFER_LENGTH`] or more samples, and the DataParser reports "Missed packet" when
//! packet numbers skip. [`StreamHealth`] does both and adds a measured samples-per-second rate
//! per channel over a sliding window.
//!
//! **Packet numbers are one sequence for the whole device**, shared by every tag (and by
//! the non-sensor packets such as `EM`), not a counter per channel. Lost-packet detection
//! therefore watches the sequence across all packets: feed it every packet in arrival order,
//! with [`StreamHealth::observe_number`] for non-sensor packets and [`StreamHealth::record`]
//! for sensor readings.

use std::collections::{BTreeMap, VecDeque};

use serde::Serialize;

use crate::types::SensorReading;

/// A packet with at least this many samples means the device's buffer backed up.
pub const MAX_BUFFER_LENGTH: usize = 64;

/// How far back the rate estimate looks, in milliseconds.
const RATE_WINDOW_MS: f64 = 5_000.0;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChannelHealth {
    pub packets: u64,
    pub samples: u64,
    /// Packets that carried [`MAX_BUFFER_LENGTH`] or more samples.
    pub underruns: u64,
    /// Samples per second over the last few seconds.
    pub rate_hz: f64,
    #[serde(skip)]
    window: VecDeque<(f64, usize)>,
}

impl ChannelHealth {
    fn record(&mut self, r: &SensorReading) {
        self.packets += 1;
        self.samples += r.samples.len() as u64;
        if r.samples.len() >= MAX_BUFFER_LENGTH {
            self.underruns += 1;
        }

        self.window.push_back((r.received_at_ms, r.samples.len()));
        while self
            .window
            .front()
            .is_some_and(|(t, _)| r.received_at_ms - t > RATE_WINDOW_MS)
        {
            self.window.pop_front();
        }
        if let (Some((first, _)), Some((last, _))) = (self.window.front(), self.window.back()) {
            let span = (last - first) / 1000.0;
            if span > 0.0 {
                // Samples after the first packet, over the time those packets spanned.
                let n: usize = self.window.iter().skip(1).map(|(_, n)| n).sum();
                self.rate_hz = n as f64 / span;
            }
        }
    }
}

/// Per-channel health, keyed by tag, plus the device-wide packet sequence.
#[derive(Debug, Default)]
pub struct StreamHealth {
    channels: BTreeMap<&'static str, ChannelHealth>,
    last_number: Option<u16>,
    lost: u64,
}

impl StreamHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a sensor reading (also advances the device-wide packet sequence).
    pub fn record(&mut self, r: &SensorReading) {
        self.observe_number(r.packet_number);
        self.channels.entry(r.channel.tag).or_default().record(r);
    }

    /// Advance the device-wide packet sequence. Call for every non-sensor packet, in order;
    /// sensor readings do it through [`record`](Self::record).
    pub fn observe_number(&mut self, number: u16) {
        if let Some(prev) = self.last_number {
            let gap = number.wrapping_sub(prev);
            // A gap of 1 is normal. Small forward gaps are lost packets; 0 or huge values
            // are duplicates, reordering or a counter restart (device reboot), not loss.
            if (2..1000).contains(&gap) {
                self.lost += (gap - 1) as u64;
            }
        }
        self.last_number = Some(number);
    }

    pub fn channel(&self, tag: &str) -> Option<&ChannelHealth> {
        self.channels.get(tag)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&&'static str, &ChannelHealth)> {
        self.channels.iter()
    }

    /// Packet numbers that were skipped in the device's sequence, i.e. packets this host never
    /// saw. On a healthy link a stock-firmware device still skips a few numbers (2 to 3) per clock
    /// sync exchange (every ~5 s): those packets are numbered but not delivered to the host.
    /// Random WiFi loss shows up as additional gaps on top of that steady baseline.
    pub fn total_lost(&self) -> u64 {
        self.lost
    }

    pub fn total_underruns(&self) -> u64 {
        self.channels.values().map(|c| c.underruns).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::channel;

    fn reading_for(tag: &str, number: u16, at_ms: f64, n: usize) -> SensorReading {
        SensorReading {
            channel: channel(tag).unwrap(),
            packet_number: number,
            device_timestamp_ms: 0,
            received_at_ms: at_ms,
            samples: vec![0.0; n],
        }
    }

    fn reading(number: u16, at_ms: f64, n: usize) -> SensorReading {
        reading_for("AX", number, at_ms, n)
    }

    #[test]
    fn interleaved_channels_share_one_sequence() {
        // Real device: packets 1,2,3,... alternate between tags. No gaps, nothing lost.
        let mut h = StreamHealth::new();
        for i in 0..30u16 {
            let tag = ["AX", "PG", "EA"][(i % 3) as usize];
            h.record(&reading_for(tag, i, i as f64 * 10.0, 2));
        }
        assert_eq!(h.total_lost(), 0);
    }

    #[test]
    fn counts_lost_packets_from_gaps() {
        let mut h = StreamHealth::new();
        h.record(&reading(1, 0.0, 2));
        h.record(&reading_for("PG", 2, 80.0, 2));
        h.record(&reading(5, 240.0, 2)); // 3 and 4 missing
        assert_eq!(h.total_lost(), 2);
    }

    #[test]
    fn non_sensor_packets_advance_the_sequence() {
        let mut h = StreamHealth::new();
        h.record(&reading(1, 0.0, 2));
        h.observe_number(2); // e.g. an EM packet
        h.record(&reading(3, 80.0, 2));
        assert_eq!(h.total_lost(), 0);
    }

    #[test]
    fn counter_wrap_restart_and_reorder_are_not_loss() {
        let mut h = StreamHealth::new();
        h.record(&reading(65535, 0.0, 2));
        h.record(&reading(0, 80.0, 2)); // wrap
        h.observe_number(5000); // jump of thousands: device rebooted
        h.observe_number(4999); // reordered
        assert_eq!(h.total_lost(), 0);
    }

    #[test]
    fn detects_underrun_and_rate() {
        let mut h = StreamHealth::new();
        h.record(&reading(1, 0.0, 2));
        for i in 1..=10 {
            h.record(&reading(1 + i, i as f64 * 80.0, 2));
        }
        let c = h.channel("AX").unwrap();
        assert!((c.rate_hz - 25.0).abs() < 0.5, "rate {}", c.rate_hz);
        h.record(&reading(12, 960.0, 64));
        assert_eq!(h.total_underruns(), 1);
    }
}
