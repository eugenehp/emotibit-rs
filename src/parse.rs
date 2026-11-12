//! Turn decoded [`Packet`]s into typed events.

use crate::protocol::Packet;
use crate::types::{channel, EmotiBitEvent, SensorReading};

/// Milliseconds since the Unix epoch. Native only: a browser has no `SystemTime`, so
/// the page supplies its own clock (`Date.now()`) to the wasm API.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs_f64()
        * 1000.0
}

/// Classify a packet received from the device.
pub fn to_event(packet: Packet, received_at_ms: f64) -> EmotiBitEvent {
    match channel(&packet.tag) {
        Some(channel) => {
            let samples = packet.values();
            EmotiBitEvent::Sensor(SensorReading {
                channel,
                packet_number: packet.number,
                device_timestamp_ms: packet.timestamp,
                received_at_ms,
                samples,
            })
        }
        None => EmotiBitEvent::Message(packet),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensor_packet_becomes_reading() {
        let p = Packet::parse("100,3,2,EA,1,100,0.031,0.032").unwrap();
        match to_event(p, 5.0) {
            EmotiBitEvent::Sensor(r) => {
                assert_eq!(r.channel.name, "EDA");
                assert_eq!(r.samples, vec![0.031, 0.032]);
                assert_eq!(r.device_timestamp_ms, 100);
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn unknown_tag_is_a_message() {
        let p = Packet::parse("1,1,0,DO,1,100").unwrap();
        assert!(matches!(to_event(p, 0.0), EmotiBitEvent::Message(_)));
    }
}
