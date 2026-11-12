//! Device-clock to computer-clock mapping.
//!
//! EmotiBit timestamps are milliseconds since the device booted. To put samples on the
//! computer's clock, the device periodically asks the host for its time (`RD` with `TL`);
//! the host answers with a `TL` packet carrying its local time, the device stamps that
//! packet on arrival and acknowledges with `AK`. Each completed exchange is a *sync
//! point*: the device time at which the host's time was received, the host time that was
//! sent, and a round-trip time.
//!
//! Like the official DataParser, the mapping is a straight line through two anchor
//! points taken from the fastest round trips in the first and last quarter of the
//! recording, with the host time shifted by half the round trip.
//!
//! This module is portable (it also runs in the browser): the caller supplies a function
//! that turns the host's local-time string into epoch seconds.

use crate::protocol::{tag, Packet};

/// One completed `RD` → `TL` → `AK` exchange.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncPoint {
    /// Device time when it sent the request.
    pub rd: u32,
    /// Device time when the host's `TL` reply arrived.
    pub received: u32,
    /// Host clock (epoch seconds) when the reply was sent.
    pub sent_local: f64,
    /// Device time when it acknowledged.
    pub ack: u32,
    /// `received - rd`, in device milliseconds.
    pub round_trip: i64,
}

/// Linear map from device milliseconds to host epoch seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchors {
    /// Device times (ms).
    pub e0: f64,
    pub e1: f64,
    /// Host times (epoch seconds) at `e0` and `e1`.
    pub c0: f64,
    pub c1: f64,
    /// False when no sync point existed: `c0..c1` then only has the right *span*, not
    /// the right epoch.
    pub anchored: bool,
}

impl Anchors {
    /// Device milliseconds to host epoch seconds.
    pub fn to_local(&self, device_ms: f64) -> f64 {
        if self.e1 == self.e0 {
            return self.c0;
        }
        self.c0 + (device_ms - self.e0) * (self.c1 - self.c0) / (self.e1 - self.e0)
    }
}

/// Collects sync points from a packet stream and derives the clock mapping.
#[derive(Debug, Default)]
pub struct TimeSync {
    points: Vec<SyncPoint>,
    /// Request being tracked: (packet number, device time).
    open: Option<(u16, u32)>,
    received: Option<(u32, f64)>,
    /// Smallest and largest device timestamp seen.
    range: Option<(u32, u32)>,
}

impl TimeSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn points(&self) -> &[SyncPoint] {
        &self.points
    }

    /// Feed every packet from the device, in order. `parse_local` converts the host's
    /// `TL` payload string to epoch seconds.
    pub fn observe(&mut self, p: &Packet, parse_local: impl Fn(&str) -> Option<f64>) {
        self.range = Some(match self.range {
            None => (p.timestamp, p.timestamp),
            Some((lo, hi)) => (lo.min(p.timestamp), hi.max(p.timestamp)),
        });
        match p.tag.as_str() {
            tag::REQUEST_DATA if p.payload.iter().any(|e| e == tag::TIMESTAMP_LOCAL) => {
                self.open = Some((p.number, p.timestamp));
                self.received = None;
            }
            tag::TIMESTAMP_LOCAL if self.open.is_some() => {
                if let Some(sent) = p.payload.first().and_then(|s| parse_local(s)) {
                    self.received = Some((p.timestamp, sent));
                }
            }
            tag::ACK if self.open.is_some() => {
                let acked = p.payload.first().and_then(|s| s.trim().parse::<u16>().ok());
                if let (Some((n, rd)), Some((received, sent)), Some(a)) = (self.open, self.received, acked)
                    && n == a {
                        self.points.push(SyncPoint {
                            rd,
                            received,
                            sent_local: sent,
                            ack: p.timestamp,
                            round_trip: received as i64 - rd as i64,
                        });
                        self.open = None;
                        self.received = None;
                    }
            }
            _ => {}
        }
    }

    /// The device-to-host mapping from what has been observed so far.
    pub fn anchors(&self) -> Option<Anchors> {
        let (lo, hi) = self.range?;
        let (lo, hi) = (lo as f64, hi as f64);
        let usable: Vec<&SyncPoint> = self.points.iter().filter(|p| p.round_trip >= 0).collect();
        let local_at = |p: &SyncPoint| p.sent_local + p.round_trip as f64 / 2.0 / 1000.0;

        Some(match usable.len() {
            0 => Anchors { e0: lo, e1: hi, c0: 0.0, c1: (hi - lo) / 1000.0, anchored: false },
            1 => {
                let p = usable[0];
                let (e, c) = (p.received as f64, local_at(p));
                if e - lo >= hi - e {
                    Anchors { e0: lo, e1: e, c0: c - (e - lo) / 1000.0, c1: c, anchored: true }
                } else {
                    Anchors { e0: e, e1: hi, c0: c, c1: c + (hi - e) / 1000.0, anchored: true }
                }
            }
            n => {
                // Fastest round trip in the first and last quarter (half, if few points).
                let q = (n / 4).max(1).min(n / 2).max(1);
                let best = |slice: &[&SyncPoint]| -> SyncPoint {
                    (*slice.iter().min_by_key(|p| p.round_trip).expect("non-empty slice")).clone()
                };
                let first = best(&usable[..q]);
                let last = best(&usable[n - q..]);
                Anchors {
                    e0: first.received as f64,
                    e1: last.received as f64,
                    c0: local_at(&first),
                    c1: local_at(&last),
                    anchored: true,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Device clock = host epoch seconds * 1000 - 5_000_000 (device booted at a known time).
    fn exchange(ts: &mut TimeSync, n: u16, rd_dev: u32, rtt_ms: u32) {
        let host_epoch = |dev_ms: f64| (dev_ms + 5_000_000.0) / 1000.0 + 1.7e9;
        let recv_dev = rd_dev + rtt_ms;
        // The host sent its time halfway through the round trip.
        let sent = host_epoch(rd_dev as f64 + rtt_ms as f64 / 2.0);
        let parse = |s: &str| s.parse::<f64>().ok();
        ts.observe(&Packet::parse(&format!("{rd_dev},{n},2,RD,1,100,TL,TU")).unwrap(), parse);
        ts.observe(&Packet::parse(&format!("{recv_dev},{},1,TL,1,100,{sent}", n + 1)).unwrap(), parse);
        ts.observe(&Packet::parse(&format!("{},{},2,AK,1,100,{n},RD", recv_dev + 1, n + 2)).unwrap(), parse);
    }

    #[test]
    fn maps_device_time_to_host_time() {
        let mut ts = TimeSync::new();
        exchange(&mut ts, 1, 10_000, 40);
        exchange(&mut ts, 10, 400_000, 25);
        exchange(&mut ts, 20, 900_000, 90);
        assert_eq!(ts.points().len(), 3);
        let a = ts.anchors().unwrap();
        assert!(a.anchored);
        let truth = |dev: f64| (dev + 5_000_000.0) / 1000.0 + 1.7e9;
        for dev in [10_000.0, 500_000.0, 900_000.0] {
            assert!((a.to_local(dev) - truth(dev)).abs() < 0.01, "dev {dev}: {} vs {}", a.to_local(dev), truth(dev));
        }
    }

    #[test]
    fn single_point_still_anchors() {
        let mut ts = TimeSync::new();
        exchange(&mut ts, 1, 10_000, 30);
        // Packets bounding the recording.
        ts.observe(&Packet::parse("500000,9,1,AX,1,100,0").unwrap(), |_| None);
        let a = ts.anchors().unwrap();
        assert!(a.anchored);
        let truth = (300_000.0 + 5_000_000.0) / 1000.0 + 1.7e9;
        assert!((a.to_local(300_000.0) - truth).abs() < 0.01);
    }

    #[test]
    fn no_points_gives_unanchored_span() {
        let mut ts = TimeSync::new();
        ts.observe(&Packet::parse("1000,1,1,AX,1,100,0").unwrap(), |_| None);
        ts.observe(&Packet::parse("3000,2,1,AX,1,100,0").unwrap(), |_| None);
        let a = ts.anchors().unwrap();
        assert!(!a.anchored);
        assert_eq!(a.c1 - a.c0, 2.0);
    }

    #[test]
    fn ack_for_other_request_is_ignored() {
        let mut ts = TimeSync::new();
        let parse = |s: &str| s.parse::<f64>().ok();
        ts.observe(&Packet::parse("100,5,2,RD,1,100,TL,TU").unwrap(), parse);
        ts.observe(&Packet::parse("140,6,1,TL,1,100,1.7e9").unwrap(), parse);
        ts.observe(&Packet::parse("141,7,2,AK,1,100,99,RD").unwrap(), parse);
        assert!(ts.points().is_empty());
    }
}
