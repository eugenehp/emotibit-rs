//! Reading signals over USB serial, no WiFi involved.
//!
//! The stock firmware has a *debug mode* that prints raw sensor values on its USB serial port.
//! It is a debugging aid, so it is limited: **one signal at a time, one bare number per line,
//! no timestamps, no batching.** It is useful for a quick look at a sensor from a plain static
//! web page (Web Serial) or the CLI, not as a replacement for the WiFi stream.
//!
//! How the firmware exposes it (from `EmotiBit.cpp`):
//!
//! **Privacy.** The firmware's boot log prints the WiFi SSID and password it read from
//! `config.txt`. [`SerialSession`] therefore never passes such lines on: any line mentioning
//! an SSID or a password is replaced by [`REDACTED`] before it reaches a caller.
//!
//! 1. At boot it prints `Enter C to enter WiFi config edit mode …` and waits **2 s** for a
//!    character. `D` enters debug mode and the firmware answers `ENTERING DEBUG MODE`.
//!    The board must therefore be *reset* first; an ESP32 Feather resets when DTR/RTS are
//!    toggled, and for any board you can press its Reset button.
//! 2. Setup then takes a while (sensor init and the WiFi connection, often 15-30 s), during
//!    which serial input is buffered, not acted on. The firmware prints a `DEBUG MODE` banner
//!    (`Enter ? to know more about available options in DEBUG MODE`) when its main loop starts;
//!    only then is `:` sent, once, **together with the signal key in the same write**: between
//!    `:` and the key the firmware's loop runs with a placeholder serial type, and on hardware
//!    a long gap there sometimes left it ignoring the key. `:` toggles serial data on (`_serialData = DataType::DEBUG`) or off
//!    (`_serialData OFF`).
//! 3. With serial data on, one key picks the signal: `E` EDA, `T` thermopile, `)` temp 0,
//!    `!` temp 1, `I` accelerometer X, `P` PPG red, `B` battery percent. (These keys also turn
//!    that sensor on.)
//!
//! [`SerialSession`] is a pure state machine: feed it the text received from the port, send the
//! bytes it asks for, and collect the values. It does no I/O, so the browser (wasm) and the CLI
//! drive the exact same logic.

use std::time::Duration;

/// A signal the firmware can print over serial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialSignal {
    Eda,
    Thermopile,
    Temp0,
    Temp1,
    AccelX,
    PpgRed,
    Battery,
}

impl SerialSignal {
    pub const ALL: [SerialSignal; 7] = [
        SerialSignal::Eda,
        SerialSignal::Thermopile,
        SerialSignal::Temp0,
        SerialSignal::Temp1,
        SerialSignal::AccelX,
        SerialSignal::PpgRed,
        SerialSignal::Battery,
    ];

    /// The key that selects this signal in debug mode.
    pub fn key(self) -> u8 {
        match self {
            SerialSignal::Eda => b'E',
            SerialSignal::Thermopile => b'T',
            SerialSignal::Temp0 => b')',
            SerialSignal::Temp1 => b'!',
            SerialSignal::AccelX => b'I',
            SerialSignal::PpgRed => b'P',
            SerialSignal::Battery => b'B',
        }
    }

    /// The EmotiBit packet tag of the same signal, so serial data can use the normal channel table.
    pub fn tag(self) -> &'static str {
        match self {
            SerialSignal::Eda => "EA",
            SerialSignal::Thermopile => "TH",
            SerialSignal::Temp0 => "T0",
            SerialSignal::Temp1 => "T1",
            SerialSignal::AccelX => "AX",
            SerialSignal::PpgRed => "PR",
            SerialSignal::Battery => "B%",
        }
    }

    /// Command-line / UI name.
    pub fn name(self) -> &'static str {
        match self {
            SerialSignal::Eda => "eda",
            SerialSignal::Thermopile => "thermopile",
            SerialSignal::Temp0 => "temp0",
            SerialSignal::Temp1 => "temp1",
            SerialSignal::AccelX => "accel-x",
            SerialSignal::PpgRed => "ppg-red",
            SerialSignal::Battery => "battery",
        }
    }

    pub fn from_name(n: &str) -> Option<Self> {
        let n = n.to_ascii_lowercase().replace('_', "-");
        Self::ALL.into_iter().find(|s| s.name() == n)
    }
}

/// Where the session is in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialState {
    /// Waiting for the boot prompt (reset the board).
    WaitingForBoot,
    /// Sent `D`, waiting for `ENTERING DEBUG MODE`.
    EnteringDebug,
    /// In debug mode; turning serial data on and selecting the signal.
    Starting,
    /// Numbers are arriving.
    Streaming,
}

/// What the session wants done, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Write these bytes to the port.
    Send(Vec<u8>),
    /// One value of the selected signal.
    Value(f64),
    /// Any other line the board printed (boot messages, acknowledgements).
    Log(String),
    /// The handshake advanced.
    State(SerialState),
}

/// Re-send the `:` / signal key if the firmware has not acknowledged within this long. Only
/// used once the firmware's main loop is running (see the module docs).
pub const RETRY_AFTER: Duration = Duration::from_millis(2500);

/// Shown instead of any board output that mentions an SSID or password.
pub const REDACTED: &str = "[hidden: may contain WiFi credentials]";

/// `line` as it may be shown: [`REDACTED`] if it mentions an SSID or password.
pub fn redact(line: &str) -> String {
    if sensitive(line) { REDACTED.to_string() } else { line.to_string() }
}

fn sensitive(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("ssid") || l.contains("pass")
}

/// Drives the handshake and extracts values. See the module docs.
#[derive(Debug)]
pub struct SerialSession {
    signal: SerialSignal,
    state: SerialState,
    buf: String,
    serial_data_on: bool,
    signal_sent: bool,
    /// The firmware's main loop has started, so it will act on what we send.
    loop_running: bool,
    /// Millisecond clock reading when we last sent something and are waiting for an answer.
    waiting_since: Option<f64>,
}

impl SerialSession {
    pub fn new(signal: SerialSignal) -> Self {
        Self {
            signal,
            state: SerialState::WaitingForBoot,
            buf: String::new(),
            serial_data_on: false,
            signal_sent: false,
            loop_running: false,
            waiting_since: None,
        }
    }

    pub fn state(&self) -> SerialState {
        self.state
    }

    pub fn signal(&self) -> SerialSignal {
        self.signal
    }

    /// Feed text received from the port. `now_ms` is any monotonic millisecond clock; it is only
    /// used to time retries (see [`SerialSession::tick`]).
    pub fn feed(&mut self, chunk: &str, now_ms: f64) -> Vec<Out> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(i) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=i).collect();
            self.line(line.trim(), now_ms, &mut out);
        }
        // A prompt can arrive without its newline yet; the boot prompt is only acted on as a line.
        if self.buf.len() > 4096 {
            self.buf.clear(); // binary garbage, e.g. a ROM boot message at another baud rate
        }
        out
    }

    /// Call periodically. Re-sends the pending command if the firmware has not answered.
    pub fn tick(&mut self, now_ms: f64) -> Vec<Out> {
        let mut out = Vec::new();
        if !self.loop_running {
            return out; // the board is still in setup; anything sent now would just queue up
        }
        let Some(since) = self.waiting_since else { return out };
        if now_ms - since < RETRY_AFTER.as_millis() as f64 {
            return out;
        }
        self.waiting_since = Some(now_ms);
        if self.state == SerialState::Starting {
            // Not acknowledged yet: send both again. Acknowledged but no number: just the key.
            out.push(Out::Send(if self.serial_data_on { vec![self.signal.key()] } else { self.start_bytes() }));
        }
        out
    }

    /// `:` (serial data on) then the signal key, as one write.
    fn start_bytes(&self) -> Vec<u8> {
        vec![b':', self.signal.key()]
    }

    fn set_state(&mut self, s: SerialState, out: &mut Vec<Out>) {
        if self.state != s {
            self.state = s;
            out.push(Out::State(s));
        }
    }

    fn line(&mut self, line: &str, now_ms: f64, out: &mut Vec<Out>) {
        if line.is_empty() {
            return;
        }
        // A bare number is a value, but only once the signal has been selected.
        if self.signal_sent
            && let Ok(v) = line.parse::<f64>()
                && v.is_finite() {
                    self.set_state(SerialState::Streaming, out);
                    out.push(Out::Value(v));
                    return;
                }
        out.push(Out::Log(redact(line)));

        // A fresh boot restarts the handshake, whatever state we were in.
        if line.contains("Enter C to enter WiFi config edit mode") {
            self.serial_data_on = false;
            self.signal_sent = false;
            self.loop_running = false;
            self.set_state(SerialState::EnteringDebug, out);
            self.waiting_since = Some(now_ms);
            out.push(Out::Send(b"D".to_vec()));
        } else if line.contains("ENTERING DEBUG MODE") {
            // Setup continues for a while; wait for the banner before sending anything more.
            self.set_state(SerialState::Starting, out);
            self.waiting_since = None;
        } else if line.contains("Enter ? to know more about available options in DEBUG MODE") {
            self.loop_running = true;
            self.signal_sent = true;
            self.waiting_since = Some(now_ms);
            out.push(Out::Send(self.start_bytes()));
        } else if line.contains("_serialData = DataType::DEBUG") {
            // Acknowledges the `:`; the key went with it, so there is nothing more to send.
            self.serial_data_on = true;
            self.waiting_since = Some(now_ms);
        } else if line.contains("_serialData OFF") {
            // It was already on from an earlier session, so our `:` turned it off: start again.
            self.serial_data_on = false;
            self.waiting_since = Some(now_ms);
            out.push(Out::Send(self.start_bytes()));
        }
    }
}

/// Turns the serial value stream into a clean series for plotting.
///
/// The firmware re-prints its recent sample buffer on every loop pass (about 1,300 lines a
/// second for EDA, which really samples at 15 Hz), so the raw values repeat and overlap. The
/// binner averages everything received in each `bin_ms` window into one sample, stamped at the
/// time the window closed.
#[derive(Debug)]
pub struct SerialBinner {
    signal: SerialSignal,
    bin_ms: f64,
    sum: f64,
    n: u32,
    started: Option<f64>,
    seq: u16,
}

impl SerialBinner {
    pub fn new(signal: SerialSignal, bin_ms: f64) -> Self {
        Self { signal, bin_ms, sum: 0.0, n: 0, started: None, seq: 0 }
    }

    /// Add a value received at `now_ms`; returns a reading when a bin closes.
    pub fn push(&mut self, value: f64, now_ms: f64) -> Option<crate::types::SensorReading> {
        let start = *self.started.get_or_insert(now_ms);
        self.sum += value;
        self.n += 1;
        if now_ms - start < self.bin_ms {
            return None;
        }
        let mean = self.sum / self.n as f64;
        self.sum = 0.0;
        self.n = 0;
        self.started = Some(now_ms);
        self.seq = self.seq.wrapping_add(1);
        Some(crate::types::SensorReading {
            channel: crate::types::channel(self.signal.tag())?,
            packet_number: self.seq,
            // No device clock over serial: use the host clock so packet-to-packet spacing is right.
            device_timestamp_ms: (now_ms as u64 % u32::MAX as u64) as u32,
            received_at_ms: now_ms,
            samples: vec![mean],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binner_averages_repeated_values_into_one_sample_per_window() {
        let mut b = SerialBinner::new(SerialSignal::Eda, 50.0);
        let mut out = Vec::new();
        // 1,300 values/s of a slowly rising signal, with each value repeated 3 times.
        for i in 0..200u32 {
            let t = i as f64 * 0.77; // ms
            let v = 0.05 + (i / 3) as f64 * 1e-5;
            if let Some(r) = b.push(v, t) {
                out.push(r);
            }
        }
        assert!(out.len() >= 2 && out.len() <= 4, "got {}", out.len());
        assert!(out.iter().all(|r| r.samples.len() == 1 && r.channel.tag == "EA"));
        assert!(out.windows(2).all(|w| w[1].received_at_ms > w[0].received_at_ms));
        assert!(out[1].samples[0] > out[0].samples[0]);
    }

    fn sends(out: &[Out]) -> Vec<Vec<u8>> {
        out.iter().filter_map(|o| if let Out::Send(b) = o { Some(b.clone()) } else { None }).collect()
    }
    fn values(out: &[Out]) -> Vec<f64> {
        out.iter().filter_map(|o| if let Out::Value(v) = o { Some(*v) } else { None }).collect()
    }

    #[test]
    fn full_handshake_then_values() {
        let mut s = SerialSession::new(SerialSignal::Eda);
        // Boot chatter with a numeric line that must NOT be mistaken for data.
        let o = s.feed("ets Jun  8 2016\r\nhwVersion: 4\r\n4\r\n", 0.0);
        assert!(values(&o).is_empty());
        assert_eq!(s.state(), SerialState::WaitingForBoot);

        let o = s.feed("Enter C to enter WiFi config edit mode (Add/ Delete WiFi creds)\r\n", 10.0);
        assert_eq!(sends(&o), vec![b"D".to_vec()]);
        assert_eq!(s.state(), SerialState::EnteringDebug);

        let o = s.feed("\nENTERING DEBUG MODE\n\n", 20.0);
        assert!(sends(&o).is_empty(), "must not send while the board is still in setup");
        assert_eq!(s.state(), SerialState::Starting);
        let o = s.feed("Sensor setup:\nEmotiBit Setup complete\nEnter ? to know more about available options in DEBUG MODE\n", 25.0);
        assert_eq!(sends(&o), vec![b":E".to_vec()], "':' and the key go out together");

        let o = s.feed("_serialData = DataType::DEBUG\n", 40.0);
        assert!(sends(&o).is_empty());

        let o = s.feed("0.044719\n0.04472\n-0.5\n", 50.0);
        assert_eq!(values(&o), vec![0.044719, 0.04472, -0.5]);
        assert_eq!(s.state(), SerialState::Streaming);
    }

    #[test]
    fn handles_split_chunks_and_crlf() {
        let mut s = SerialSession::new(SerialSignal::Battery);
        s.feed("Enter C to enter WiFi config ", 0.0);
        let o = s.feed("edit mode (x)\r\n", 1.0);
        assert_eq!(sends(&o), vec![b"D".to_vec()]);
        s.feed("ENTERING DEBUG MODE\r\nEnter ? to know more about available options in DEBUG MODE\r\n_serialData = DataType::DEBUG\r\n", 2.0);
        let o = s.feed("10", 3.0);
        assert!(values(&o).is_empty()); // no newline yet
        let o = s.feed("0\r\n99.5\r\n", 4.0);
        assert_eq!(values(&o), vec![100.0, 99.5]);
    }

    #[test]
    fn serial_data_already_on_is_toggled_back() {
        let mut s = SerialSession::new(SerialSignal::Eda);
        s.feed("Enter C to enter WiFi config edit mode\nENTERING DEBUG MODE\nEnter ? to know more about available options in DEBUG MODE\n", 0.0);
        let o = s.feed("_serialData OFF\n", 1.0);
        assert_eq!(sends(&o), vec![b":E".to_vec()]);
        let o = s.feed("_serialData = DataType::DEBUG\n", 2.0);
        assert!(sends(&o).is_empty());
    }

    #[test]
    fn a_reboot_restarts_the_handshake() {
        let mut s = SerialSession::new(SerialSignal::Eda);
        s.feed("Enter C to enter WiFi config edit mode\nENTERING DEBUG MODE\nEnter ? to know more about available options in DEBUG MODE\n_serialData = DataType::DEBUG\n1.5\n", 0.0);
        assert_eq!(s.state(), SerialState::Streaming);
        let o = s.feed("Enter C to enter WiFi config edit mode\n", 100.0);
        assert_eq!(sends(&o), vec![b"D".to_vec()]);
        assert_eq!(s.state(), SerialState::EnteringDebug);
        // Numbers are logs again until the signal is re-selected.
        assert!(values(&s.feed("7\n", 101.0)).is_empty());
    }

    #[test]
    fn retries_when_the_firmware_is_silent() {
        let mut s = SerialSession::new(SerialSignal::Eda);
        s.feed("Enter C to enter WiFi config edit mode\nENTERING DEBUG MODE\n", 0.0);
        // Still in setup: never retry, however long it takes (this is what flipped the toggle on hardware).
        assert!(sends(&s.tick(30_000.0)).is_empty());
        s.feed("Enter ? to know more about available options in DEBUG MODE\n", 31_000.0);
        assert!(sends(&s.tick(32_000.0)).is_empty()); // too soon
        assert_eq!(sends(&s.tick(33_600.0)), vec![b":E".to_vec()]); // no acknowledgement: send both again
        s.feed("_serialData = DataType::DEBUG\n", 33_700.0);
        assert_eq!(sends(&s.tick(36_300.0)), vec![b"E".to_vec()]); // no numbers yet: ask again
    }

    #[test]
    fn signal_keys_match_the_firmware() {
        let keys: Vec<(&str, u8)> = SerialSignal::ALL.iter().map(|s| (s.name(), s.key())).collect();
        assert!(keys.contains(&("eda", b'E')));
        assert!(keys.contains(&("temp0", b')')));
        assert!(keys.contains(&("temp1", b'!')));
        assert_eq!(SerialSignal::from_name("PPG_RED"), Some(SerialSignal::PpgRed));
        assert_eq!(SerialSignal::from_name("nope"), None);
        // Every serial signal has an entry in the normal channel table.
        for s in SerialSignal::ALL {
            assert!(crate::types::channel(s.tag()).is_some(), "{}", s.tag());
        }
    }

    #[test]
    fn wifi_credentials_never_reach_the_caller() {
        let mut s = SerialSession::new(SerialSignal::Eda);
        let o = s.feed(
            "Adding SSID: TestNet -pass:hunter2 ... success\nAttempting to connect to SSID: TestNet\nSSID: TestNet\nIP Address: 10.0.0.5\n",
            0.0,
        );
        let logs: Vec<&str> = o.iter().filter_map(|x| if let Out::Log(l) = x { Some(l.as_str()) } else { None }).collect();
        assert_eq!(logs.len(), 4);
        for l in &logs[..3] {
            assert_eq!(*l, REDACTED);
        }
        assert_eq!(logs[3], "IP Address: 10.0.0.5");
        let all = format!("{o:?}");
        assert!(!all.contains("hunter2") && !all.contains("TestNet"));
    }

    #[test]
    fn garbage_does_not_grow_the_buffer_forever() {
        let mut s = SerialSession::new(SerialSignal::Eda);
        s.feed(&"x".repeat(10_000), 0.0);
        assert!(s.buf.len() <= 4096);
    }
}
