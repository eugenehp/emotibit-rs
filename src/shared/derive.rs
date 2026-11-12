//! Derived channels for the BLE firmware, which streams only raw sensors: heart rate and
//! inter-beat interval from the infrared PPG, and skin-conductance-response (SCR) amplitude /
//! rise time / frequency from EDA. Shared verbatim by the host (live stream) and the firmware
//! (SD files); `core` + `alloc` only.
//!
//! These are simple, documented detectors; they are not the stock firmware's exact algorithms,
//! so expect similar rather than identical values.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Intervals that must agree (within 25% of their median) before beats are reported.
const REGULAR_BEATS: usize = 4;

/// One detected beat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Beat {
    /// Inter-beat interval in milliseconds.
    pub ibi_ms: f64,
    /// Heart rate in beats per minute, averaged over the last few beats.
    pub bpm: f64,
}

/// Peak detector on the infrared PPG. The pulse is deflected the same way the signal sags
/// when blood volume rises, so both polarities are handled by tracking the sign of the
/// first-order difference of a detrended, smoothed signal.
#[derive(Debug)]
pub struct HeartRate {
    rate: f64,
    baseline: f64,
    smooth: f64,
    prev: f64,
    prev_slope: f64,
    sample: u64,
    last_peak: Option<u64>,
    level: f64,
    ibis: VecDeque<f64>,
}

impl HeartRate {
    /// `rate` is the PPG sampling rate in Hz.
    pub fn new(rate: f64) -> Self {
        Self { rate, baseline: 0.0, smooth: 0.0, prev: 0.0, prev_slope: 0.0, sample: 0, last_peak: None, level: 0.0, ibis: VecDeque::new() }
    }

    /// Feed one sample; returns a beat when one completes.
    pub fn push(&mut self, v: f64) -> Option<Beat> {
        self.sample += 1;
        if self.sample == 1 {
            self.baseline = v;
            self.smooth = v;
        }
        // Slow baseline (about 1.5 s) removed, then a short (about 0.12 s) smoother.
        self.baseline += (v - self.baseline) / (1.5 * self.rate).max(1.0);
        let x = v - self.baseline;
        self.smooth += (x - self.smooth) / (0.12 * self.rate).max(1.0);
        let slope = self.smooth - self.prev;
        self.prev = self.smooth;
        // Running amplitude estimate, used as the detection threshold.
        self.level += (self.smooth.abs() - self.level) / (3.0 * self.rate).max(1.0);

        let mut beat = None;
        if self.prev_slope > 0.0 && slope <= 0.0 && self.smooth > 0.5 * self.level && self.sample > (2.0 * self.rate) as u64 {
            let refractory = (0.3 * self.rate) as u64;
            match self.last_peak {
                Some(p) if self.sample - p < refractory => {}
                Some(p) => {
                    let ibi_ms = (self.sample - p) as f64 / self.rate * 1000.0;
                    self.last_peak = Some(self.sample);
                    self.ibis.push_back(ibi_ms);
                    if self.ibis.len() > REGULAR_BEATS {
                        self.ibis.pop_front();
                    }
                    // Only a steady rhythm counts as a pulse: noise from a sensor that is not on
                    // skin also has peaks, but their spacing is irregular.
                    if self.ibis.len() == REGULAR_BEATS {
                        let mut sorted: Vec<f64> = self.ibis.iter().copied().collect();
                        sorted.sort_by(f64::total_cmp);
                        let median = (sorted[1] + sorted[2]) / 2.0;
                        let steady = self.ibis.iter().all(|i| (i - median).abs() <= 0.25 * median);
                        if steady && (300.0..=2000.0).contains(&median) {
                            let mean = self.ibis.iter().sum::<f64>() / REGULAR_BEATS as f64;
                            beat = Some(Beat { ibi_ms, bpm: 60_000.0 / mean });
                        }
                    }
                }
                None => self.last_peak = Some(self.sample),
            }
        }
        self.prev_slope = slope;
        beat
    }
}

/// One detected skin conductance response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scr {
    /// Rise from onset to peak, in microsiemens.
    pub amplitude: f64,
    /// Seconds from onset to peak.
    pub rise_time: f64,
    /// Responses per minute over the last minute.
    pub per_minute: f64,
}

/// Onset/peak detector on EDA. A response starts when conductance rises, peaks when it stops
/// rising, and counts when the rise is at least `min_amplitude` (stock firmware uses 0.01 uS).
#[derive(Debug)]
pub struct ScrDetector {
    rate: f64,
    min_amplitude: f64,
    smooth: f64,
    prev: f64,
    sample: u64,
    onset: Option<(u64, f64)>,
    peaks: VecDeque<u64>,
}

impl ScrDetector {
    pub fn new(rate: f64) -> Self {
        Self { rate, min_amplitude: 0.01, smooth: 0.0, prev: 0.0, sample: 0, onset: None, peaks: VecDeque::new() }
    }

    pub fn push(&mut self, v: f64) -> Option<Scr> {
        self.sample += 1;
        if self.sample == 1 {
            self.smooth = v;
            self.prev = v;
        }
        self.smooth += (v - self.smooth) / (0.3 * self.rate).max(1.0);
        let rising = self.smooth > self.prev + 1e-5;
        let mut out = None;
        match (self.onset, rising) {
            (None, true) => self.onset = Some((self.sample, self.prev)),
            (Some((start, base)), false) => {
                let amplitude = self.prev - base;
                if amplitude >= self.min_amplitude {
                    self.peaks.push_back(self.sample);
                    let window = (60.0 * self.rate) as u64;
                    while self.peaks.front().is_some_and(|&p| self.sample - p > window) {
                        self.peaks.pop_front();
                    }
                    let span = (self.sample.min(window)) as f64 / self.rate / 60.0;
                    out = Some(Scr {
                        amplitude,
                        rise_time: (self.sample - start) as f64 / self.rate,
                        per_minute: self.peaks.len() as f64 / span.max(1.0 / 60.0),
                    });
                }
                self.onset = None;
            }
            _ => {}
        }
        self.prev = self.smooth;
        out
    }
}

/// Heart rate and SCR together, fed with stock-tagged values: `PI` drives the heart rate, `EA`
/// the SCR detector. For files that should carry the derived channels as stock files do.
#[derive(Debug)]
pub struct Derived {
    heart: HeartRate,
    scr: ScrDetector,
}

impl Derived {
    /// `ppg_hz`, `eda_hz`: the rates the PPG and EDA values arrive at.
    pub fn new(ppg_hz: f64, eda_hz: f64) -> Self {
        Self { heart: HeartRate::new(ppg_hz), scr: ScrDetector::new(eda_hz) }
    }

    /// Feeds one value; calls `out(tag, value)` for each derived value it completes
    /// (`HR`, `BI` on a beat; `SA`, `SR`, `SF` on a skin-conductance response).
    pub fn push(&mut self, tag: &str, v: f64, mut out: impl FnMut(&'static str, f64)) {
        match tag {
            "PI" => {
                if let Some(b) = self.heart.push(v) {
                    out("HR", b.bpm);
                    out("BI", b.ibi_ms);
                }
            }
            "EA" => {
                if let Some(r) = self.scr.push(v) {
                    out("SA", r.amplitude);
                    out("SR", r.rise_time);
                    out("SF", r.per_minute);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heart_rate_follows_a_synthetic_pulse() {
        let rate = 25.0;
        let mut hr = HeartRate::new(rate);
        let mut last = None;
        for i in 0..(rate as usize * 30) {
            let t = i as f64 / rate;
            // 72 bpm pulse on a drifting baseline.
            let v = 1000.0 + 5.0 * t + 40.0 * (2.0 * core::f64::consts::PI * 1.2 * t).sin();
            if let Some(b) = hr.push(v) {
                last = Some(b);
            }
        }
        let b = last.expect("beats detected");
        assert!((b.bpm - 72.0).abs() < 4.0, "{b:?}");
        assert!((b.ibi_ms - 833.0).abs() < 90.0, "{b:?}");
    }

    #[test]
    fn heart_rate_ignores_noise() {
        // Off-skin PPG: small random wander, no pulse. A fixed-seed LCG keeps it reproducible.
        let mut hr = HeartRate::new(25.0);
        let mut state = 12345u64;
        let mut beats = 0;
        for _ in 0..25 * 60 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = ((state >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 40.0;
            beats += hr.push(1100.0 + noise).is_some() as u32;
        }
        assert!(beats < 5, "{beats} beats from noise");
    }

    #[test]
    fn scr_counts_responses() {
        let rate = 15.0;
        let mut d = ScrDetector::new(rate);
        let mut found = Vec::new();
        for i in 0..(rate as usize * 60) {
            let t = i as f64 / rate;
            // 0.05 uS bumps every 10 s on a flat 2 uS tonic level.
            let phase = t % 10.0;
            let bump = if phase < 3.0 { 0.05 * (core::f64::consts::PI * phase / 3.0).sin() } else { 0.0 };
            if let Some(s) = d.push(2.0 + bump) {
                found.push(s);
            }
        }
        assert!(found.len() >= 5, "{}", found.len());
        assert!(found.iter().all(|s| s.amplitude >= 0.01 && s.rise_time > 0.2));
    }
}
