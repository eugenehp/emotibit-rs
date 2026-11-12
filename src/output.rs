//! Data outputs, like the Oscilloscope's "Send data via" menu: UDP, OSC and LSL.
//!
//! * **UDP** forwards every raw packet line to an address ([`spawn_udp_forward`]).
//! * **OSC** sends each sensor packet as one message of float arguments ([`OscOutput`]).
//! * **LSL** publishes one single-channel float stream per signal, named as in the
//!   official `lslOutputSettings.json` ([`LslOutput`]), and can take LSL marker streams in
//!   and forward them to the device ([`spawn_lsl_markers`]). Uses the pure-Rust `rlsl` crate.
//!
//! [`Outputs`] bundles the ones you enable; call [`Outputs::on_event`] for each event.

#[cfg(any(feature = "osc", feature = "lsl"))]
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
#[cfg(feature = "osc")]
use std::path::Path;

use anyhow::{Context, Result};
use log::warn;
#[cfg(feature = "lsl")]
use log::debug;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::emotibit_client::EmotiBitHandle;
use crate::types::EmotiBitEvent;
use crate::types::SensorReading;

// ── UDP forward ───────────────────────────────────────────────────────────────

/// Forward every raw packet from the device to `target`, one datagram per packet line.
pub fn spawn_udp_forward(handle: &EmotiBitHandle, target: SocketAddr) -> Result<JoinHandle<()>> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("cannot bind UDP output socket")?;
    socket.set_nonblocking(true)?;
    let mut rx = handle.subscribe_raw();
    Ok(tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(line) => {
                    let mut buf = line.as_bytes().to_vec();
                    buf.push(b'\n');
                    let _ = socket.send_to(&buf, target);
                }
                Err(RecvError::Lagged(n)) => warn!("UDP forward fell behind by {n} packets"),
                Err(RecvError::Closed) => break,
            }
        }
    }))
}

// ── OSC ───────────────────────────────────────────────────────────────────────

/// Sends each sensor packet as an OSC message of float arguments (one per sample).
///
/// Default address: `/<device id>/<channel name>`, e.g. `/MD-V6-0000356/ACC:X`. A map of
/// tag → addresses overrides this, like the Oscilloscope's OSC patchboard.
#[cfg(feature = "osc")]
pub struct OscOutput {
    socket: UdpSocket,
    target: SocketAddr,
    device: String,
    map: Option<HashMap<String, Vec<String>>>,
}

#[cfg(feature = "osc")]
impl OscOutput {
    pub fn new(target: SocketAddr, device: &str, map_file: Option<&Path>) -> Result<Self> {
        let map = match map_file {
            Some(p) => {
                let text = std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
                Some(serde_json::from_str::<HashMap<String, Vec<String>>>(&text)
                    .context("OSC map must be a JSON object of tag -> [addresses]")?)
            }
            None => None,
        };
        Ok(Self { socket: UdpSocket::bind("0.0.0.0:0")?, target, device: device.to_string(), map })
    }

    fn addresses(&self, r: &SensorReading) -> Vec<String> {
        match &self.map {
            Some(m) => m.get(r.channel.tag).cloned().unwrap_or_default(),
            None => vec![format!("/{}/{}", self.device, r.channel.name)],
        }
    }

    /// Encode the OSC messages for a reading (exposed for testing).
    pub fn messages(&self, r: &SensorReading) -> Vec<Vec<u8>> {
        use rosc::{encoder, OscMessage, OscPacket, OscType};
        self.addresses(r)
            .into_iter()
            .filter_map(|addr| {
                let args = r.samples.iter().map(|v| OscType::Float(*v as f32)).collect();
                encoder::encode(&OscPacket::Message(OscMessage { addr, args })).ok()
            })
            .collect()
    }

    pub fn send(&self, r: &SensorReading) {
        for m in self.messages(r) {
            let _ = self.socket.send_to(&m, self.target);
        }
    }
}

// ── LSL ───────────────────────────────────────────────────────────────────────

/// One published stream, from the official `lslOutputSettings.json`.
#[cfg(feature = "lsl")]
const LSL_CHANNELS: &[(&str, &str, &str, f64)] = &[
    // (EmotiBit tag, stream name, stream type, nominal rate Hz; 0 = irregular)
    ("PR", "PPG_RED", "PPGRed", 25.0),
    ("PI", "PPG_IR", "PPGInfrared", 25.0),
    ("PG", "PPG_GRN", "PPGGreen", 25.0),
    ("HR", "HR", "HeartRate", 0.0),
    ("EA", "EDA", "EDA", 15.0),
    ("SA", "SCR_AMP", "SCRAmplitude", 0.0),
    ("SR", "SCR_RIS", "SCRRiseTime", 0.0),
    ("SF", "SCR_FREQ", "SCRFrequency", 0.0),
    ("T1", "TEMP1", "Temperature", 7.5),
    ("TH", "THERM", "Thermopile", 7.5),
    ("AX", "ACC_X", "AccelerometerX", 25.0),
    ("AY", "ACC_Y", "AccelerometerY", 25.0),
    ("AZ", "ACC_Z", "AccelerometerZ", 25.0),
    ("GX", "GYRO_X", "GyroscopeX", 25.0),
    ("GY", "GYRO_Y", "GyroscopeY", 25.0),
    ("GZ", "GYRO_Z", "GyroscopeZ", 25.0),
    ("MX", "MAG_X", "MagnetometerX", 25.0),
    ("MY", "MAG_Y", "MagnetometerY", 25.0),
    ("MZ", "MAG_Z", "MagnetometerZ", 25.0),
];

/// Publishes the device's signals as LSL streams (source id = the device id).
#[cfg(feature = "lsl")]
pub struct LslOutput {
    outlets: HashMap<&'static str, rlsl::outlet::StreamOutlet>,
    last_device_ts: HashMap<&'static str, u32>,
}

#[cfg(feature = "lsl")]
impl LslOutput {
    /// Create the outlets. Safe to call from async code: `rlsl` drives its own tokio runtime
    /// internally and panics if asked to start it from inside another one, so the outlets are
    /// built on a plain OS thread.
    pub fn new(device_id: &str) -> Self {
        let id = device_id.to_string();
        std::thread::spawn(move || Self::build(&id)).join().expect("LSL outlet thread panicked")
    }

    fn build(device_id: &str) -> Self {
        use rlsl::prelude::*;
        let outlets = LSL_CHANNELS
            .iter()
            .map(|(tag, name, ty, rate)| {
                let info = StreamInfo::new(name, ty, 1, *rate, ChannelFormat::Float32, device_id);
                (*tag, StreamOutlet::new(&info, 0, 360))
            })
            .collect();
        Self { outlets, last_device_ts: HashMap::new() }
    }

    /// The outlet for an EmotiBit tag, if that tag is published.
    pub fn outlet(&self, tag: &str) -> Option<&rlsl::outlet::StreamOutlet> {
        self.outlets.get(tag)
    }

    /// Push every sample of a reading. The last sample is stamped "now" on the LSL clock and
    /// earlier ones are spaced back by the device's own packet interval.
    pub fn push(&mut self, r: &SensorReading) {
        let Some(outlet) = self.outlets.get(r.channel.tag) else { return };
        let n = r.samples.len();
        if n == 0 {
            return;
        }
        let prev = self.last_device_ts.insert(r.channel.tag, r.device_timestamp_ms);
        let step_s = match prev {
            Some(p) if r.device_timestamp_ms > p => (r.device_timestamp_ms - p) as f64 / 1000.0 / n as f64,
            _ => 0.0,
        };
        let now = rlsl::clock::local_clock();
        for (i, v) in r.samples.iter().enumerate() {
            let age = (n - 1 - i) as f64 * step_s;
            outlet.push_sample_f(&[*v as f32], now - age, true);
        }
    }
}

/// Take an LSL marker stream in and forward each marker to the device, like the Oscilloscope.
///
/// Every marker becomes three control packets: `LM` (the marker with its timestamps), and two
/// `TX` cross-time packets that let the DataParser relate the LSL clock to the device clock.
/// Waits for the stream to appear; ends when the device disconnects.
#[cfg(feature = "lsl")]
pub fn spawn_lsl_markers(handle: &EmotiBitHandle, name: String, source_id: Option<String>) -> JoinHandle<()> {
    use crate::emotibit_client::local_timestamp;
    use crate::protocol::tag;
    use rlsl::prelude::*;

    let handle = handle.clone();
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let found = loop {
            if !handle.is_connected() {
                return;
            }
            let streams = rlsl::resolver::resolve_by_property("name", &name, 1, 2.0);
            let hit = streams
                .into_iter()
                .find(|s| source_id.as_deref().is_none_or(|id| s.source_id() == id));
            if let Some(s) = hit {
                break s;
            }
        };
        let inlet = StreamInlet::new(&found, 360, 0, true);
        if inlet.open_stream(10.0).is_err() {
            warn!("could not open LSL marker stream '{name}'");
            return;
        }
        while handle.is_connected() {
            let Ok((values, ts)) = inlet.pull_sample_str(1.0) else { continue };
            if ts == 0.0 {
                continue; // timeout, no sample
            }
            let corr = inlet.time_correction(1.0);
            let lsl_now = local_clock();
            let f = |x: f64| format!("{x:.7}");

            let mut lm = vec!["LR".into(), f(ts + corr), "LM".into(), f(ts), "LC".into(), f(lsl_now), "LD".into()];
            lm.extend(values);
            let tx1 = vec!["LC".into(), f(ts + corr), "LM".into(), f(ts)];
            let tx2 = vec!["TL".into(), local_timestamp(), "LC".into(), f(local_clock())];
            for (t, payload) in [(tag::LSL_MARKER, lm), (tag::TIMESTAMP_CROSS_TIME, tx1), (tag::TIMESTAMP_CROSS_TIME, tx2)] {
                // The TCP control channel comes up about a second after the handshake; a marker
                // that arrives before then waits for it rather than being lost.
                let mut sent = false;
                for _ in 0..40 {
                    if rt.block_on(handle.send_control(t, &payload)).is_ok() {
                        sent = true;
                        break;
                    }
                    if !handle.is_connected() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                if !sent {
                    if handle.is_connected() {
                        warn!("control channel never came up; marker dropped");
                    } else {
                        debug!("connection closed; marker not forwarded");
                    }
                }
            }
        }
    })
}

// ── Bundle ────────────────────────────────────────────────────────────────────

/// What to enable.
#[derive(Debug, Clone, Default)]
pub struct OutputConfig {
    pub udp: Option<SocketAddr>,
    pub osc: Option<SocketAddr>,
    pub osc_map: Option<std::path::PathBuf>,
    pub lsl: bool,
    /// LSL marker stream to read: `(name, optional source id)`.
    pub lsl_marker: Option<(String, Option<String>)>,
}

/// The outputs that are switched on for one device.
pub struct Outputs {
    #[cfg(feature = "osc")]
    osc: Option<OscOutput>,
    #[cfg(feature = "lsl")]
    lsl: Option<LslOutput>,
    tasks: Vec<JoinHandle<()>>,
}

impl Outputs {
    pub fn start(config: &OutputConfig, handle: &EmotiBitHandle) -> Result<Self> {
        #[cfg(any(feature = "osc", feature = "lsl"))]
        let device = handle.device().id.clone();
        let mut tasks = Vec::new();
        if let Some(addr) = config.udp {
            tasks.push(spawn_udp_forward(handle, addr)?);
        }
        #[cfg(not(feature = "osc"))]
        if config.osc.is_some() {
            anyhow::bail!("built without the `osc` feature");
        }
        #[cfg(not(feature = "lsl"))]
        if config.lsl || config.lsl_marker.is_some() {
            anyhow::bail!("built without the `lsl` feature");
        }
        #[cfg(feature = "lsl")]
        if let Some((name, id)) = config.lsl_marker.clone() {
            tasks.push(spawn_lsl_markers(handle, name, id));
        }
        Ok(Self {
            #[cfg(feature = "osc")]
            osc: config.osc.map(|a| OscOutput::new(a, &device, config.osc_map.as_deref())).transpose()?,
            #[cfg(feature = "lsl")]
            lsl: config.lsl.then(|| LslOutput::new(&device)),
            tasks,
        })
    }

    /// Outputs for a source without a WiFi handle (BLE): OSC and LSL only.
    pub fn for_device(config: &OutputConfig, device: &str) -> Result<Self> {
        if config.udp.is_some() || config.lsl_marker.is_some() {
            anyhow::bail!("--udp and --lsl-marker need the WiFi connection; BLE supports --osc and --lsl");
        }
        #[cfg(not(feature = "osc"))]
        if config.osc.is_some() {
            anyhow::bail!("built without the `osc` feature");
        }
        #[cfg(not(feature = "lsl"))]
        if config.lsl {
            anyhow::bail!("built without the `lsl` feature");
        }
        #[cfg(not(any(feature = "osc", feature = "lsl")))]
        let _ = device;
        Ok(Self {
            #[cfg(feature = "osc")]
            osc: config.osc.map(|a| OscOutput::new(a, device, config.osc_map.as_deref())).transpose()?,
            #[cfg(feature = "lsl")]
            lsl: config.lsl.then(|| LslOutput::new(device)),
            tasks: Vec::new(),
        })
    }

    /// Feed a reading directly (sources that do not produce `EmotiBitEvent`s).
    pub fn on_reading(&mut self, r: &SensorReading) {
        self.on_event(&EmotiBitEvent::Sensor(r.clone()));
    }

    /// Feed an event; only sensor readings are published.
    pub fn on_event(&mut self, event: &EmotiBitEvent) {
        if let EmotiBitEvent::Sensor(r) = event {
            #[cfg(feature = "osc")]
            if let Some(o) = &self.osc {
                o.send(r);
            }
            #[cfg(feature = "lsl")]
            if let Some(l) = &mut self.lsl {
                l.push(r);
            }
            let _ = r;
        }
    }

    pub fn stop(self) {
        for t in self.tasks {
            t.abort();
        }
    }
}

// Tests of the OSC and LSL outputs; without either there is nothing to test here.
#[cfg(all(test, any(feature = "osc", feature = "lsl")))]
mod tests {
    use super::*;
    use crate::types::channel;

    fn reading() -> SensorReading {
        SensorReading {
            channel: channel("AX").unwrap(),
            packet_number: 1,
            device_timestamp_ms: 100,
            received_at_ms: 0.0,
            samples: vec![0.5, -0.25],
        }
    }

    #[cfg(feature = "osc")]
    #[test]
    fn osc_message_has_address_and_float_args() {
        let out = OscOutput::new("127.0.0.1:9000".parse().unwrap(), "MD-V6-1", None).unwrap();
        let msgs = out.messages(&reading());
        assert_eq!(msgs.len(), 1);
        let (_, packet) = rosc::decoder::decode_udp(&msgs[0]).unwrap();
        match packet {
            rosc::OscPacket::Message(m) => {
                assert_eq!(m.addr, "/MD-V6-1/ACC:X");
                assert_eq!(m.args, vec![rosc::OscType::Float(0.5), rosc::OscType::Float(-0.25)]);
            }
            _ => panic!("expected a message"),
        }
    }

    #[cfg(feature = "osc")]
    #[test]
    fn osc_map_overrides_addresses() {
        let dir = std::env::temp_dir().join(format!("emotibit-osc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("map.json");
        std::fs::write(&f, r#"{"AX": ["/a/x", "/b/x"]}"#).unwrap();
        let out = OscOutput::new("127.0.0.1:9000".parse().unwrap(), "D", Some(&f)).unwrap();
        assert_eq!(out.messages(&reading()).len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "lsl")]
    #[tokio::test(flavor = "multi_thread")]
    async fn lsl_outlets_can_be_created_inside_a_runtime() {
        // Regression: rlsl panicked with "Cannot start a runtime from within a runtime".
        let out = LslOutput::new("RT-TEST");
        assert!(out.outlet("PG").is_some());
    }

    #[cfg(feature = "lsl")]
    #[test]
    fn lsl_outlet_delivers_samples_to_an_inlet() {
        use rlsl::prelude::*;
        let mut out = LslOutput::new("TEST-DEVICE");
        let outlet = out.outlet("AX").expect("AX is published");
        assert_eq!(outlet.info().name(), "ACC_X");
        assert_eq!(outlet.info().source_id(), "TEST-DEVICE");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let inlet = StreamInlet::new(outlet.info(), 360, 0, true);
        inlet.open_stream(10.0).unwrap();
        out.push(&reading());
        let mut buf = [0.0f32; 1];
        inlet.pull_sample_f(&mut buf, 5.0).unwrap();
        assert_eq!(buf[0], 0.5);
        inlet.pull_sample_f(&mut buf, 5.0).unwrap();
        assert_eq!(buf[0], -0.25);
    }
}
