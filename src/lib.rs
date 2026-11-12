//! # emotibit
//!
//! Async Rust client for [EmotiBit](https://www.emotibit.com/) boards, with either firmware: the
//! **stock firmware** over **WiFi** (UDP discovery, a short handshake, then CSV packets of
//! batched samples; see [`protocol`]), or **`emotibit-ble`**, this project's firmware, over
//! **Bluetooth LE** (see [`ble`]). Both arrive as the same [`types::EmotiBitEvent`]s behind one
//! handle, with everything the official Oscilloscope and DataParser do; [`EmotiBitClientConfig`]'s
//! `transport` picks where to look.
//!
//! [`EmotiBitClientConfig`]: emotibit_client::EmotiBitClientConfig
//!
//! ## Quick start
//!
//! ```no_run
//! use emotibit::prelude::*;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let client = EmotiBitClient::new(EmotiBitClientConfig::default());
//!     let (mut rx, handle) = client.connect().await?;
//!
//!     while let Some(event) = rx.recv().await {
//!         match event {
//!             EmotiBitEvent::Sensor(r) => println!("{}: {:?}", r.channel.name, r.samples),
//!             EmotiBitEvent::Status(s) => println!("battery {:?}%", s.battery_percent),
//!             EmotiBitEvent::Disconnected => break,
//!             _ => {}
//!         }
//!     }
//!     handle.disconnect().await?;
//!     Ok(())
//! }
//! ```
//!
//! Only one host can hold a device at a time, so close the EmotiBit Oscilloscope first. The
//! command-line client, TUI and firmware flasher built on this are described in the
//! [README](https://github.com/eugenehp/emotibit-rs#readme).
//!
//! ## Portable core vs native
//!
//! The first group builds for `wasm32-unknown-unknown` and powers the browser UI; the second
//! needs sockets, files or native libraries and is native-only.
//!
//! | Module | Purpose |
//! |---|---|
//! | [`protocol`] | Packet format, type tags, ports and timing constants |
//! | [`types`] | Events, readings, devices and the channel table |
//! | [`parse`] | Packets → typed events |
//! | [`status`] | Battery, recording, power mode, error counters |
//! | [`health`] | Measured rates, lost packets, underruns |
//! | [`periodizer`] | Aperiodic metrics (HR, SCR) → evenly sampled series |
//! | [`timesync`] | Device clock → computer clock |
//! | [`scope`] | Rolling buffers, windows, autoscale, pause |
//! | [`ble`] | GATT layout and packet decoder for the custom BLE firmware; the `webbluetooth` client with `ble` (native and browser) |
//! | [`handle`] | A connected device behind one handle, over WiFi or BLE |
//! | [`platform`] | Spawning, timers and the clock, per target (tokio natively, the page's event loop in a browser) |
//! | [`derive`](mod@derive) | Heart rate / IBI from PPG and SCR from EDA, for raw-only streams |
//! | [`serial`] | USB-serial debug-mode handshake and values (portable state machine) |
//! | `emotibit_client` | Discovery, connection, [`emotibit_client::EmotiBitHandle`] commands |
//! | `datafile` | Raw logging and DataParser-compatible CSV conversion |
//! | `output` | UDP / OSC / LSL outputs and LSL marker input |
//! | `remote` | JSON remote-control server (UDP 3130) |
//! | `bridge` | WebSocket bridge for the browser UI |
//! | `tui` | Terminal oscilloscope |
//! | `flasher` | Firmware over USB: info, backup, restore, stock, emotibit-ble (built in), any image (`flash`) |

extern crate alloc;

pub mod ble;
/// Heart rate and SCR detectors, shared verbatim with the firmware (it runs them for SD files).
#[path = "shared/derive.rs"]
pub mod derive;
pub mod health;
pub mod parse;
pub mod periodizer;
#[cfg(any(not(target_arch = "wasm32"), feature = "ble"))]
pub mod platform;
pub mod protocol;
pub mod scope;
pub mod serial;
pub mod status;
pub mod timesync;
pub mod types;

#[cfg(not(target_arch = "wasm32"))]
pub mod datafile;
#[cfg(not(target_arch = "wasm32"))]
pub mod emotibit_client;
#[cfg(any(not(target_arch = "wasm32"), feature = "ble"))]
pub mod handle;
#[cfg(not(target_arch = "wasm32"))]
pub mod output;
#[cfg(not(target_arch = "wasm32"))]
pub mod remote;
#[cfg(all(not(target_arch = "wasm32"), feature = "ws"))]
pub mod bridge;
#[cfg(all(not(target_arch = "wasm32"), feature = "ws"))]
pub mod webui;
#[cfg(all(not(target_arch = "wasm32"), feature = "tui"))]
pub mod tui;
#[cfg(all(not(target_arch = "wasm32"), feature = "flash"))]
pub mod flasher;

#[cfg(all(target_arch = "wasm32", feature = "webscope"))]
pub mod web;

/// Convenience re-exports for downstream crates.
pub mod prelude {
    pub use crate::health::{ChannelHealth, StreamHealth};
    pub use crate::periodizer::Periodizer;
    pub use crate::protocol::{tag, Packet};
    pub use crate::scope::Scope;
    pub use crate::status::{DeviceStatus, PowerMode, StatusTracker};
    pub use crate::timesync::{Anchors, TimeSync};
    pub use crate::types::{channel, Channel, EmotiBitDevice, EmotiBitEvent, SensorReading, CHANNELS};

    #[cfg(not(target_arch = "wasm32"))]
    pub use crate::emotibit_client::{EmotiBitClient, EmotiBitClientConfig, EmotiBitHandle, Transport};
}
