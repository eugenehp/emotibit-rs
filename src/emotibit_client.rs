//! Discovery, connection and streaming for EmotiBit over WiFi, and (with the `ble` feature)
//! for boards running the custom BLE firmware, behind the same [`EmotiBitHandle`].
//!
//! The shape mirrors `muse-rs`: build a [`EmotiBitClient`], [`scan_all`](EmotiBitClient::scan_all)
//! or [`connect`](EmotiBitClient::connect), and read [`EmotiBitEvent`]s from the returned channel
//! while an [`EmotiBitHandle`] issues commands. See [`crate::protocol`] for the wire model.
//!
//! Only one host can be connected to a device at a time: close the EmotiBit Oscilloscope
//! before connecting from here, or the device reports itself unavailable.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use log::{debug, warn};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, timeout};

pub use crate::handle::{local_timestamp, EmotiBitHandle};
use crate::handle::{Inner, Link};
use crate::parse::now_ms;
use crate::protocol::{
    self, label, tag, Packet, ADVERTISING_PORT, CONNECTION_TIMEOUT_MS, CONNECT_INTERVAL_MS,
    CONNECT_TIMEOUT_MS, EMOTIBIT_AVAILABLE, PING_INTERVAL_MS,
};
use crate::types::{EmotiBitDevice, EmotiBitEvent};

/// First data port tried; the reference host uses `ADVERTISING_PORT + 1` and steps by 2.
const FIRST_DATA_PORT: u16 = ADVERTISING_PORT + 1;

/// Client configuration.
#[derive(Debug, Clone)]
pub struct EmotiBitClientConfig {
    /// How long [`EmotiBitClient::scan_all`] listens, and the limit for [`EmotiBitClient::connect`]
    /// to find an available device.
    pub scan_timeout: Duration,
    /// Probe every address of each local /24 in addition to broadcasting. Some networks drop
    /// broadcasts between WiFi clients; the unicast sweep still finds the device.
    pub unicast_sweep: bool,
    /// Skip discovery's network walk and ask this one address.
    pub target_ip: Option<IpAddr>,
    /// Restrict to this device id (e.g. `MD-V6-0000356`, `EmotiBit-BLE-E848`) or IP when connecting.
    pub device: Option<String>,
    /// Which links to look for devices on.
    pub transport: Transport,
    /// BLE wire format: compact (delta + varint) instead of raw 20-byte records.
    pub ble_compact: bool,
}

/// Which links [`EmotiBitClient`] looks for devices on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    /// Stock firmware over WiFi.
    #[default]
    Wifi,
    /// The custom BLE firmware (`ble` feature).
    Ble,
    /// Both at once.
    Both,
}

impl Transport {
    fn wifi(self) -> bool {
        self != Transport::Ble
    }
    #[cfg_attr(not(all(feature = "ble", not(target_arch = "wasm32"))), allow(dead_code))]
    fn ble(self) -> bool {
        self != Transport::Wifi
    }
}

impl Default for EmotiBitClientConfig {
    fn default() -> Self {
        Self {
            scan_timeout: Duration::from_secs(5),
            unicast_sweep: true,
            target_ip: None,
            device: None,
            transport: Transport::Wifi,
            ble_compact: true,
        }
    }
}

/// Entry point: discovers and connects to EmotiBit devices.
pub struct EmotiBitClient {
    config: EmotiBitClientConfig,
}

impl EmotiBitClient {
    pub fn new(config: EmotiBitClientConfig) -> Self {
        Self { config }
    }

    /// Listen for [`EmotiBitClientConfig::scan_timeout`] and return every device that answered
    /// (WiFi) or advertised (BLE), depending on [`EmotiBitClientConfig::transport`].
    pub async fn scan_all(&self) -> Result<Vec<EmotiBitDevice>> {
        let mut found = Vec::new();
        #[cfg(all(feature = "ble", not(target_arch = "wasm32")))]
        let ble = async {
            if self.config.transport.ble() {
                crate::ble::scan(self.config.scan_timeout).await.map(|v| v.into_iter().map(|(d, _)| d).collect())
            } else {
                Ok(Vec::new())
            }
        };
        #[cfg(not(all(feature = "ble", not(target_arch = "wasm32"))))]
        let ble = async { anyhow::Ok(Vec::<EmotiBitDevice>::new()) };
        let wifi = async {
            if self.config.transport.wifi() {
                discover(&self.config, false).await
            } else {
                Ok(Vec::new())
            }
        };
        let (wifi, ble) = tokio::join!(wifi, ble);
        found.extend(wifi?);
        found.extend(ble?);
        Ok(found)
    }

    fn wanted(&self, d: &EmotiBitDevice) -> bool {
        self.config.device.as_deref().is_none_or(|w| d.id == w || d.ip.to_string() == w)
    }

    /// Scan, pick the first available device (matching `config.device` if set) and connect.
    pub async fn connect(&self) -> Result<(mpsc::Receiver<EmotiBitEvent>, EmotiBitHandle)> {
        if self.config.transport == Transport::Wifi {
            let found = discover(&self.config, true).await?;
            let device = found
                .into_iter()
                .find(|d| self.wanted(d))
                .ok_or_else(|| anyhow!("no EmotiBit found; is it on the same 2.4 GHz network?"))?;
            return self.connect_to(device).await;
        }
        let device = self
            .scan_all()
            .await?
            .into_iter()
            .find(|d| d.available && self.wanted(d))
            .ok_or_else(|| anyhow!("no EmotiBit found (transport {:?})", self.config.transport))?;
        self.connect_to(device).await
    }

    /// Connect to **every** available device found (restricted by `config.device` if set).
    /// Each device gets its own event channel and handle.
    pub async fn connect_all(&self) -> Result<Vec<(mpsc::Receiver<EmotiBitEvent>, EmotiBitHandle)>> {
        let devices: Vec<EmotiBitDevice> =
            self.scan_all().await?.into_iter().filter(|d| d.available && self.wanted(d)).collect();
        if devices.is_empty() {
            bail!("no available EmotiBit found (transport {:?})", self.config.transport);
        }
        let mut out = Vec::new();
        for d in devices {
            out.push(self.connect_to(d).await?);
        }
        Ok(out)
    }

    /// Connect to a device returned by [`scan_all`](Self::scan_all).
    pub async fn connect_to(
        &self,
        device: EmotiBitDevice,
    ) -> Result<(mpsc::Receiver<EmotiBitEvent>, EmotiBitHandle)> {
        if !device.available {
            bail!("{} is connected to another host (close the Oscilloscope?)", device.id);
        }
        #[cfg(all(feature = "ble", not(target_arch = "wasm32")))]
        if device.is_ble() {
            return crate::ble::establish(&device, self.config.ble_compact, self.config.scan_timeout).await;
        }
        establish(device).await
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// IPv4 broadcast addresses and /24 prefixes of this machine's interfaces.
fn local_networks() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let mut nets: Vec<(Ipv4Addr, Ipv4Addr)> = Vec::new();
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if v4.ip.is_loopback() || v4.ip.is_link_local() {
                continue;
            }
            let o = v4.ip.octets();
            let prefix = Ipv4Addr::new(o[0], o[1], o[2], 0);
            // Two interfaces on one subnet (wired + WiFi) would double every probe.
            if nets.iter().all(|(p, _)| *p != prefix) {
                nets.push((prefix, v4.broadcast.unwrap_or(Ipv4Addr::new(o[0], o[1], o[2], 255))));
            }
        }
    }
    nets
}

async fn discover(config: &EmotiBitClientConfig, stop_on_available: bool) -> Result<Vec<EmotiBitDevice>> {
    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await.context("cannot bind discovery socket")?);
    socket.set_broadcast(true)?;

    let nets = local_networks();
    if nets.is_empty() && config.target_ip.is_none() {
        bail!("no usable IPv4 network interface (is WiFi/Ethernet connected?)");
    }

    let sender = {
        let socket = socket.clone();
        let target = config.target_ip;
        let sweep = config.unicast_sweep;
        let start = Instant::now();
        tokio::spawn(async move {
            let mut counter: u16 = 0;
            let hello = |c: &mut u16| {
                let p = protocol::encode(tag::HELLO_EMOTIBIT, *c, start.elapsed().as_millis() as u32, &[]);
                *c = c.wrapping_add(1);
                p
            };
            let mut first = true;
            loop {
                if let Some(ip) = target {
                    let _ = socket.send_to(hello(&mut counter).as_bytes(), (ip, ADVERTISING_PORT)).await;
                } else {
                    for (_, bcast) in &nets {
                        let _ = socket.send_to(hello(&mut counter).as_bytes(), (*bcast, ADVERTISING_PORT)).await;
                    }
                    if sweep && first {
                        for (prefix, _) in &nets {
                            let o = prefix.octets();
                            for host in 1..=254u8 {
                                let ip = Ipv4Addr::new(o[0], o[1], o[2], host);
                                let _ = socket.send_to(hello(&mut counter).as_bytes(), (ip, ADVERTISING_PORT)).await;
                                sleep(Duration::from_millis(2)).await;
                            }
                        }
                    }
                }
                first = false;
                sleep(Duration::from_secs(1)).await;
            }
        })
    };

    let mut found: Vec<EmotiBitDevice> = Vec::new();
    let deadline = Instant::now() + config.scan_timeout;
    let mut buf = vec![0u8; 4096];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(Ok((n, from))) = timeout(left, socket.recv_from(&mut buf)).await else { break };
        let text = String::from_utf8_lossy(&buf[..n]);
        for packet in protocol::split_message(&text).into_iter().flatten() {
            if packet.tag != tag::HELLO_HOST {
                continue;
            }
            let Some(dp) = packet.keyed(label::DATA_PORT).and_then(|v| v.parse::<i32>().ok()) else {
                continue;
            };
            let device = EmotiBitDevice {
                id: packet.keyed(label::DEVICE_ID).map(str::to_string).unwrap_or_else(|| from.ip().to_string()),
                ip: from.ip(),
                available: dp == EMOTIBIT_AVAILABLE,
            };
            match found.iter_mut().find(|d| d.id == device.id) {
                Some(existing) => *existing = device.clone(),
                None => found.push(device.clone()),
            }
            let wanted = config.device.as_deref().is_none_or(|w| device.id == w || device.ip.to_string() == w);
            if stop_on_available && device.available && wanted {
                sender.abort();
                return Ok(found);
            }
        }
    }
    sender.abort();
    Ok(found)
}

// ── Connection ────────────────────────────────────────────────────────────────

async fn bind_data_port() -> Result<(UdpSocket, u16)> {
    let mut port = FIRST_DATA_PORT;
    loop {
        match UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(s) => return Ok((s, port)),
            Err(_) if port < FIRST_DATA_PORT + 200 => port += 2,
            Err(e) => return Err(e).context("no free UDP data port"),
        }
    }
}

async fn bind_control_port(start: u16) -> Result<(TcpListener, u16)> {
    let mut port = start;
    loop {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => return Ok((l, port)),
            Err(_) if port < start + 200 => port += 2,
            Err(e) => return Err(e).context("no free TCP control port"),
        }
    }
}

async fn establish(device: EmotiBitDevice) -> Result<(mpsc::Receiver<EmotiBitEvent>, EmotiBitHandle)> {
    let (data, data_port) = bind_data_port().await?;
    let (listener, control_port) = bind_control_port(data_port + 1).await?;
    let data = Arc::new(data);
    let adv = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let device_addr = SocketAddr::new(device.ip, ADVERTISING_PORT);
    let started = Instant::now();
    let mut counter: u16 = 0;

    // Handshake: repeat EC until the device PONGs our data port.
    let payload: Vec<String> = vec![
        label::CONTROL_PORT.into(),
        control_port.to_string(),
        label::DATA_PORT.into(),
        data_port.to_string(),
    ];
    let deadline = Instant::now() + Duration::from_millis(CONNECT_TIMEOUT_MS);
    let mut buf = vec![0u8; 4096];
    'handshake: loop {
        if Instant::now() >= deadline {
            bail!("{} did not accept the connection within {} s", device.id, CONNECT_TIMEOUT_MS / 1000);
        }
        let wire = protocol::encode(tag::EMOTIBIT_CONNECT, counter, started.elapsed().as_millis() as u32, &payload);
        counter = counter.wrapping_add(1);
        adv.send_to(wire.as_bytes(), device_addr).await?;

        let wait = Duration::from_millis(CONNECT_INTERVAL_MS);
        if let Ok(Ok((n, from))) = timeout(wait, adv.recv_from(&mut buf)).await {
            if from.ip() != device.ip {
                continue;
            }
            for p in protocol::split_message(&String::from_utf8_lossy(&buf[..n])).into_iter().flatten() {
                let ours = p.keyed(label::DATA_PORT).and_then(|v| v.parse::<u16>().ok()) == Some(data_port);
                if p.tag == tag::PONG && ours {
                    break 'handshake;
                }
            }
        }
    }
    debug!("connected to {} (data {data_port}, control {control_port})", device.id);

    let handle = EmotiBitHandle::new_wifi(device.clone(), Link::Wifi { control_writer: Mutex::new(None), adv: adv.clone() }, counter);
    let inner = handle.inner.clone();
    let (tx, rx) = mpsc::channel::<EmotiBitEvent>(8192);
    let _ = tx.send(EmotiBitEvent::Connected(device.clone())).await;

    // PING + PONG watchdog.
    {
        let inner = inner.clone();
        let adv = adv.clone();
        let tx = tx.clone();
        let mut stop = inner.shutdown.subscribe();
        tokio::spawn(async move {
            let mut last_pong = Instant::now();
            let mut buf = vec![0u8; 4096];
            let mut next_ping = Instant::now();
            loop {
                if Instant::now() >= next_ping {
                    next_ping = Instant::now() + Duration::from_millis(PING_INTERVAL_MS);
                    let wire = inner.packet(tag::PING, &[label::DATA_PORT.into(), data_port.to_string()]);
                    let _ = adv.send_to(wire.as_bytes(), device_addr).await;
                }
                let wait = next_ping.saturating_duration_since(Instant::now());
                tokio::select! {
                    _ = stop.changed() => break,
                    r = timeout(wait, adv.recv_from(&mut buf)) => {
                        if let Ok(Ok((n, from))) = r
                            && from.ip() == inner.device.ip {
                                let text = String::from_utf8_lossy(&buf[..n]);
                                if protocol::split_message(&text).into_iter().flatten().any(|p| p.tag == tag::PONG) {
                                    last_pong = Instant::now();
                                }
                            }
                    }
                }
                if last_pong.elapsed() > Duration::from_millis(CONNECTION_TIMEOUT_MS) {
                    warn!("no PONG from {} for {} s", inner.device.id, CONNECTION_TIMEOUT_MS / 1000);
                    break;
                }
            }
            if inner.connected.swap(false, Ordering::Relaxed) {
                let _ = tx.send(EmotiBitEvent::Disconnected).await;
            }
            let _ = inner.shutdown.send(true);
        });
    }

    // UDP data channel.
    {
        let inner = inner.clone();
        let tx = tx.clone();
        let data = data.clone();
        let mut stop = inner.shutdown.subscribe();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                let (n, from) = tokio::select! {
                    _ = stop.changed() => break,
                    r = data.recv_from(&mut buf) => match r { Ok(v) => v, Err(_) => break },
                };
                if from.ip() != inner.device.ip {
                    continue;
                }
                let received_at = now_ms();
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                for packet in protocol::split_message(&text) {
                    match packet {
                        Ok(p) => {
                            if p.tag == tag::REQUEST_DATA {
                                answer_request(&inner, &data, from, &p).await;
                            }
                            if !inner.dispatch(&tx, p, received_at).await {
                                return;
                            }
                        }
                        Err(e) => debug!("dropped malformed packet: {e}"),
                    }
                }
            }
        });
    }

    // TCP control channel: the device connects to us.
    {
        let inner = inner.clone();
        let tx = tx.clone();
        let mut stop = inner.shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                let (stream, from) = tokio::select! {
                    _ = stop.changed() => break,
                    r = listener.accept() => match r { Ok(v) => v, Err(_) => break },
                };
                if from.ip() != inner.device.ip {
                    continue;
                }
                let (read, write) = stream.into_split();
                let Link::Wifi { control_writer, .. } = &inner.link else { unreachable!("WiFi task on a BLE link") };
                *control_writer.lock().await = Some(write);
                let mut lines = BufReader::new(read).lines();
                loop {
                    let line = tokio::select! {
                        _ = stop.changed() => break,
                        l = lines.next_line() => match l { Ok(Some(l)) => l, _ => break },
                    };
                    let received_at = now_ms();
                    if let Ok(p) = Packet::parse(&line)
                        && !inner.dispatch(&tx, p, received_at).await {
                            return;
                        }
                }
                *control_writer.lock().await = None;
            }
        });
    }

    Ok((rx, handle))
}

/// Answer a device's `RD` (request data): it asks for the host's local time so it can
/// align clocks. Reply with `TL`, then acknowledge.
async fn answer_request(inner: &Inner, data: &UdpSocket, to: SocketAddr, request: &Packet) {
    if request.payload.iter().any(|e| e == tag::TIMESTAMP_LOCAL) {
        let wire = inner.packet(tag::TIMESTAMP_LOCAL, &[local_timestamp()]);
        let _ = data.send_to(wire.as_bytes(), to).await;
    }
    let ack = inner.packet(tag::ACK, &[request.number.to_string(), request.tag.clone()]);
    let _ = data.send_to(ack.as_bytes(), to).await;
}
