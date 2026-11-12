//! Browser API (wasm32 only).
//!
//! The page receives raw packet lines from the native bridge ([`crate::bridge`]) and feeds
//! them to a [`WebScope`], which is the same [`Scope`] the terminal UI uses: decoding,
//! sample timing, periodized heart rate / SCR, status, health, time window, pause and
//! autoscale all come from the shared Rust code. JavaScript only draws.
//!
//! Build with `scripts/build-web.sh` (wasm-pack, `--target web`).

use wasm_bindgen::prelude::*;

use crate::ble::{self, Converter, GapTracker, WireDecoder};
use crate::protocol::{encode, split_message};
use crate::scope::Scope;
use crate::serial::{Out, SerialBinner, SerialSession, SerialSignal, SerialState};
use crate::types::{channel, SensorReading};

#[wasm_bindgen]
pub struct WebScope {
    scope: Scope,
    packets: u64,
    malformed: u64,
    serial: Option<(SerialSession, SerialBinner)>,
    ble: Option<(Converter, GapTracker, WireDecoder)>,
    /// Last link diagnostics record from the BLE firmware: dropped, ATT MTU, batch, bytes.
    ble_link: Option<[f64; 4]>,
    /// Recording in the browser: raw lines in the stock format, the clock of the first reading,
    /// lines written, and the host time stamp (`TL` format) to anchor the recording with.
    rec: Option<(String, Option<f64>, u64, Option<String>)>,
    /// Packet number of the last control line sent to a BLE board.
    ble_sent: u16,
    /// The board's last `CG` (settings) payload, and its last `CS` answer.
    ble_settings: Option<Vec<String>>,
    ble_settings_reply: Option<Vec<String>>,
}

#[wasm_bindgen]
impl WebScope {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WebScope {
        WebScope { scope: Scope::new(), packets: 0, malformed: 0, serial: None, ble: None, ble_link: None, rec: None, ble_sent: 0, ble_settings: None, ble_settings_reply: None }
    }

    /// Feed newline-separated packet lines, as received at `now_ms` (`Date.now()`).
    pub fn push_lines(&mut self, text: &str, now_ms: f64) {
        for packet in split_message(text) {
            match packet {
                Ok(p) => {
                    self.packets += 1;
                    if let Some((text, _, lines, _)) = self.rec.as_mut() {
                        text.push_str(&p.to_line());
                        text.push('\n');
                        *lines += 1;
                    }
                    self.scope.push_packet(p, now_ms);
                }
                Err(_) => self.malformed += 1,
            }
        }
    }

    /// Tags that have data, in a stable order.
    pub fn tags(&self) -> Vec<String> {
        self.scope.tags().into_iter().map(String::from).collect()
    }

    /// Display name of a tag (`PPG:GRN`), or the tag itself if unknown.
    pub fn channel_name(&self, tag: &str) -> String {
        channel(tag).map(|c| c.name.to_string()).unwrap_or_else(|| tag.to_string())
    }

    pub fn channel_unit(&self, tag: &str) -> String {
        channel(tag).map(|c| c.unit.to_string()).unwrap_or_default()
    }

    /// Points in the current window, interleaved `[x0, y0, x1, y1, …]`; `x` is seconds before
    /// the newest sample (≤ 0).
    pub fn window(&self, tag: &str) -> Vec<f64> {
        self.scope.window(tag).into_iter().flat_map(|(x, y)| [x, y]).collect()
    }

    /// Autoscaled `[min, max]` for the current window.
    pub fn y_range(&self, tag: &str) -> Vec<f64> {
        let (lo, hi) = self.scope.y_range(tag);
        vec![lo, hi]
    }

    /// Newest value, or NaN if the tag has no data.
    pub fn last_value(&self, tag: &str) -> f64 {
        self.scope.last_value(tag).unwrap_or(f64::NAN)
    }

    pub fn window_s(&self) -> f64 {
        self.scope.window_s()
    }

    pub fn increment_window(&mut self) {
        self.scope.increment_window();
    }

    pub fn decrement_window(&mut self) {
        self.scope.decrement_window();
    }

    pub fn toggle_pause(&mut self) {
        self.scope.toggle_pause();
    }

    pub fn is_paused(&self) -> bool {
        self.scope.is_paused()
    }

    /// Device status as JSON (battery, recording, power mode, counters).
    pub fn status_json(&self) -> String {
        serde_json::to_string(self.scope.status()).unwrap_or_else(|_| "{}".into())
    }

    /// Stream health as JSON: `{lost, underruns, channels: {tag: {packets, samples, underruns, rate_hz}}}`.
    pub fn health_json(&self) -> String {
        let h = self.scope.health();
        let channels: std::collections::BTreeMap<&str, &crate::health::ChannelHealth> =
            h.iter().map(|(t, c)| (*t, c)).collect();
        serde_json::json!({ "lost": h.total_lost(), "underruns": h.total_underruns(), "channels": channels }).to_string()
    }

    // ── Bluetooth (Web Bluetooth, custom `emotibit-ble` firmware) ───────────────

    /// GATT service UUID to request.
    pub fn ble_service_uuid() -> String {
        ble::SERVICE_UUID.to_string()
    }

    /// Notify characteristic UUID.
    pub fn ble_data_uuid() -> String {
        ble::DATA_UUID.to_string()
    }

    /// Advertised device name to filter on.
    pub fn ble_device_name() -> String {
        ble::DEVICE_NAME.to_string()
    }

    /// Mode characteristic UUID: write 0 for raw records, 1 for the compact codec.
    pub fn ble_mode_uuid() -> String {
        ble::MODE_UUID.to_string()
    }

    /// Control characteristic UUID: stock packet lines from [`ble_command`](Self::ble_command).
    pub fn ble_control_uuid() -> String {
        ble::CONTROL_UUID.to_string()
    }

    /// Events characteristic UUID: status and time-sync lines for [`ble_events`](Self::ble_events).
    pub fn ble_events_uuid() -> String {
        ble::EVENTS_UUID.to_string()
    }

    /// A control line for the board: `tag` with `payload` elements, numbered by the page.
    /// `RB` (SD recording; payload: local time, optionally `FMT`, `BIN`), `RE`, `UN` (local
    /// time, note), `MN`/`ML`/`MM`/`MO`/`MH` (power).
    pub fn ble_command(&mut self, tag: &str, payload: Vec<String>, now_ms: f64) -> String {
        self.ble_sent = self.ble_sent.wrapping_add(1);
        crate::protocol::encode(tag, self.ble_sent, now_ms as u32, &payload)
    }

    /// Feed one events notification (status and time-sync lines). Updates the status; the
    /// lines stay out of the browser recording, which has its own clock. Returns the lines to
    /// write back to the control characteristic: the `TL`/`AK` answer to a time-sync request.
    pub fn ble_events(&mut self, text: &str, now_ms: f64, local_stamp: String) -> Vec<String> {
        let mut replies = Vec::new();
        for p in split_message(text).into_iter().flatten() {
            if p.tag == crate::protocol::tag::REQUEST_DATA {
                replies.push(self.ble_command(crate::protocol::tag::TIMESTAMP_LOCAL, vec![local_stamp.clone()], now_ms));
                let ack = vec![p.number.to_string(), p.tag.clone()];
                replies.push(self.ble_command(crate::protocol::tag::ACK, ack, now_ms));
            }
            match p.tag.as_str() {
                "CG" => self.ble_settings = Some(p.payload.clone()),
                "CS" | "CW" => self.ble_settings_reply = Some(p.payload.clone()),
                _ => {}
            }
            self.scope.push_packet(p, now_ms);
        }
        replies
    }

    /// The board's settings as JSON (`{"name": "...", "ppg_hz": "25", "leds": "1", "source":
    /// "flash", "restart_needed": "0", "device_id": "..."}`), or empty before the first `CG`
    /// answer. Ask with `ble_command("CG", [])`.
    pub fn ble_settings(&self) -> String {
        let Some(p) = &self.ble_settings else { return String::new() };
        let map: serde_json::Map<String, serde_json::Value> =
            p.chunks(2).filter_map(|kv| Some((kv.first()?.clone(), serde_json::Value::String(kv.get(1).cloned().unwrap_or_default())))).collect();
        serde_json::Value::Object(map).to_string()
    }

    /// The board's answer to the last settings change (`ok;saved`, `error;<why>`), once; then
    /// empty.
    pub fn ble_settings_reply(&mut self) -> String {
        self.ble_settings_reply.take().map(|p| p.join(";")).unwrap_or_default()
    }

    /// Start (or restart) a Bluetooth session: resets the converter, decoder and loss counters.
    /// `compact` must match the value written to the mode characteristic.
    pub fn ble_start(&mut self, compact: bool) {
        self.ble = Some((Converter::new(), GapTracker::default(), WireDecoder::new(compact)));
        self.ble_link = None;
    }

    /// Feed one notification (one or more 20-byte records). Readings go straight into the scope.
    pub fn ble_feed(&mut self, bytes: &[u8], now_ms: f64) {
        let Some((conv, gaps, wire)) = self.ble.as_mut() else { return };
        let mut readings = Vec::new();
        let mut batch_ms = 0;
        for p in wire.decode(bytes) {
            if p.tag == ble::id::CLOCK {
                batch_ms = p.samples.first().map_or(0, |&t| t as u32);
                continue;
            }
            self.packets += 1;
            gaps.observe(&p);
            if p.tag == ble::id::LINK && p.samples.len() == 4 {
                self.ble_link = Some([0, 1, 2, 3].map(|i| p.samples[i] as f64));
            }
            readings.extend(conv.readings(&p, now_ms, batch_ms));
        }
        for r in readings {
            self.push_reading(&r);
        }
    }

    /// Link diagnostics from the firmware, about once a second: `[dropped, att_mtu, batch,
    /// bytes]`, or empty before the first one arrives (or with older firmware).
    pub fn ble_link(&self) -> Vec<f64> {
        self.ble_link.map(|l| l.to_vec()).unwrap_or_default()
    }

    // ── Recording in the browser ─────────────────────────────────────────────────

    /// Start collecting everything that arrives as a raw recording in the stock format
    /// (`timestamp,packet,count,tag,version,reliability,values…`), which this page can replay
    /// and `emotibit parse` can split. Packets from the bridge, demo and replay are kept
    /// as-is; Bluetooth and USB readings get a millisecond clock starting at the first one.
    /// `local_stamp` (host local time, `2026-11-05_17-54-49-677532`) is written as a time-sync
    /// exchange at that first reading, so `emotibit parse` gives wall-clock times.
    pub fn record_start(&mut self, local_stamp: Option<String>) {
        self.rec = Some((String::new(), None, 0, local_stamp));
    }

    pub fn is_recording(&self) -> bool {
        self.rec.is_some()
    }

    /// Lines recorded so far.
    pub fn recorded_lines(&self) -> f64 {
        self.rec.as_ref().map_or(0.0, |r| r.2 as f64)
    }

    /// Stop and return the recording (empty if none was running).
    pub fn record_stop(&mut self) -> String {
        self.rec.take().map(|r| r.0).unwrap_or_default()
    }

    /// `[received, lost]` record counts for the current Bluetooth session.
    pub fn ble_counts(&self) -> Vec<f64> {
        match &self.ble {
            Some((_, g, _)) => vec![g.received as f64, g.lost as f64],
            None => vec![0.0, 0.0],
        }
    }

    // ── USB serial (Web Serial) ────────────────────────────────────────────────

    /// Names of the signals the firmware can print over serial.
    pub fn serial_signals() -> Vec<String> {
        SerialSignal::ALL.iter().map(|s| s.name().to_string()).collect()
    }

    /// Begin (or restart) a serial session for `signal` (e.g. `"eda"`). Returns false if the
    /// name is unknown. Call again to restart the handshake after rebooting the board.
    pub fn serial_start(&mut self, signal: &str) -> bool {
        match SerialSignal::from_name(signal) {
            Some(s) => {
                self.serial = Some((SerialSession::new(s), SerialBinner::new(s, 50.0)));
                true
            }
            None => false,
        }
    }

    /// Feed text read from the port. Returns JSON: a list of `{"send": "…"}`, `{"state": "…"}` and
    /// `{"log": "…"}` actions. Values go straight into the scope. `now_ms` is `performance.now()`
    /// or any monotonic millisecond clock.
    pub fn serial_feed(&mut self, text: &str, now_ms: f64) -> String {
        let outs = match self.serial.as_mut() {
            Some((session, _)) => session.feed(text, now_ms),
            None => return "[]".into(),
        };
        self.serial_apply(outs, now_ms)
    }

    /// Call every few hundred ms; returns actions in the same format as [`serial_feed`].
    pub fn serial_tick(&mut self, now_ms: f64) -> String {
        let outs = match self.serial.as_mut() {
            Some((session, _)) => session.tick(now_ms),
            None => return "[]".into(),
        };
        self.serial_apply(outs, now_ms)
    }

    /// `waiting`, `entering-debug`, `starting`, `streaming`, or `idle` before `serial_start`.
    pub fn serial_state(&self) -> String {
        match self.serial.as_ref().map(|(s, _)| s.state()) {
            None => "idle",
            Some(SerialState::WaitingForBoot) => "waiting",
            Some(SerialState::EnteringDebug) => "entering-debug",
            Some(SerialState::Starting) => "starting",
            Some(SerialState::Streaming) => "streaming",
        }
        .into()
    }

    pub fn packets(&self) -> f64 {
        self.packets as f64
    }

    pub fn malformed(&self) -> f64 {
        self.malformed as f64
    }
}

impl WebScope {
    /// Into the scope, and into the recording if one is running.
    fn push_reading(&mut self, r: &SensorReading) {
        if let Some((text, t0, lines, stamp)) = self.rec.as_mut() {
            if t0.is_none() {
                // The stock firmware's sync exchange (request, host time, acknowledgement), with
                // no round trip: device time 0 is the host time in `stamp`.
                if let Some(stamp) = stamp.take() {
                    text.push_str(&encode(crate::protocol::tag::REQUEST_DATA, 0, 0, &[crate::protocol::tag::TIMESTAMP_LOCAL.into()]));
                    text.push_str(&encode(crate::protocol::tag::TIMESTAMP_LOCAL, 1, 0, &[stamp]));
                    text.push_str(&encode(crate::protocol::tag::ACK, 2, 0, &["0".into()]));
                    *lines += 3;
                }
            }
            let t0 = *t0.get_or_insert(r.received_at_ms);
            let values: Vec<String> = r.samples.iter().map(|v| format!("{v}")).collect();
            let ts = (r.received_at_ms - t0).max(0.0) as u32;
            text.push_str(&encode(r.channel.tag, r.packet_number, ts, &values));
            *lines += 1;
        }
        self.scope.push_reading(r);
    }

    fn serial_apply(&mut self, outs: Vec<Out>, now_ms: f64) -> String {
        let mut actions = Vec::new();
        for o in outs {
            match o {
                Out::Send(b) => actions.push(serde_json::json!({ "send": String::from_utf8_lossy(&b) })),
                Out::Log(l) => actions.push(serde_json::json!({ "log": l })),
                Out::State(s) => {
                    let name = match s {
                        SerialState::WaitingForBoot => "waiting",
                        SerialState::EnteringDebug => "entering-debug",
                        SerialState::Starting => "starting",
                        SerialState::Streaming => "streaming",
                    };
                    actions.push(serde_json::json!({ "state": name }));
                }
                Out::Value(v) => {
                    if let Some((_, binner)) = self.serial.as_mut()
                        && let Some(r) = binner.push(v, now_ms)
                    {
                        self.push_reading(&r);
                    }
                }
            }
        }
        serde_json::Value::Array(actions).to_string()
    }
}

impl Default for WebScope {
    fn default() -> Self {
        Self::new()
    }
}
