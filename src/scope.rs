//! Oscilloscope state: rolling buffers, time window, autoscale, pause, periodized metrics.
//!
//! This is the model behind both the terminal UI and the browser UI. It is portable and
//! has no clock of its own: every sample carries the host time (ms) at which it arrived,
//! which the caller supplies.
//!
//! **Sample times.** A packet carries several samples and one header timestamp (the time
//! of the *last* sample). As in the DataParser, sample `i` of `N` is placed at
//! `prev + (i+1)/N * (now - prev)` on the device clock, where `prev` is the previous
//! packet's timestamp for the same tag; that spacing is anchored to the arrival time of the
//! packet. Network jitter then shifts a whole packet but never reorders or bunches its samples.

use std::collections::{BTreeMap, VecDeque};

use crate::health::StreamHealth;
use crate::parse::to_event;
use crate::periodizer::Periodizer;
use crate::protocol::{Packet, tag};
use crate::status::{DeviceStatus, StatusTracker};
use crate::types::{CHANNELS, EmotiBitEvent, SensorReading};

/// Time windows the arrow keys step through, in seconds.
pub const WINDOWS_S: &[f64] = &[1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0];
const DEFAULT_WINDOW_IDX: usize = 3;

/// Samples kept per series: 60 s at the fastest channel (25 Hz) with headroom.
const SERIES_CAP: usize = 4_000;

/// A single plotted signal.
#[derive(Debug, Clone, Default)]
pub struct Series {
    /// `(host time ms, value)`, oldest first.
    samples: VecDeque<(f64, f64)>,
    last_device_ts: Option<u32>,
}

impl Series {
    fn push(&mut self, t: f64, v: f64) {
        if self.samples.len() == SERIES_CAP {
            self.samples.pop_front();
        }
        self.samples.push_back((t, v));
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn last(&self) -> Option<f64> {
        self.samples.back().map(|s| s.1)
    }
}

/// Rolling view of everything a device is sending.
#[derive(Debug)]
pub struct Scope {
    series: BTreeMap<&'static str, Series>,
    periodizers: Vec<Periodizer>,
    health: StreamHealth,
    status: StatusTracker,
    window_idx: usize,
    paused_at: Option<f64>,
    latest_ms: f64,
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

impl Scope {
    pub fn new() -> Self {
        Self {
            series: BTreeMap::new(),
            periodizers: vec![
                Periodizer::heart_rate(),
                Periodizer::scr_amplitude(),
                Periodizer::scr_rise_time(),
                Periodizer::scr_frequency(),
            ],
            health: StreamHealth::new(),
            status: StatusTracker::new(),
            window_idx: DEFAULT_WINDOW_IDX,
            paused_at: None,
            latest_ms: 0.0,
        }
    }

    // ── Ingest ────────────────────────────────────────────────────────────────

    /// Feed a decoded packet (as received at `received_at_ms`).
    pub fn push_packet(&mut self, packet: Packet, received_at_ms: f64) {
        match to_event(packet, received_at_ms) {
            EmotiBitEvent::Sensor(r) => self.push_reading(&r),
            EmotiBitEvent::Message(p) => {
                self.health.observe_number(p.number);
                self.status.update_packet(&p);
            }
            _ => {}
        }
    }

    /// Feed one sensor reading.
    pub fn push_reading(&mut self, r: &SensorReading) {
        self.latest_ms = self.latest_ms.max(r.received_at_ms);
        self.health.record(r);
        self.status.update_reading(r);
        if r.samples.is_empty() {
            return;
        }

        let times = self.sample_times(r);
        let is_aperiodic = self.periodizers.iter().any(|p| p.input_tag() == r.channel.tag);

        // Aperiodic metrics are only plotted through their periodizer (HR, SCR:*).
        if !is_aperiodic {
            let s = self.series.entry(r.channel.tag).or_default();
            for (t, v) in times.iter().zip(&r.samples) {
                s.push(*t, *v);
            }
        }
        for p in &mut self.periodizers {
            let out = p.update(r.channel.tag, &r.samples);
            if !out.is_empty() {
                let s = self.series.entry(p.input_tag()).or_default();
                for (t, v) in times.iter().zip(out) {
                    s.push(*t, v);
                }
            }
        }
        // Remember the aperiodic packet too, so its own timing is tracked.
        if is_aperiodic {
            self.series.entry(r.channel.tag).or_default();
        }
    }

    /// Host-clock time of each sample in `r` (see the module docs).
    fn sample_times(&mut self, r: &SensorReading) -> Vec<f64> {
        let n = r.samples.len();
        let s = self.series.entry(r.channel.tag).or_default();
        let prev = s.last_device_ts.replace(r.device_timestamp_ms);
        match prev {
            Some(prev) if r.device_timestamp_ms > prev => {
                let span = (r.device_timestamp_ms - prev) as f64;
                (0..n)
                    .map(|i| {
                        let frac = (i + 1) as f64 / n as f64;
                        // Distance of this sample before the packet's header time.
                        r.received_at_ms - span * (1.0 - frac)
                    })
                    .collect()
            }
            _ => vec![r.received_at_ms; n],
        }
    }

    // ── View ──────────────────────────────────────────────────────────────────

    pub fn window_s(&self) -> f64 {
        WINDOWS_S[self.window_idx]
    }

    pub fn increment_window(&mut self) {
        self.window_idx = (self.window_idx + 1).min(WINDOWS_S.len() - 1);
    }

    pub fn decrement_window(&mut self) {
        self.window_idx = self.window_idx.saturating_sub(1);
    }

    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    /// Freeze or resume the display. Data keeps being collected while paused.
    pub fn toggle_pause(&mut self) {
        self.paused_at = match self.paused_at {
            Some(_) => None,
            None => Some(self.latest_ms),
        };
    }

    fn view_end(&self) -> f64 {
        self.paused_at.unwrap_or(self.latest_ms)
    }

    /// Tags that have data, in the Oscilloscope's channel order (then any others, by tag).
    pub fn tags(&self) -> Vec<&'static str> {
        let has = |t: &str| self.series.get(t).is_some_and(|s| !s.is_empty());
        let mut out: Vec<&'static str> = CHANNELS.iter().map(|c| c.tag).filter(|t| has(t)).collect();
        out.extend(self.series.keys().copied().filter(|t| has(t) && !out.contains(t)).collect::<Vec<_>>());
        out
    }

    /// Points inside the current window as `(seconds before the newest sample, value)`.
    /// The x values are `<= 0`, so they plot left to right in time order.
    pub fn window(&self, tag: &str) -> Vec<(f64, f64)> {
        let Some(s) = self.series.get(tag) else { return Vec::new() };
        let end = self.view_end();
        let start = end - self.window_s() * 1000.0;
        s.samples
            .iter()
            .filter(|(t, _)| *t >= start && *t <= end)
            .map(|(t, v)| ((t - end) / 1000.0, *v))
            .collect()
    }

    /// Autoscaled `(min, max)` over the current window, padded 5% and never zero-height.
    pub fn y_range(&self, tag: &str) -> (f64, f64) {
        let pts = self.window(tag);
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for (_, v) in &pts {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
        if !lo.is_finite() {
            return (0.0, 1.0);
        }
        // EDA is a slow, tiny signal: keep a minimum span relative to its level, like the
        // Oscilloscope (0.1 * level^1.5), so noise is not blown up to full height.
        let min_span = if tag == tag::EDA { 0.1 * lo.abs().max(1e-6).powf(1.5) } else { 1e-9 };
        let span = (hi - lo).max(min_span);
        let pad = span * 0.05;
        let mid = (hi + lo) / 2.0;
        (mid - span / 2.0 - pad, mid + span / 2.0 + pad)
    }

    pub fn last_value(&self, tag: &str) -> Option<f64> {
        self.series.get(tag).and_then(Series::last)
    }

    pub fn status(&self) -> &DeviceStatus {
        self.status.status()
    }

    pub fn health(&self) -> &StreamHealth {
        &self.health
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::channel;

    fn reading(tag: &str, number: u16, dev: u32, at: f64, samples: Vec<f64>) -> SensorReading {
        SensorReading {
            channel: channel(tag).unwrap(),
            packet_number: number,
            device_timestamp_ms: dev,
            received_at_ms: at,
            samples,
        }
    }

    #[test]
    fn samples_are_spaced_across_the_packet_interval() {
        let mut s = Scope::new();
        s.push_reading(&reading("PG", 1, 1000, 10_000.0, vec![1.0]));
        // Next packet: 3 samples over 120 device-ms, so 40 ms apart, ending at arrival.
        s.push_reading(&reading("PG", 2, 1120, 10_120.0, vec![2.0, 3.0, 4.0]));
        let w = s.window("PG");
        let times: Vec<f64> = w.iter().map(|(x, _)| (x * 1000.0).round()).collect();
        assert_eq!(times, vec![-120.0, -80.0, -40.0, 0.0]);
    }

    #[test]
    fn window_limits_and_steps() {
        let mut s = Scope::new();
        for i in 0..40u32 {
            s.push_reading(&reading("AX", i as u16, i * 1000, i as f64 * 1000.0, vec![i as f64]));
        }
        assert_eq!(s.window_s(), 10.0);
        assert!(s.window("AX").len() <= 11);
        s.decrement_window();
        s.decrement_window();
        assert_eq!(s.window_s(), 2.0);
        assert!(s.window("AX").len() <= 3);
    }

    #[test]
    fn pause_freezes_the_view_but_not_the_data() {
        let mut s = Scope::new();
        s.push_reading(&reading("AX", 1, 0, 1000.0, vec![1.0]));
        s.toggle_pause();
        s.push_reading(&reading("AX", 2, 1000, 2000.0, vec![2.0]));
        // View still ends at the pause moment, so the newer point is outside it.
        assert_eq!(s.window("AX").len(), 1);
        s.toggle_pause();
        assert_eq!(s.window("AX").len(), 2);
    }

    #[test]
    fn heart_rate_is_periodized_against_ppg() {
        let mut s = Scope::new();
        s.push_reading(&reading("HR", 1, 100, 1000.0, vec![71.0]));
        s.push_reading(&reading("PI", 1, 100, 1000.0, vec![1.0, 2.0, 3.0]));
        assert_eq!(s.window("HR").len(), 3);
        assert_eq!(s.last_value("HR"), Some(71.0));
    }

    #[test]
    fn tags_follow_the_channel_table_order() {
        let mut s = Scope::new();
        for tag in ["MX", "AX", "EA", "PG"] {
            s.push_reading(&reading(tag, 1, 100, 1000.0, vec![1.0]));
        }
        // CHANNELS order is EDA, PPG, ..., ACC, ..., MAG — not alphabetical (AX, EA, MX, PG).
        // (EDA also creates the periodized SCR series, so look only at the tags we pushed.)
        let pushed: Vec<_> = s.tags().into_iter().filter(|t| ["MX", "AX", "EA", "PG"].contains(t)).collect();
        assert_eq!(pushed, vec!["EA", "PG", "AX", "MX"]);
    }

    #[test]
    fn autoscale_pads_and_handles_flat_signals() {
        let mut s = Scope::new();
        for i in 0..5u32 {
            s.push_reading(&reading("AX", i as u16, i * 100, i as f64 * 100.0, vec![2.0]));
        }
        let (lo, hi) = s.y_range("AX");
        assert!(lo < 2.0 && hi > 2.0);
        assert_eq!(s.y_range("nothing"), (0.0, 1.0));
    }
}
