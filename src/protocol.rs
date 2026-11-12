//! EmotiBit wire format and protocol constants.
//!
//! Everything EmotiBit sends or receives is a newline-terminated CSV *packet*:
//!
//! ```text
//! timestamp,packetNumber,dataLength,TAG,protocolVersion,dataReliability[,payload...]
//! 183422,41,3,PG,1,100,2412,2418,2409
//! ```
//!
//! `dataLength` is the number of payload elements. Several packets may share one
//! UDP datagram. The device timestamp is milliseconds since the device booted.
//!
//! The connection model (taken from the reference host, `EmotiBitWiFiHost`):
//!
//! 1. **Discovery** — the host sends `HE` (HELLO_EMOTIBIT) to UDP port
//!    [`ADVERTISING_PORT`], by broadcast and by unicast sweep. Each device
//!    answers `HH` (HELLO_HOST) with its id (`DI`) and the data port of the host
//!    it is connected to (`DP`), or [`EMOTIBIT_AVAILABLE`] if it is free.
//! 2. **Connect** — the host binds a UDP *data* port and a TCP *control* port and
//!    repeats `EC` (EMOTIBIT_CONNECT) with `CP,<control>,DP,<data>` until the
//!    device answers `PN`/`PO` (PONG) naming the data port.
//! 3. **Stream** — the device opens a TCP connection to the control port and
//!    sends sensor packets as UDP datagrams to the data port. The host sends `PN`
//!    (PING) every 500 ms; the device must answer within 10 s or the link drops.

use std::fmt;

/// UDP port every EmotiBit listens on for discovery and connection requests.
pub const ADVERTISING_PORT: u16 = 3131;

/// Value of the `DP` field in a `HH` reply when the device has no host.
pub const EMOTIBIT_AVAILABLE: i32 = -1;

/// Number of fixed header fields before the payload.
pub const HEADER_LEN: usize = 6;

/// Packet terminator.
pub const PACKET_DELIMITER: char = '\n';

/// Protocol version this client speaks.
pub const PROTOCOL_VERSION: u8 = 1;

/// Milliseconds between `PN` pings once connected.
pub const PING_INTERVAL_MS: u64 = 500;
/// Milliseconds between `EC` connect requests while connecting.
pub const CONNECT_INTERVAL_MS: u64 = 100;
/// Give up connecting after this long.
pub const CONNECT_TIMEOUT_MS: u64 = 5_000;
/// Drop the connection if no PONG arrives within this long.
pub const CONNECTION_TIMEOUT_MS: u64 = 10_000;
/// A device not heard from for this long is no longer "available".
pub const AVAILABILITY_TIMEOUT_MS: u64 = 5_000;

/// Packet type tags (two characters).
pub mod tag {
    // Sensors
    pub const EDA: &str = "EA";
    pub const EDL: &str = "EL";
    pub const EDR: &str = "ER";
    pub const PPG_INFRARED: &str = "PI";
    pub const PPG_RED: &str = "PR";
    pub const PPG_GREEN: &str = "PG";
    pub const SPO2: &str = "O2";
    pub const TEMPERATURE_0: &str = "T0";
    pub const TEMPERATURE_1: &str = "T1";
    pub const THERMOPILE: &str = "TH";
    pub const HUMIDITY_0: &str = "H0";
    pub const ACCELEROMETER_X: &str = "AX";
    pub const ACCELEROMETER_Y: &str = "AY";
    pub const ACCELEROMETER_Z: &str = "AZ";
    pub const GYROSCOPE_X: &str = "GX";
    pub const GYROSCOPE_Y: &str = "GY";
    pub const GYROSCOPE_Z: &str = "GZ";
    pub const MAGNETOMETER_X: &str = "MX";
    pub const MAGNETOMETER_Y: &str = "MY";
    pub const MAGNETOMETER_Z: &str = "MZ";
    // Derived metrics
    pub const HEART_RATE: &str = "HR";
    pub const INTER_BEAT_INTERVAL: &str = "BI";
    pub const SCR_AMPLITUDE: &str = "SA";
    pub const SCR_FREQUENCY: &str = "SF";
    pub const SCR_RISE_TIME: &str = "SR";
    // Device status
    pub const BATTERY_VOLTAGE: &str = "BV";
    pub const BATTERY_PERCENT: &str = "B%";
    pub const BUTTON_PRESS_SHORT: &str = "BS";
    pub const BUTTON_PRESS_LONG: &str = "BL";
    pub const DATA_CLIPPING: &str = "DC";
    pub const DATA_OVERFLOW: &str = "DO";
    pub const SD_CARD_PERCENT: &str = "SD";
    pub const RESET: &str = "RS";
    pub const DEBUG: &str = "DB";
    pub const EMOTIBIT_INFO: &str = "EI";
    pub const EMOTIBIT_MODE: &str = "EM";
    // Time
    pub const TIMESTAMP_EMOTIBIT: &str = "TE";
    pub const TIMESTAMP_LOCAL: &str = "TL";
    pub const TIMESTAMP_UTC: &str = "TU";
    pub const TIMESTAMP_CROSS_TIME: &str = "TX";
    // Host -> device
    pub const RECORD_BEGIN: &str = "RB";
    pub const RECORD_END: &str = "RE";
    pub const USER_NOTE: &str = "UN";
    pub const LSL_MARKER: &str = "LM";
    pub const MODE_NORMAL_POWER: &str = "MN";
    pub const MODE_LOW_POWER: &str = "ML";
    pub const MODE_MAX_LOW_POWER: &str = "MM";
    pub const MODE_WIRELESS_OFF: &str = "MO";
    pub const MODE_HIBERNATE: &str = "MH";
    pub const SERIAL_DATA_ON: &str = "S+";
    pub const SERIAL_DATA_OFF: &str = "S-";
    // Connection
    pub const HELLO_EMOTIBIT: &str = "HE";
    pub const HELLO_HOST: &str = "HH";
    pub const EMOTIBIT_CONNECT: &str = "EC";
    pub const EMOTIBIT_DISCONNECT: &str = "ED";
    pub const ACK: &str = "AK";
    pub const NACK: &str = "NK";
    pub const REQUEST_DATA: &str = "RD";
    pub const PING: &str = "PN";
    pub const PONG: &str = "PO";
}

/// Payload keys used inside connection packets.
pub mod label {
    pub const CONTROL_PORT: &str = "CP";
    pub const DATA_PORT: &str = "DP";
    pub const DEVICE_ID: &str = "DI";
}

/// Why a line was not a packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Fewer than [`HEADER_LEN`] comma-separated fields.
    TooShort(usize),
    /// A numeric header field did not parse.
    BadField(&'static str),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::TooShort(n) => write!(f, "packet has {n} fields, need at least {HEADER_LEN}"),
            ParseError::BadField(name) => write!(f, "malformed header field `{name}`"),
        }
    }
}

impl std::error::Error for ParseError {}

/// One decoded packet.
#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    /// Sender's clock in milliseconds (device: since boot).
    pub timestamp: u32,
    /// Sender's packet counter for this tag family.
    pub number: u16,
    /// Declared number of payload elements.
    pub data_length: u16,
    pub tag: String,
    pub protocol_version: u8,
    /// Percentage, 100 = reliable.
    pub reliability: u8,
    pub payload: Vec<String>,
}

impl Packet {
    /// Parse a single packet (without its trailing newline).
    pub fn parse(line: &str) -> Result<Packet, ParseError> {
        let fields: Vec<&str> = line.trim_end_matches(['\r', '\n']).split(',').collect();
        if fields.len() < HEADER_LEN {
            return Err(ParseError::TooShort(fields.len()));
        }
        let timestamp = fields[0].trim().parse().map_err(|_| ParseError::BadField("timestamp"))?;
        let number = fields[1].trim().parse().map_err(|_| ParseError::BadField("packetNumber"))?;
        let data_length = fields[2].trim().parse().map_err(|_| ParseError::BadField("dataLength"))?;
        let protocol_version = fields[4].trim().parse().map_err(|_| ParseError::BadField("protocolVersion"))?;
        let reliability = fields[5].trim().parse().map_err(|_| ParseError::BadField("dataReliability"))?;

        let mut payload: Vec<String> = fields[HEADER_LEN..].iter().map(|s| s.to_string()).collect();
        // A packet written as `header,\n` carries one empty trailing element.
        if payload.last().is_some_and(|s| s.is_empty()) {
            payload.pop();
        }
        Ok(Packet {
            timestamp,
            number,
            data_length,
            tag: fields[3].to_string(),
            protocol_version,
            reliability,
            payload,
        })
    }

    /// The element following `key` in the payload, if `key` is present.
    ///
    /// Connection packets are `key,value` pairs: `CP,3133,DP,3132,DI,MD-V6-...`.
    pub fn keyed(&self, key: &str) -> Option<&str> {
        let i = self.payload.iter().position(|e| e == key)?;
        self.payload.get(i + 1).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// Re-serialize as one line (no newline): exactly the format the device writes to its
    /// SD card and sends over UDP, so a stream of these is a valid raw recording.
    pub fn to_line(&self) -> String {
        let mut s = format!(
            "{},{},{},{},{},{}",
            self.timestamp, self.number, self.data_length, self.tag, self.protocol_version, self.reliability
        );
        for p in &self.payload {
            s.push(',');
            s.push_str(p);
        }
        s
    }

    /// Payload elements parsed as numbers; elements that are not numbers are skipped.
    pub fn values(&self) -> Vec<f64> {
        self.payload.iter().filter_map(|s| s.trim().parse().ok()).collect()
    }
}

/// Split a datagram (or TCP chunk) into packets. Malformed lines are returned as errors
/// rather than dropped silently, so callers can count them.
pub fn split_message(message: &str) -> Vec<Result<Packet, ParseError>> {
    message
        .split(PACKET_DELIMITER)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(Packet::parse)
        .collect()
}

/// Encode a packet for sending: header, one `,element` per payload element, newline.
pub fn encode(tag: &str, number: u16, timestamp_ms: u32, payload: &[String]) -> String {
    let mut s = format!(
        "{timestamp_ms},{number},{},{tag},{PROTOCOL_VERSION},100",
        payload.len()
    );
    for p in payload {
        s.push(',');
        s.push_str(p);
    }
    s.push(PACKET_DELIMITER);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sensor_packet() {
        let p = Packet::parse("183422,41,3,PG,1,100,2412,2418,2409").unwrap();
        assert_eq!(p.timestamp, 183422);
        assert_eq!(p.number, 41);
        assert_eq!(p.data_length, 3);
        assert_eq!(p.tag, "PG");
        assert_eq!(p.values(), vec![2412.0, 2418.0, 2409.0]);
    }

    #[test]
    fn parses_hello_host_keyed_values() {
        let p = Packet::parse("5,1,6,HH,1,100,CP,3133,DP,-1,DI,MD-V6-0000356").unwrap();
        assert_eq!(p.keyed(label::DATA_PORT), Some("-1"));
        assert_eq!(p.keyed(label::DEVICE_ID), Some("MD-V6-0000356"));
        assert_eq!(p.keyed("XX"), None);
    }

    #[test]
    fn trailing_comma_is_not_a_payload_element() {
        let p = Packet::parse("1,2,0,HE,1,100,").unwrap();
        assert!(p.payload.is_empty());
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(Packet::parse("1,2,3"), Err(ParseError::TooShort(3)));
        assert_eq!(Packet::parse("x,2,0,HE,1,100"), Err(ParseError::BadField("timestamp")));
    }

    #[test]
    fn splits_multi_packet_datagram() {
        let msg = "1,1,1,AX,1,100,0.5\n2,2,2,AY,1,100,0.1,0.2\nbad\n";
        let r = split_message(msg);
        assert_eq!(r.len(), 3);
        assert!(r[0].is_ok() && r[1].is_ok() && r[2].is_err());
    }

    #[test]
    fn to_line_roundtrips() {
        let line = "183422,41,3,PG,1,100,2412,2418,2409";
        assert_eq!(Packet::parse(line).unwrap().to_line(), line);
    }

    #[test]
    fn encode_roundtrips() {
        let wire = encode(tag::EMOTIBIT_CONNECT, 7, 1234, &["CP".into(), "3133".into(), "DP".into(), "3132".into()]);
        assert_eq!(wire, "1234,7,4,EC,1,100,CP,3133,DP,3132\n");
        let p = Packet::parse(wire.trim_end()).unwrap();
        assert_eq!(p.keyed(label::CONTROL_PORT), Some("3133"));
        assert_eq!(encode(tag::HELLO_EMOTIBIT, 0, 5, &[]), "5,0,0,HE,1,100\n");
    }
}
