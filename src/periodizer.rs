//! Turn irregular ("aperiodic") metrics into evenly sampled series.
//!
//! Heart rate, inter-beat interval and the SCR metrics arrive only when an event is
//! detected, but a plot wants one value per source sample. A [`Periodizer`] listens to
//! one aperiodic tag and one periodic *source* tag: every source packet produces as many
//! output values as it has samples, each holding the last aperiodic value (or a default
//! fill value with the new value in the first slot). Same behaviour as the Oscilloscope's
//! `Periodizer`.

use crate::protocol::tag;

#[derive(Debug, Clone)]
pub struct Periodizer {
    aperiodic: &'static str,
    periodic: &'static str,
    last: f64,
    /// `None`: repeat the last value. `Some(v)`: fill with `v`, emitting each new value once.
    default: Option<f64>,
    pending: bool,
}

impl Periodizer {
    pub fn new(aperiodic: &'static str, periodic: &'static str, default: Option<f64>) -> Self {
        Self { aperiodic, periodic, last: 0.0, default, pending: false }
    }

    /// Heart rate held at the PPG-IR sample rate.
    pub fn heart_rate() -> Self {
        Self::new(tag::HEART_RATE, tag::PPG_INFRARED, None)
    }

    /// The Oscilloscope's defaults for the SCR metrics: one value per EDA sample.
    pub fn scr_amplitude() -> Self {
        Self::new(tag::SCR_AMPLITUDE, tag::EDA, Some(0.0))
    }
    pub fn scr_rise_time() -> Self {
        Self::new(tag::SCR_RISE_TIME, tag::EDA, Some(0.0))
    }
    pub fn scr_frequency() -> Self {
        Self::new(tag::SCR_FREQUENCY, tag::EDA, None)
    }

    /// The aperiodic tag this one consumes.
    pub fn input_tag(&self) -> &'static str {
        self.aperiodic
    }

    /// Feed one packet. Returns the periodized samples when `tag` is the source signal, else empty.
    pub fn update(&mut self, tag: &str, samples: &[f64]) -> Vec<f64> {
        if tag == self.aperiodic {
            if let Some(&v) = samples.last() {
                self.last = v;
                self.pending = true;
            }
            return Vec::new();
        }
        if tag != self.periodic {
            return Vec::new();
        }
        match self.default {
            None => vec![self.last; samples.len()],
            Some(fill) => {
                let mut out = vec![fill; samples.len()];
                if self.pending && !out.is_empty() {
                    out[0] = self.last;
                    self.pending = false;
                }
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_last_value_at_source_rate() {
        let mut p = Periodizer::heart_rate();
        assert!(p.update("HR", &[72.0]).is_empty());
        assert_eq!(p.update("PI", &[1.0, 2.0, 3.0]), vec![72.0; 3]);
        assert!(p.update("XX", &[1.0]).is_empty());
    }

    #[test]
    fn default_fill_emits_event_once() {
        let mut p = Periodizer::scr_amplitude();
        p.update("SA", &[0.4]);
        assert_eq!(p.update("EA", &[0.0, 0.0]), vec![0.4, 0.0]);
        assert_eq!(p.update("EA", &[0.0, 0.0]), vec![0.0, 0.0]);
    }
}
