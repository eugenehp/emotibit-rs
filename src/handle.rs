//! A connected device behind one handle, whatever the link: the stock firmware over WiFi
//! (`emotibit_client`, native only) or the BLE firmware (`ble`, native and browser). Commands go
//! out as stock packet lines; what comes back is published as [`EmotiBitEvent`]s and raw lines.

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{anyhow, Result};
use tokio::sync::{broadcast, mpsc, watch};

use crate::parse::to_event;
use crate::protocol::{self, tag, Packet};
use crate::status::{DeviceStatus, PowerMode, StatusTracker};
use crate::types::{EmotiBitDevice, EmotiBitEvent};

pub(crate) struct Inner {
    pub(crate) device: EmotiBitDevice,
    /// Host milliseconds at the start (`platform::now_ms`), for packet timestamps.
    started_ms: f64,
    packet_counter: AtomicU16,
    /// Numbers for what a BLE board sends (its records carry none of their own).
    received_counter: AtomicU16,
    pub(crate) connected: AtomicBool,
    pub(crate) link: Link,
    pub(crate) shutdown: watch::Sender<bool>,
    /// Every packet received from the device, as one line, for recorders and forwarders.
    raw: broadcast::Sender<Arc<str>>,
    status: StdMutex<StatusTracker>,
    /// Last `(timestamp, packet number)` per tag, to drop exact duplicates.
    seen: StdMutex<std::collections::HashMap<String, (u32, u16)>>,
}

/// How commands reach the device.
pub(crate) enum Link {
    /// Stock firmware: TCP control channel (once the device has connected back) and the UDP
    /// advertising socket.
    #[cfg(not(target_arch = "wasm32"))]
    Wifi { control_writer: tokio::sync::Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>, adv: Arc<tokio::net::UdpSocket> },
    /// BLE firmware: packet lines for the BLE task to write to the control characteristic.
    Ble { control: mpsc::UnboundedSender<String> },
}

impl Inner {
    fn new(device: EmotiBitDevice, link: Link, first_number: u16) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            device,
            started_ms: crate::platform::now_ms(),
            packet_counter: AtomicU16::new(first_number),
            received_counter: AtomicU16::new(0),
            connected: AtomicBool::new(true),
            link,
            shutdown,
            raw: broadcast::channel(16_384).0,
            status: StdMutex::new(StatusTracker::new()),
            seen: StdMutex::new(std::collections::HashMap::new()),
        }
    }

    fn next_number(&self) -> u16 {
        self.packet_counter.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn packet(&self, tag: &str, payload: &[String]) -> String {
        let ms = (crate::platform::now_ms() - self.started_ms).max(0.0) as u32;
        protocol::encode(tag, self.next_number(), ms, payload)
    }

    /// Publish a received packet: raw tap, typed event, and a `Status` event if it changed
    /// the device status. Returns false when the event receiver has gone away.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) async fn dispatch(&self, tx: &mpsc::Sender<EmotiBitEvent>, p: Packet, received_at: f64) -> bool {
        // The device sends some packets (the `EM` mode packet, about ten a second) over both
        // UDP and the TCP control channel. Drop the second copy so recordings and counters
        // see each packet once.
        {
            let mut seen = self.seen.lock().expect("dedupe mutex poisoned");
            let key = (p.timestamp, p.number);
            if seen.get(&p.tag) == Some(&key) {
                return true;
            }
            seen.insert(p.tag.clone(), key);
        }
        // No subscribers is the normal case; ignore the error.
        let _ = self.raw.send(Arc::from(p.to_line()));
        let event = to_event(p, received_at);
        let changed = {
            let mut st = self.status.lock().expect("status mutex poisoned");
            match &event {
                EmotiBitEvent::Sensor(r) => st.update_reading(r),
                EmotiBitEvent::Message(m) => st.update_packet(m),
                _ => false,
            }
            .then(|| st.status().clone())
        };
        if tx.send(event).await.is_err() {
            return false;
        }
        if let Some(status) = changed {
            return tx.send(EmotiBitEvent::Status(status)).await.is_ok();
        }
        true
    }
}

/// Commands to a connected device.
#[derive(Clone)]
pub struct EmotiBitHandle {
    pub(crate) inner: Arc<Inner>,
}

/// `2025-11-05_17-37-57-123456`, the local-time format EmotiBit uses in `TL`/`RB`/`RE` packets.
pub fn local_timestamp() -> String {
    crate::platform::local_timestamp()
}

impl EmotiBitHandle {
    /// The device this handle talks to.
    pub fn device(&self) -> &EmotiBitDevice {
        &self.inner.device
    }

    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Relaxed)
    }

    /// Send a control packet: over the TCP control channel (WiFi) or the control
    /// characteristic (BLE).
    pub async fn send_control(&self, tag: &str, payload: &[String]) -> Result<()> {
        let wire = self.inner.packet(tag, payload);
        match &self.inner.link {
            #[cfg(not(target_arch = "wasm32"))]
            Link::Wifi { control_writer, .. } => {
                use tokio::io::AsyncWriteExt;
                let mut guard = control_writer.lock().await;
                let writer = guard.as_mut().ok_or_else(|| anyhow!("control channel is not up yet"))?;
                writer.write_all(wire.as_bytes()).await?;
            }
            Link::Ble { control } => control.send(wire).map_err(|_| anyhow!("BLE link is closed"))?,
        }
        Ok(())
    }

    /// Start recording to the SD card in the BLE firmware's binary format (`.EBB`, about 3x
    /// smaller; `emotibit parse` converts it). The stock firmware ignores the format and
    /// writes CSV.
    pub async fn record_begin_binary(&self) -> Result<()> {
        self.send_control(tag::RECORD_BEGIN, &[local_timestamp(), "FMT".into(), "BIN".into()]).await
    }

    /// Start recording to the device's SD card.
    pub async fn record_begin(&self) -> Result<()> {
        self.send_control(tag::RECORD_BEGIN, &[local_timestamp()]).await
    }

    /// Stop recording.
    pub async fn record_end(&self) -> Result<()> {
        self.send_control(tag::RECORD_END, &[local_timestamp()]).await
    }

    /// Add a note to the recording.
    pub async fn user_note(&self, note: &str) -> Result<()> {
        self.send_control(tag::USER_NOTE, &[local_timestamp(), note.replace(',', ";")]).await
    }

    /// Change power mode: one of the `tag::MODE_*` constants.
    pub async fn set_mode(&self, mode_tag: &str) -> Result<()> {
        self.send_control(mode_tag, &[local_timestamp()]).await
    }

    /// Change power mode. `Hibernate` powers the device down: it will stop streaming
    /// and has to be woken with its button.
    pub async fn set_power_mode(&self, mode: PowerMode) -> Result<()> {
        self.set_mode(mode.tag()).await
    }

    /// The latest device status (battery, recording, power mode, error counters).
    pub fn status(&self) -> DeviceStatus {
        self.inner.status.lock().expect("status mutex poisoned").status().clone()
    }

    /// Resolves when the connection has ended (disconnect, timeout or device gone).
    pub async fn closed(&self) {
        let mut rx = self.inner.shutdown.subscribe();
        // `changed` errors only if the sender is gone, which also means closed.
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// Subscribe to every packet received from the device, one line each, in the device's
    /// SD-card format. Slow subscribers lose the oldest lines rather than blocking the stream.
    pub fn subscribe_raw(&self) -> broadcast::Receiver<Arc<str>> {
        self.inner.raw.subscribe()
    }

    /// Tell the device to disconnect and stop all tasks. Idempotent.
    pub async fn disconnect(&self) -> Result<()> {
        let was = self.inner.connected.swap(false, Ordering::Relaxed);
        // The stock firmware is told to let go; a BLE board sees the link close.
        #[cfg(not(target_arch = "wasm32"))]
        if was && let Link::Wifi { adv, .. } = &self.inner.link {
            let _ = self.send_control(tag::EMOTIBIT_DISCONNECT, &[]).await;
            let wire = self.inner.packet(tag::EMOTIBIT_DISCONNECT, &[]);
            let _ = adv.send_to(wire.as_bytes(), (self.inner.device.ip, crate::protocol::ADVERTISING_PORT)).await;
        }
        #[cfg(target_arch = "wasm32")]
        let _ = was;
        let _ = self.inner.shutdown.send(true);
        Ok(())
    }
}

/// Hooks for the BLE link task (`crate::ble`), which feeds the same event pipeline.
#[cfg_attr(not(feature = "ble"), allow(dead_code))]
impl EmotiBitHandle {
    /// A WiFi link (`emotibit_client::establish`); packet numbers continue from the handshake.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn new_wifi(device: EmotiBitDevice, link: Link, first_number: u16) -> Self {
        Self { inner: Arc::new(Inner::new(device, link, first_number)) }
    }

    pub(crate) fn new_ble(device: EmotiBitDevice, control: mpsc::UnboundedSender<String>) -> Self {
        Self { inner: Arc::new(Inner::new(device, Link::Ble { control }, 0)) }
    }

    /// A packet from the device (status, time sync). Recorders get it as the device numbered
    /// it (`AK` refers to the `RD` number); the event gets the host's running number, which
    /// sensor readings share, so gap counting sees one sequence. False when the receiver is gone.
    pub(crate) async fn dispatch_packet(&self, tx: &mpsc::Sender<EmotiBitEvent>, mut p: Packet, received_at: f64) -> bool {
        let _ = self.inner.raw.send(Arc::from(p.to_line()));
        p.number = self.next_number();
        let event = to_event(p, received_at);
        let changed = {
            let mut st = self.inner.status.lock().expect("status mutex poisoned");
            match &event {
                EmotiBitEvent::Sensor(r) => st.update_reading(r),
                EmotiBitEvent::Message(m) => st.update_packet(m),
                _ => false,
            }
            .then(|| st.status().clone())
        };
        if tx.send(event).await.is_err() {
            return false;
        }
        match changed {
            Some(status) => tx.send(EmotiBitEvent::Status(status)).await.is_ok(),
            None => true,
        }
    }

    /// A converted sensor reading. `raw_line` (a stock packet line) goes to recorders when the
    /// device clock is known. False when the receiver is gone.
    pub(crate) async fn dispatch_reading(&self, tx: &mpsc::Sender<EmotiBitEvent>, r: crate::types::SensorReading, raw_line: Option<String>) -> bool {
        if let Some(line) = raw_line {
            let _ = self.inner.raw.send(Arc::from(line));
        }
        let changed = {
            let mut st = self.inner.status.lock().expect("status mutex poisoned");
            st.update_reading(&r).then(|| st.status().clone())
        };
        if tx.send(EmotiBitEvent::Sensor(r)).await.is_err() {
            return false;
        }
        match changed {
            Some(status) => tx.send(EmotiBitEvent::Status(status)).await.is_ok(),
            None => true,
        }
    }

    /// The next number for a received packet or reading (BLE).
    pub(crate) fn next_number(&self) -> u16 {
        self.inner.received_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Ends the link: the BLE task stops and reports `Disconnected`.
    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.inner.shutdown.subscribe()
    }

    pub(crate) fn mark_disconnected(&self) -> bool {
        let was = self.inner.connected.swap(false, Ordering::Relaxed);
        let _ = self.inner.shutdown.send(true);
        was
    }
}

