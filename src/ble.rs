//! BLE transport for the custom `emotibit-ble` firmware (see `firmware/emotibit-ble`).
//!
//! The firmware exposes one GATT service with a notify characteristic. Each notification
//! (at most 20 bytes, so it fits the default ATT MTU) is:
//!
//! ```text
//! u8  tag      channel id (0 = heartbeat counter)
//! u8  count    samples that follow (0..=4)
//! u16 seq      per-channel sequence number (wraps)
//! i32 sample[] little-endian fixed-point samples
//! ```
//!
//! The decoder is portable. The client (`ble` feature) runs on `webbluetooth`, natively and in
//! a browser (`examples/web`).

/// Compact record codec, shared verbatim with the firmware.
#[path = "shared/codec.rs"]
pub mod codec;

/// Wire format: write 0 for raw 20-byte records, 1 for the compact codec.
pub const MODE_UUID: &str = "6e6f7469-6269-7400-0000-000000000003";
/// Host commands: stock packet lines (record, notes, markers, time sync, power modes).
pub const CONTROL_UUID: &str = "6e6f7469-6269-7400-0000-000000000004";
/// Device status and time-sync requests: stock packet lines (notify).
pub const EVENTS_UUID: &str = "6e6f7469-6269-7400-0000-000000000005";
/// Firmware update writes (`emotibit-ble` with update slots): begin, data, end; answers are
/// `OT` lines on the events characteristic.
pub const UPDATE_UUID: &str = "6e6f7469-6269-7400-0000-000000000006";

/// Primary service UUID.
pub const SERVICE_UUID: &str = "6e6f7469-6269-7400-0000-000000000001";
/// Notify characteristic carrying sensor packets.
pub const DATA_UUID: &str = "6e6f7469-6269-7400-0000-000000000002";
/// Advertised local name, or its prefix: the firmware appends the last two MAC bytes
/// (`EmotiBit-BLE-E848`) so boards can be told apart.
pub const DEVICE_NAME: &str = "EmotiBit-BLE";

/// True for a name the firmware advertises.
pub fn is_device_name(name: &str) -> bool {
    name == DEVICE_NAME || name.strip_prefix(DEVICE_NAME).is_some_and(|rest| rest.starts_with('-'))
}

/// One decoded notification.
#[derive(Debug, Clone, PartialEq)]
pub struct BlePacket {
    pub tag: u8,
    pub seq: u16,
    pub samples: Vec<i32>,
}

/// Decode one notification; `None` when it is malformed.
pub fn decode(bytes: &[u8]) -> Option<BlePacket> {
    if bytes.len() < 4 {
        return None;
    }
    let count = bytes[1] as usize;
    if count > 4 || bytes.len() < 4 + 4 * count {
        return None;
    }
    let samples = (0..count)
        .map(|i| i32::from_le_bytes(bytes[4 + 4 * i..8 + 4 * i].try_into().unwrap()))
        .collect();
    Some(BlePacket { tag: bytes[0], seq: u16::from_le_bytes([bytes[2], bytes[3]]), samples })
}

/// Size of one record; a notification carries one or more of them back to back.
pub const RECORD_LEN: usize = 20;

/// Decode every record in a notification (the firmware batches them up to the ATT MTU).
pub fn decode_all(bytes: &[u8]) -> Vec<BlePacket> {
    bytes.chunks(RECORD_LEN).filter_map(decode).collect()
}

/// Decodes notifications in either wire format.
#[derive(Debug, Default)]
pub struct WireDecoder {
    compact: bool,
    decoder: Option<Box<codec::Decoder>>,
}

impl WireDecoder {
    pub fn new(compact: bool) -> Self {
        Self { compact, decoder: compact.then(Default::default) }
    }

    /// Decode one notification. A malformed compact record stops that notification; the
    /// decoder keeps its state so the next key frame resynchronises it.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<BlePacket> {
        match self.decoder.as_mut() {
            Some(d) if self.compact => {
                let mut out = Vec::new();
                d.decode(bytes, |tag, seq, v| out.push(BlePacket { tag, seq, samples: v.to_vec() }));
                out
            }
            _ => decode_all(bytes),
        }
    }
}

/// Counts packets lost per channel from the 16-bit sequence numbers.
#[derive(Debug, Default)]
pub struct GapTracker {
    last: std::collections::HashMap<u8, u16>,
    pub lost: u64,
    pub received: u64,
}

impl GapTracker {
    pub fn observe(&mut self, p: &BlePacket) {
        self.received += 1;
        if let Some(prev) = self.last.insert(p.tag, p.seq) {
            self.lost += p.seq.wrapping_sub(prev).wrapping_sub(1) as u64;
        }
    }
}

/// Raw-to-stock unit conversions, shared verbatim with the firmware.
#[path = "shared/units.rs"]
pub mod units;
/// Records to stock tags, the stock line format and the binary SD framing, shared verbatim
/// with the firmware.
#[path = "shared/stock.rs"]
pub mod stock;
/// The firmware's numeric settings and profiles (`CS`/`CG`), shared verbatim with the firmware.
#[path = "shared/tuning.rs"]
pub mod tuning;

/// Channel ids sent by the firmware.
pub use stock::id;

/// Turns raw firmware packets into stock-firmware tags and units, plus the metrics the host
/// derives (heart rate, SCR).
///
/// It keeps the calibration words the firmware repeats every 10 s, so thermopile and
/// magnetometer values appear once they have arrived.
#[derive(Debug)]
pub struct Converter {
    records: stock::Records,
    heart: crate::derive::HeartRate,
    scr: crate::derive::ScrDetector,
    ppg_hz: f64,
}

impl Default for Converter {
    fn default() -> Self {
        Self::new()
    }
}

impl Converter {
    pub fn new() -> Self {
        Self {
            records: stock::Records::new(),
            // Nominal firmware rates: PPG 400 sps / 16 averaged, EDA 250 sps / 16 averaged.
            heart: crate::derive::HeartRate::new(25.0),
            scr: crate::derive::ScrDetector::new(15.625),
            ppg_hz: 25.0,
        }
    }

    /// Converts one packet into `(emotibit tag, value)` pairs, one per channel sample.
    /// Calibration packets are absorbed and produce nothing.
    pub fn convert(&mut self, p: &BlePacket) -> Vec<(&'static str, f64)> {
        use crate::protocol::tag as t;
        let mut out = Vec::new();
        self.records.convert(p.tag, &p.samples, |tag, v| out.push((tag, v)));
        match p.tag {
            // The board's PPG rate (25 or 100 Hz) sets the heart-rate detector's time constants.
            id::INFO => {
                if let Some(&hz) = p.samples.first()
                    && hz > 0 && hz as f64 != self.ppg_hz {
                        self.ppg_hz = hz as f64;
                        self.heart = crate::derive::HeartRate::new(self.ppg_hz);
                    }
            }
            // Firmware before the eFuse calibration sent only the raw count.
            id::BATTERY if p.samples.len() == 1 => {
                let v = battery_volts(p.samples[0] as f64);
                out = vec![(t::BATTERY_VOLTAGE, v), (t::BATTERY_PERCENT, battery_percent(v))];
            }
            id::EDA => {
                if let Some(r) = self.scr.push(out[0].1) {
                    out.extend([(t::SCR_AMPLITUDE, r.amplitude), (t::SCR_RISE_TIME, r.rise_time), (t::SCR_FREQUENCY, r.per_minute)]);
                }
            }
            id::PPG => {
                if let Some(b) = self.heart.push(out[1].1) {
                    out.extend([(t::HEART_RATE, b.bpm), (t::INTER_BEAT_INTERVAL, b.ibi_ms)]);
                }
            }
            _ => {}
        }
        out
    }

    /// Converts a packet to [`SensorReading`](crate::types::SensorReading)s so the rest of the
    /// crate can treat BLE like the WiFi stream. `device_ms` is the device clock estimate.
    pub fn readings(&mut self, p: &BlePacket, now_ms: f64, device_ms: u32) -> Vec<crate::types::SensorReading> {
        self.convert(p)
            .into_iter()
            .filter_map(|(tag, v)| {
                Some(crate::types::SensorReading {
                    channel: crate::types::channel(tag)?,
                    packet_number: p.seq,
                    device_timestamp_ms: device_ms,
                    received_at_ms: now_ms,
                    samples: vec![v],
                })
            })
            .collect()
    }
}

/// Converts a binary SD file (`.EBB`, see [`stock::FRAME_RECORD`]) into stock packet lines, as
/// the board would have written them in CSV mode. Truncated trailing frames are ignored.
pub fn binary_to_lines(bytes: &[u8]) -> Option<Vec<String>> {
    let mut rest = bytes.strip_prefix(stock::BINARY_MAGIC.as_slice())?;
    let mut records = stock::Records::new();
    // Heart rate and SCR, as the board writes them in CSV mode (PPG rate from INFO records).
    let mut derived = crate::derive::Derived::new(25.0, 15.625);
    let mut ppg_hz = 25;
    let mut lines = Vec::new();
    let mut number: u16 = 0;
    while let Some((&kind, tail)) = rest.split_first() {
        match kind {
            stock::FRAME_RECORD if tail.len() >= 24 => {
                let ts = u32::from_le_bytes(tail[..4].try_into().unwrap());
                if let Some(p) = decode(&tail[4..24]) {
                    if p.tag == id::INFO && p.samples.first().is_some_and(|&hz| hz > 0 && hz != ppg_hz) {
                        ppg_hz = p.samples[0];
                        derived = crate::derive::Derived::new(ppg_hz as f64, 15.625);
                    }
                    let mut values = Vec::new();
                    records.convert(p.tag, &p.samples, |tag, v| values.push((tag, v)));
                    for (tag, v) in values {
                        let mut out = vec![(tag, v)];
                        derived.push(tag, v, |dt, dv| out.push((dt, dv)));
                        for (t, x) in out {
                            let mut f = stock::Fixed::<96>::new();
                            let _ = stock::write_value(&mut f, ts, number, t, x);
                            number = number.wrapping_add(1);
                            lines.push(f.as_str().trim_end().to_string());
                        }
                    }
                }
                rest = &tail[24..];
            }
            stock::FRAME_TEXT if tail.len() >= 2 => {
                let n = u16::from_le_bytes([tail[0], tail[1]]) as usize;
                let Some(text) = tail.get(2..2 + n) else { break };
                lines.extend(String::from_utf8_lossy(text).lines().map(str::to_string));
                rest = &tail[2 + n..];
            }
            _ => break,
        }
    }
    Some(lines)
}

/// Battery voltage from the 12-bit reading of the divider pin (half the battery voltage),
/// at 11 dB attenuation, for firmware that sends no calibrated value: esp-idf's default line
/// (1100 mV reference, no eFuse calibration).
pub fn battery_volts(raw: f64) -> f64 {
    let pin_mv = (1100.0_f64 * 197_611.0 / 4096.0).floor() * raw / 65536.0 + 142.0;
    pin_mv * 2.0 / 1000.0
}

/// The stock firmware's ESP32 battery curve (3.4 V empty, 4.05 V full).
pub use units::battery_percent;

/// Link statistics, reported as an `LK` message about once a second: dropped-in-firmware, ATT
/// MTU, records in the last batch, bytes on air, records lost on the way (from sequence gaps),
/// records received.
pub const LINK_TAG: &str = "LK";

#[cfg(feature = "ble")]
pub use client::*;

/// The BLE client, on [`webbluetooth`]: CoreBluetooth, BlueZ or WinRT natively, and the
/// browser's own Web Bluetooth in a page (`examples/web`). The same code runs on both; spawning
/// and timers go through [`crate::platform`].
#[cfg(feature = "ble")]
mod client {
    use super::*;
    use crate::handle::EmotiBitHandle;
    use crate::platform;
    use crate::protocol::{self, tag, Packet};
    use crate::types::{EmotiBitDevice, EmotiBitEvent};
    use anyhow::{anyhow, Context, Result};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use webbluetooth::stream::{select_all, BoxStream, StreamExt};
    use webbluetooth::uuid::BluetoothUuid;
    use webbluetooth::{Bluetooth, BluetoothDevice, DeviceFilter, RemoteGattCharacteristic, RequestDeviceOptions};

    fn uuid(s: &str) -> uuid::Uuid {
        uuid::Uuid::parse_str(s).expect("valid uuid")
    }

    /// Boards by name (all of them, or one), with access to the EmotiBit service. The service is
    /// named because a Web Bluetooth grant covers exactly the services a request lists.
    fn request_options(name: Option<&str>) -> Result<RequestDeviceOptions> {
        let filter = match name {
            Some(n) => DeviceFilter::new().name(n),
            None => DeviceFilter::new().name_prefix(DEVICE_NAME),
        };
        Ok(RequestDeviceOptions::new().filter(filter).optional_service(uuid(SERVICE_UUID))?)
    }

    /// Devices found by [`scan`], so connecting to one does not scan again.
    #[cfg(not(target_arch = "wasm32"))]
    static FOUND: std::sync::Mutex<Vec<(String, BluetoothDevice)>> = std::sync::Mutex::new(Vec::new());

    /// Scan for `timeout` and return every board running the firmware (native; a page asks
    /// the user with [`request`] instead).
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn scan(timeout: Duration) -> Result<Vec<(EmotiBitDevice, BluetoothDevice)>> {
        use webbluetooth::{Grant, LeScanOptions};
        let bluetooth = Bluetooth::shared();
        bluetooth.availability().await.context("Bluetooth is not usable")?;
        let mut scan = bluetooth.request_le_scan(LeScanOptions::new().filter(DeviceFilter::new().name_prefix(DEVICE_NAME))).await?;
        let mut seen: Vec<webbluetooth::Candidate> = Vec::new();
        let _ = platform::timeout(timeout, async {
            while let Some(c) = scan.next().await {
                if is_device_name(c.label()) && seen.iter().all(|s| s.id != c.id) {
                    seen.push(c);
                }
            }
        })
        .await;
        scan.stop();
        let grant = Grant::new().service(uuid(SERVICE_UUID))?;
        let mut found = Vec::new();
        for c in seen {
            let name = c.label().to_string();
            match bluetooth.adopt_candidate(&c, grant.clone()).await {
                Ok(device) => {
                    FOUND.lock().expect("found mutex").retain(|(n, _)| *n != name);
                    FOUND.lock().expect("found mutex").push((name.clone(), device.clone()));
                    found.push((EmotiBitDevice { id: name, ip: std::net::Ipv4Addr::UNSPECIFIED.into(), available: true }, device));
                }
                Err(e) => log::warn!("{name}: cannot use it ({e})"),
            }
        }
        Ok(found)
    }

    /// The first board (or the one named `name`) to answer: natively the first that
    /// advertises within `timeout`; in a page, the one the user picks in the browser's chooser
    /// (call it from a user gesture).
    pub async fn request(name: Option<&str>, timeout: Duration) -> Result<BluetoothDevice> {
        let bluetooth = Bluetooth::shared();
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(n) = name
                && let Some((_, d)) = FOUND.lock().expect("found mutex").iter().find(|(f, _)| f == n) {
                    return Ok(d.clone());
                }
            bluetooth.availability().await.context("Bluetooth is not usable")?;
        }
        let chooser = webbluetooth::chooser::FirstMatch::with_timeout(timeout);
        bluetooth
            .request_device_with(request_options(name)?, &chooser)
            .await
            .map_err(|e| anyhow!("no {} found: {e}", name.unwrap_or(DEVICE_NAME)))
    }

    /// Connect to a board found by [`scan`] (by its name) and run it behind an
    /// [`EmotiBitHandle`]; see [`connect_device`].
    pub async fn establish(device: &EmotiBitDevice, compact: bool, scan_timeout: Duration) -> Result<(mpsc::Receiver<EmotiBitEvent>, EmotiBitHandle)> {
        let bt = request(Some(&device.id), scan_timeout.max(Duration::from_secs(10))).await?;
        connect_device(bt, compact).await
    }

    /// Installs an emotibit-ble app image over Bluetooth: the board writes it into the app slot
    /// it is not running, checks it (SHA-256, image hash), and restarts into it; a failed or
    /// interrupted update leaves the running firmware as it was. The image goes compressed
    /// ([`compress_image`]) to firmware that takes it, as it is otherwise. `progress` gets the bytes
    /// sent and the total; the result is the bytes sent. The board needs emotibit-ble with update
    /// slots, installed over USB once.
    pub async fn update_firmware(bt: BluetoothDevice, image: &[u8], mut progress: impl FnMut(usize, usize)) -> Result<usize> {
        use sha2::{Digest, Sha256};
        /// Writes per window: the board's stack queues at most 8 incoming writes.
        const WINDOW: usize = 6;
        let name = bt.name().unwrap_or_else(|| bt.id().to_string());
        let gatt = bt.gatt();
        platform::timeout(Duration::from_secs(10), gatt.connect()).await.map_err(|_| anyhow!("{name}: connect timed out"))??;
        let result = async {
            let service = platform::timeout(Duration::from_secs(15), gatt.get_primary_service(uuid(SERVICE_UUID)))
                .await
                .map_err(|_| anyhow!("{name}: service discovery timed out"))??;
            let chars = service.get_characteristics(None).await?;
            let update = find(&chars, UPDATE_UUID)
                .context("this firmware cannot be updated over Bluetooth: install the new one over USB once (emotibit firmware ble)")?;
            let events = find(&chars, EVENTS_UUID).context("device has no events characteristic")?;
            // `OT` answers, from their own task so they arrive while writes wait.
            let mut stream = events.start_notifications().await?;
            let (tx, mut rx) = mpsc::unbounded_channel::<Vec<String>>();
            platform::spawn(async move {
                while let Some(v) = stream.next().await {
                    for p in protocol::split_message(&String::from_utf8_lossy(&v)).into_iter().flatten() {
                        if p.tag == "OT" && tx.send(p.payload).is_err() {
                            return;
                        }
                    }
                }
            });
            // The next answer other than progress.
            async fn answer(rx: &mut mpsc::UnboundedReceiver<Vec<String>>, secs: u64) -> Result<Vec<String>> {
                platform::timeout(Duration::from_secs(secs), async {
                    loop {
                        match rx.recv().await {
                            Some(p) if p.first().is_some_and(|s| s == "progress") => continue,
                            other => return other,
                        }
                    }
                })
                .await
                .map_err(|_| anyhow!("the board did not answer"))?
                .ok_or_else(|| anyhow!("the link dropped"))
            }
            fn board_error(p: &[String]) -> anyhow::Error {
                anyhow!("the board: {}", if p.first().is_some_and(|s| s == "error") { p[1..].join(",") } else { p.join(",") })
            }

            // Compressed first; firmware without it calls that write malformed.
            let sha = Sha256::digest(image);
            let compressed = compress_image(image);
            let mut begin = vec![b'Z'];
            begin.extend_from_slice(&(image.len() as u32).to_le_bytes());
            begin.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
            begin.extend_from_slice(&sha);
            update.write_value_with_response(&begin).await?;
            let mut p = answer(&mut rx, 15).await?;
            let mut data: &[u8] = &compressed;
            if p.first().is_some_and(|s| s == "error") && p.get(1).is_some_and(|s| s == "malformed write") {
                let mut begin = vec![b'B'];
                begin.extend_from_slice(&(image.len() as u32).to_le_bytes());
                begin.extend_from_slice(&sha);
                update.write_value_with_response(&begin).await?;
                p = answer(&mut rx, 15).await?;
                data = image;
            }
            if p.first().map(String::as_str) != Some("ready") {
                return Err(board_error(&p));
            }
            // Writes as long as the ATT MTU allows, less the header and offset. Firmware that takes
            // them without response gets a window at a time, the last write of each with response
            // (see the firmware's `ota`): several per connection interval instead of one per two.
            let chunk = update.max_write_length(false).unwrap_or(185).clamp(32, 512) - 5;
            let window = if update.properties().write_without_response() { WINDOW } else { 1 };
            let total = data.len();
            let chunks = total.div_ceil(chunk);
            let mut sent = 0;
            for (i, data) in data.chunks(chunk).enumerate() {
                let mut w = Vec::with_capacity(5 + data.len());
                w.push(b'D');
                w.extend_from_slice(&(sent as u32).to_le_bytes());
                w.extend_from_slice(data);
                let synced = (i + 1) % window == 0 || i + 1 == chunks;
                let written = if synced { update.write_value_with_response(&w).await } else { update.write_value_without_response(&w).await };
                if let Err(e) = written {
                    let _ = update.write_value_with_response(b"A").await;
                    return Err(anyhow!("writing at byte {sent} failed: {e}"));
                }
                sent += data.len();
                while let Ok(p) = rx.try_recv() {
                    if p.first().is_some_and(|s| s == "error") {
                        return Err(board_error(&p));
                    }
                }
                progress(sent, total);
            }
            update.write_value_with_response(b"E").await?;
            let p = answer(&mut rx, 60).await?;
            if p.first().map(String::as_str) != Some("done") {
                return Err(board_error(&p));
            }
            Ok(total)
        }
        .await;
        gatt.disconnect();
        result
    }

    /// An app image as the firmware takes it compressed: per 4 KB sector, a u16 (LE) length and
    /// the sector as raw deflate, or as it is (the length's top bit set) when that is no shorter.
    /// Each sector stands alone, so the board decompresses it without a 32 KB history.
    pub fn compress_image(image: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(image.len());
        for sector in image.chunks(4096) {
            let packed = miniz_oxide::deflate::compress_to_vec(sector, 10);
            let (head, body) = if packed.len() < sector.len() { (packed.len() as u16, &packed[..]) } else { (sector.len() as u16 | 0x8000, sector) };
            out.extend_from_slice(&head.to_le_bytes());
            out.extend_from_slice(body);
        }
        out
    }

    /// Device clock estimate from the timestamps of the lines the device sends (`EM` every
    /// second) and the batch clocks: the smallest `host - device` offset seen, i.e. the fastest
    /// delivery.
    #[derive(Default)]
    struct DeviceClock {
        offset: Option<f64>,
    }

    impl DeviceClock {
        fn observe(&mut self, device_ms: u32, host_ms: f64) {
            let o = host_ms - device_ms as f64;
            self.offset = Some(self.offset.map_or(o, |prev| prev.min(o)));
        }
        fn device_ms(&self, host_ms: f64) -> Option<u32> {
            self.offset.map(|o| (host_ms - o).max(0.0) as u32)
        }
    }

    fn find(chars: &[RemoteGattCharacteristic], u: &str) -> Option<RemoteGattCharacteristic> {
        let want = BluetoothUuid::from(uuid(u));
        chars.iter().find(|c| *c.uuid() == want).cloned()
    }

    async fn subscribe(c: &RemoteGattCharacteristic, events: bool) -> Result<BoxStream<'static, (bool, Vec<u8>)>> {
        Ok(c.start_notifications().await?.map(move |v| (events, v)).boxed())
    }

    /// Connect a board and run it behind an [`EmotiBitHandle`]: sensor readings, status
    /// (`EM`), time sync (`RD` answered with `TL`/`AK`), and commands (recording, notes,
    /// markers, power modes, settings) as on WiFi.
    pub async fn connect_device(bt: BluetoothDevice, compact: bool) -> Result<(mpsc::Receiver<EmotiBitEvent>, EmotiBitHandle)> {
        let name = bt.name().unwrap_or_else(|| bt.id().to_string());
        let gatt = bt.gatt();
        platform::timeout(Duration::from_secs(10), gatt.connect()).await.map_err(|_| anyhow!("{name}: connect timed out"))??;
        let service = platform::timeout(Duration::from_secs(15), gatt.get_primary_service(uuid(SERVICE_UUID)))
            .await
            .map_err(|_| anyhow!("{name}: service discovery timed out"))??;
        let chars = service.get_characteristics(None).await?;
        let data = find(&chars, DATA_UUID).context("device has no EmotiBit data characteristic")?;
        let mode = find(&chars, MODE_UUID);
        let control = find(&chars, CONTROL_UUID);
        let events = find(&chars, EVENTS_UUID);
        // Choose the wire format before data starts flowing; firmware older than the mode
        // characteristic simply keeps sending raw records.
        let compact = compact && mode.is_some();
        if let Some(m) = &mode {
            m.write_value_with_response(&[compact as u8]).await?;
        }
        let mut streams = vec![subscribe(&data, false).await?];
        if let Some(e) = &events {
            streams.push(subscribe(e, true).await?);
        }
        // Notifications sent before the subscription were discarded, taking the compact
        // stream's first key frames with them; writing the mode again restarts the encoder.
        if let Some(m) = &mode {
            m.write_value_with_response(&[compact as u8]).await?;
        }
        let mut notifications = select_all(streams);

        let device = EmotiBitDevice { id: name, ip: std::net::Ipv4Addr::UNSPECIFIED.into(), available: true };
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<String>();
        let handle = EmotiBitHandle::new_ble(device.clone(), control_tx);
        let (tx, rx) = mpsc::channel::<EmotiBitEvent>(8192);
        let _ = tx.send(EmotiBitEvent::Connected(device)).await;

        // The link can drop before the notification streams notice.
        {
            let h = handle.clone();
            let mut disconnects = bt.watch_disconnect();
            platform::spawn(async move {
                if disconnects.next().await.is_some() {
                    log::warn!("{}: Bluetooth link lost", h.device().id);
                    h.mark_disconnected();
                }
            });
        }
        // Commands go out from their own task: a write waits a connection interval or two for
        // its response, and notifications should not wait behind it.
        {
            let mut stop = handle.shutdown_signal();
            platform::spawn(async move {
                loop {
                    let line = tokio::select! {
                        biased;
                        _ = stop.changed() => break,
                        l = control_rx.recv() => match l { Some(l) => l, None => break },
                    };
                    match &control {
                        Some(c) => {
                            if let Err(e) = c.write_value_with_response(line.as_bytes()).await {
                                log::warn!("BLE control write failed: {e}");
                            }
                        }
                        None => log::warn!("this firmware takes no commands (flash a newer one)"),
                    }
                }
            });
        }
        let h = handle.clone();
        let mut stop = handle.shutdown_signal();
        platform::spawn(async move {
            let mut wire = WireDecoder::new(compact);
            let mut conv = Converter::new();
            let mut gaps = GapTracker::default();
            let mut clock = DeviceClock::default();
            let started = platform::now_ms();
            loop {
                let next = tokio::select! {
                    biased;
                    _ = stop.changed() => break,
                    n = notifications.next() => n,
                };
                let Some((is_event, value)) = next else { break };
                let now = platform::now_ms();
                if is_event {
                    for p2 in protocol::split_message(&String::from_utf8_lossy(&value)).into_iter().flatten() {
                        clock.observe(p2.timestamp, now);
                        if p2.tag == tag::REQUEST_DATA {
                            // Time sync, as the WiFi client answers it.
                            if p2.payload.iter().any(|e| e == tag::TIMESTAMP_LOCAL) {
                                let _ = h.send_control(tag::TIMESTAMP_LOCAL, &[platform::local_timestamp()]).await;
                            }
                            let _ = h.send_control(tag::ACK, &[p2.number.to_string(), p2.tag.clone()]).await;
                        }
                        if !h.dispatch_packet(&tx, p2, now).await {
                            return;
                        }
                    }
                    continue;
                }
                // Firmware with batch clocks puts the device time of the batch first.
                let mut batch_ms: Option<u32> = None;
                for pkt in wire.decode(&value) {
                    if pkt.tag == id::CLOCK {
                        if let Some(&ts) = pkt.samples.first() {
                            batch_ms = Some(ts as u32);
                            clock.observe(ts as u32, now);
                        }
                        continue;
                    }
                    let lost_before = gaps.lost;
                    gaps.observe(&pkt);
                    // Readings are numbered by one host counter, as a stock stream is; records
                    // lost on the way skip numbers so they count as gaps.
                    for _ in lost_before..gaps.lost.min(lost_before + 1000) {
                        h.next_number();
                    }
                    if pkt.tag == id::LINK && pkt.samples.len() == 4 {
                        let mut v: Vec<String> = pkt.samples.iter().map(|x| x.to_string()).collect();
                        v.extend([gaps.lost.to_string(), gaps.received.to_string()]);
                        let lk = Packet {
                            timestamp: (now - started) as u32,
                            number: h.next_number(),
                            data_length: v.len() as u16,
                            tag: LINK_TAG.into(),
                            protocol_version: 1,
                            reliability: 100,
                            payload: v,
                        };
                        if tx.send(EmotiBitEvent::Message(lk)).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    let device_ms = batch_ms.or_else(|| clock.device_ms(now));
                    for mut r in conv.readings(&pkt, now, device_ms.unwrap_or(0)) {
                        r.packet_number = h.next_number();
                        // Lines for recorders, once the device clock is known.
                        let line = device_ms.map(|ts| {
                            let v = r.samples[0];
                            let v = if v == v.trunc() && v.abs() < 1e9 { format!("{}", v as i64) } else { format!("{v:.6}") };
                            protocol::encode(r.channel.tag, r.packet_number, ts, &[v]).trim_end().to_string()
                        });
                        if !h.dispatch_reading(&tx, r, line).await {
                            return;
                        }
                    }
                }
            }
            gatt.disconnect();
            if h.mark_disconnected() {
                let _ = tx.send(EmotiBitEvent::Disconnected).await;
            }
        });
        Ok((rx, handle))
    }
}

#[cfg(test)]
mod tests {
    /// The compressed update stream decodes back to the image, frame by frame as the firmware
    /// reads it (`firmware/emotibit-ble/src/ota.rs`).
    #[cfg(feature = "ble")]
    #[test]
    fn compressed_image_round_trips() {
        // Compressible and incompressible sectors, and a short last one.
        let mut image: Vec<u8> = (0..20_000u32).map(|i| (i / 7) as u8).collect();
        let mut x = 0x1234_5678u32;
        image.extend((0..5000).map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        }));
        let stream = super::compress_image(&image);
        assert!(stream.len() < image.len());
        let (mut out, mut at, mut stored) = (Vec::new(), 0, 0);
        while at < stream.len() {
            let head = u16::from_le_bytes([stream[at], stream[at + 1]]);
            let (len, raw) = ((head & 0x7FFF) as usize, head & 0x8000 != 0);
            let body = &stream[at + 2..at + 2 + len];
            if raw {
                stored += 1;
                out.extend_from_slice(body);
            } else {
                let sector = miniz_oxide::inflate::decompress_to_vec(body).unwrap();
                assert!(sector.len() <= 4096);
                out.extend_from_slice(&sector);
            }
            at += 2 + len;
        }
        assert_eq!(out, image);
        assert!(stored >= 1, "random data is stored as it is");
    }

    use super::*;

    #[test]
    fn wire_decoder_reads_both_formats() {
        let mut raw = [0u8; 20];
        raw[1] = 2;
        raw[2..4].copy_from_slice(&9u16.to_le_bytes());
        raw[4..8].copy_from_slice(&100i32.to_le_bytes());
        raw[8..12].copy_from_slice(&(-7i32).to_le_bytes());
        let want = BlePacket { tag: 0, seq: 9, samples: vec![100, -7] };
        assert_eq!(WireDecoder::new(false).decode(&raw), vec![want.clone()]);
        let mut e = codec::Encoder::new();
        let (mut wire, mut len) = ([0u8; 64], 0);
        assert!(e.encode(&raw, &mut wire, &mut len));
        assert!(len < 20);
        let got = WireDecoder::new(true).decode(&wire[..len]);
        assert_eq!(got[0].samples, want.samples);
        assert_eq!(got[0].seq & 0xFF, 9);
    }

    #[test]
    fn decodes_and_tracks_gaps() {
        let mut b = [0u8; 20];
        b[1] = 1;
        b[2..4].copy_from_slice(&7u16.to_le_bytes());
        b[4..8].copy_from_slice(&(-5i32).to_le_bytes());
        let p = decode(&b).unwrap();
        assert_eq!(p, BlePacket { tag: 0, seq: 7, samples: vec![-5] });
        let mut g = GapTracker::default();
        g.observe(&p);
        g.observe(&BlePacket { seq: 10, ..p.clone() });
        assert_eq!((g.received, g.lost), (2, 2));
        assert!(decode(&b[..3]).is_none());
        let mut two = b.to_vec();
        two.extend_from_slice(&b);
        assert_eq!(decode_all(&two).len(), 2);
        b[1] = 5;
        assert!(decode(&b).is_none());
    }

    fn pkt(tag: u8, samples: &[i32]) -> BlePacket {
        BlePacket { tag, seq: 0, samples: samples.to_vec() }
    }

    #[test]
    fn converts_stock_units() {
        let mut c = Converter::new();
        // 8 g range: 4096 counts is 1 g; gyro 1000 dps: 32768 counts is 1000 dps.
        assert_eq!(c.convert(&pkt(id::ACCEL, &[4096, 0, -4096])), vec![("AX", 1.0), ("AY", 0.0), ("AZ", -1.0)]);
        assert_eq!(c.convert(&pkt(id::GYRO, &[32768, 0, 0]))[0], ("GX", 1000.0));
        // Raw 16 x 55 counts -> 55*728.84+14179797 ohms -> about 0.0703 uS.
        let eda = c.convert(&pkt(id::EDA, &[16 * 55]))[0].1;
        assert!((eda - 0.07033).abs() < 1e-4, "{eda}");
        assert_eq!(c.convert(&pkt(id::TEMP1, &[36_750]))[0], ("T1", 36.75));
        // No calibration yet: magnetometer and thermopile stay silent.
        assert!(c.convert(&pkt(id::MAG, &[1, 2, 3, 4])).is_empty());
        assert!(c.convert(&pkt(id::THERMOPILE_RAW, &[1, 2, 3])).is_empty());
        assert_eq!(battery_percent(4.2), 100.0);
        assert_eq!(battery_percent(3.0), 0.0);
        // Calibrated pin millivolts win over the raw count; old firmware sends only the count,
        // converted with esp-idf's default line (raw 2048 -> 1800 mV at the pin).
        assert_eq!(c.convert(&pkt(id::BATTERY, &[2048, 1900]))[0], ("BV", 3.8));
        let old = c.convert(&pkt(id::BATTERY, &[2048]))[0].1;
        assert!((old - 3.6).abs() < 0.002, "{old}");
    }

    #[test]
    fn binary_file_converts_to_stock_lines() {
        let mut f = stock::BINARY_MAGIC.to_vec();
        let line = "100,0,2,UN,1,100,2026-11-07_18-00-00-000000,start\n";
        f.push(stock::FRAME_TEXT);
        f.extend((line.len() as u16).to_le_bytes());
        f.extend(line.as_bytes());
        let mut rec = [0u8; 20];
        rec[0] = id::ACCEL;
        rec[1] = 3;
        rec[4..8].copy_from_slice(&4096i32.to_le_bytes());
        f.push(stock::FRAME_RECORD);
        f.extend(250u32.to_le_bytes());
        f.extend(rec);
        f.push(stock::FRAME_RECORD); // truncated frame at the end is ignored
        let lines = binary_to_lines(&f).unwrap();
        assert_eq!(lines, ["100,0,2,UN,1,100,2026-11-07_18-00-00-000000,start", "250,0,1,AX,1,100,1", "250,1,1,AY,1,100,0", "250,2,1,AZ,1,100,0"]);
        assert!(binary_to_lines(b"not binary").is_none());
    }

    #[test]
    fn calibration_enables_magnetometer() {
        let mut c = Converter::new();
        // x1, y1, x2, y2, z1, z2, z3, z4, xy1, xy2, xyz1
        for (i, v) in [0, 0, 0, 0, 100, 100, 0, 0, 0, 0, 1000].into_iter().enumerate() {
            assert!(c.convert(&pkt(id::MAG_CAL, &[i as i32, v])).is_empty());
        }
        let out = c.convert(&pkt(id::MAG, &[100, 0, 0, 16384]));
        assert_eq!(out.len(), 3);
        assert!(out[0].1.is_finite());
    }
}
