//! Command-line client for EmotiBit.
//!
//! | command | what it does |
//! |---|---|
//! | `scan` | list devices on the network |
//! | `info` | connect briefly, report channels, rates, status, lost packets, clock sync |
//! | `stream` | connect and stream (summary by default, `--raw` per sample); outputs; recording |
//! | `tui` | terminal oscilloscope |
//! | `parse FILE` | convert a raw recording (live log or SD card file) to per-signal CSVs |
//! | `serve` | headless daemon: web bridge, remote-control port, outputs, recording |
//! | `power MODE` | set the device's power mode |
//! | `ble`, `sd`, `selftest`, `config` | the emotibit-ble firmware: link report, SD card, self-test, settings |
//! | `serial` | one signal over USB serial (the stock firmware's debug mode) |
//! | `firmware …` | over USB: info, backup, restore, install stock / emotibit-ble / any image |
//!
//! Every command that connects accepts `--all` to use every available device at once.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Parser, Subcommand};
use emotibit::datafile;
use emotibit::output::{OutputConfig, Outputs};
use emotibit::prelude::*;
use emotibit::remote::{self, RemoteCommand};
use tokio::sync::mpsc::Receiver;

#[derive(Parser)]
#[command(name = "emotibit", version, about = "Stream from EmotiBit: over WiFi (stock firmware) or Bluetooth (the emotibit-ble firmware)")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Connect to this device id (e.g. MD-V6-0000356) or IP address
    #[arg(long, global = true)]
    device: Option<String>,

    /// Ask this IP address directly instead of scanning the network
    #[arg(long, global = true)]
    ip: Option<IpAddr>,

    /// Seconds to scan for devices
    #[arg(long, default_value_t = 5, global = true)]
    timeout: u64,

    /// Use every available device, not just the first
    #[arg(long, global = true)]
    all: bool,

    /// Where to look for devices: wifi (stock firmware), ble (emotibit-ble firmware) or both
    #[arg(long, global = true, value_parser = ["wifi", "ble", "both"], default_value = "wifi")]
    transport: String,

    /// BLE wire format: compact (delta + varint, about 4x less airtime) or raw (20-byte records)
    #[arg(long, global = true, value_parser = ["compact", "raw"], default_value = "compact")]
    ble_format: String,

    /// Gzip recordings: the raw log (`--record-dir`) and the per-signal CSVs (`.csv.gz`, about a
    /// sixth of the size). `parse` reads gzipped files either way
    #[arg(long, global = true)]
    gzip: bool,
}

/// `--gzip`, for every recording this run makes.
static GZIP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn gzip() -> bool {
    GZIP.load(std::sync::atomic::Ordering::Relaxed)
}

/// A new raw log's name in `dir`: `<device>_<date>.csv`, or `.csv.gz` with `--gzip`.
fn raw_log_path(dir: &Path, device: &str) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    dir.join(format!("{device}_{stamp}.csv{}", if gzip() { ".gz" } else { "" }))
}

/// Where to send the data besides the screen.
#[derive(ClapArgs, Clone, Default)]
struct OutputArgs {
    /// Forward every raw packet as a UDP datagram to HOST:PORT
    #[arg(long, value_name = "HOST:PORT")]
    udp: Option<SocketAddr>,
    /// Send OSC messages to HOST:PORT (address /<device>/<channel>, one float per sample)
    #[arg(long, value_name = "HOST:PORT")]
    osc: Option<SocketAddr>,
    /// JSON file mapping tag -> [OSC addresses], overriding the default addresses
    #[arg(long, value_name = "FILE", requires = "osc")]
    osc_map: Option<PathBuf>,
    /// Publish the signals as Lab Streaming Layer streams (source id = device id)
    #[arg(long)]
    lsl: bool,
    /// Forward an LSL marker stream to the device; NAME or NAME:SOURCE_ID
    #[arg(long, value_name = "NAME[:SOURCE_ID]")]
    lsl_marker: Option<String>,
}

impl OutputArgs {
    fn config(&self) -> OutputConfig {
        OutputConfig {
            udp: self.udp,
            osc: self.osc,
            osc_map: self.osc_map.clone(),
            lsl: self.lsl,
            lsl_marker: self.lsl_marker.as_ref().map(|s| match s.split_once(':') {
                Some((n, id)) => (n.to_string(), Some(id.to_string())),
                None => (s.clone(), None),
            }),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// List the EmotiBit devices on the network, then exit
    Scan,
    /// Connect, report what the device streams for a few seconds, then exit
    Info {
        /// Seconds to listen
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// Connect and stream (the default)
    Stream {
        /// Print every sample instead of a summary
        #[arg(long)]
        raw: bool,
        /// Stop after this many seconds (default: until Ctrl-C)
        #[arg(long)]
        seconds: Option<u64>,
        /// Also record to the device's SD card for the duration of the stream
        #[arg(long)]
        record: bool,
        /// SD file format on BLE boards: csv (stock lines) or bin (raw records, ~3x smaller)
        #[arg(long, value_parser = ["csv", "bin"], default_value = "csv")]
        sd_format: String,
        /// Save a raw log here and convert it to per-signal CSVs when the stream ends
        #[arg(long, value_name = "DIR")]
        record_dir: Option<PathBuf>,
        #[command(flatten)]
        outputs: OutputArgs,
    },
    /// Terminal oscilloscope
    Tui {
        #[command(flatten)]
        outputs: OutputArgs,
    },
    /// Convert a raw recording into one CSV per signal, with local timestamps
    Parse {
        /// A raw file: a live log from `--record-dir`, or a file copied off the SD card
        file: PathBuf,
        /// Where to write the CSVs (default: next to the input)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Run as a daemon: web bridge, remote-control port, outputs, recording
    Serve {
        /// Serve the browser UI's WebSocket bridge on this address (e.g. 127.0.0.1:8765)
        #[arg(long, value_name = "ADDR")]
        ws: Option<SocketAddr>,
        /// Also serve the web UI from this directory on the same port (see scripts/build-web.sh)
        #[arg(long, value_name = "DIR", requires = "ws")]
        web: Option<PathBuf>,
        /// Allow a statically hosted page at this origin to control the device (repeatable)
        #[arg(long, value_name = "ORIGIN", requires = "ws")]
        allow_origin: Vec<String>,
        /// Listen for the Oscilloscope's JSON remote-control messages (UDP, default port 3130)
        #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "3130")]
        remote: Option<u16>,
        /// Save a raw log here and convert it when the connection ends
        #[arg(long, value_name = "DIR")]
        record_dir: Option<PathBuf>,
        #[command(flatten)]
        outputs: OutputArgs,
    },
    /// Start the browser oscilloscope: bridge + web page, then open your browser
    Web {
        /// Port to serve on; if taken, the next free one is used
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Address to bind (default 127.0.0.1; anything else exposes device control to the network)
        #[arg(long, value_name = "IP")]
        bind: Option<IpAddr>,
        /// Do not open a browser window
        #[arg(long)]
        no_open: bool,
        /// Serve the UI from this directory instead of the copy built into the binary
        #[arg(long, value_name = "DIR")]
        web: Option<PathBuf>,
        /// Allow a statically hosted page at this origin to control the device (repeatable)
        #[arg(long, value_name = "ORIGIN")]
        allow_origin: Vec<String>,
        /// Also listen for the Oscilloscope's JSON remote-control messages (UDP, default 3130)
        #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "3130")]
        remote: Option<u16>,
        /// Save a raw log here and convert it when the connection ends
        #[arg(long, value_name = "DIR")]
        record_dir: Option<PathBuf>,
        #[command(flatten)]
        outputs: OutputArgs,
    },
    /// Read one signal over USB serial (the firmware's debug mode); no WiFi needed
    Serial {
        /// Serial port (auto-detected: the CP210x Feather)
        #[arg(long)]
        port: Option<String>,
        /// Signal: eda, thermopile, temp0, temp1, accel-x, ppg-red, battery
        #[arg(long, default_value = "eda")]
        signal: String,
        /// How long to read
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// Baud rate. The stock firmware uses 2000000 (the ROM boot log is 115200 and shows as noise)
        #[arg(long, default_value_t = 2_000_000)]
        baud: u32,
        /// Don't reset the board on connect (press its Reset button yourself)
        #[arg(long)]
        no_reset: bool,
        /// Print every value (default: a summary)
        #[arg(long)]
        raw: bool,
        /// Also show the board's own text output
        #[arg(long)]
        verbose: bool,
    },
    /// Stream from the custom BLE firmware (`firmware/emotibit-ble`) for a while and report
    /// rates, values and link statistics. Same as `--transport ble stream` plus the link report.
    Ble {
        /// How long to read
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// How long to scan for the device
        #[arg(long, default_value_t = 10)]
        scan_seconds: u64,
        /// Print every converted sample (default: a summary)
        #[arg(long)]
        raw: bool,
        /// Wire format (overrides --ble-format): raw (20-byte records) or compact
        #[arg(long, value_parser = ["raw", "compact"])]
        format: Option<String>,
        /// Also record to the board's SD card meanwhile
        #[arg(long)]
        record: bool,
        /// SD file format: csv (stock lines) or bin (raw records, ~3x smaller)
        #[arg(long, value_parser = ["csv", "bin"], default_value = "csv")]
        sd_format: String,
        /// Save a raw log here and convert it to per-signal CSVs when the stream ends
        #[arg(long, value_name = "DIR")]
        record_dir: Option<PathBuf>,
        #[command(flatten)]
        outputs: OutputArgs,
    },
    /// SD card of a BLE board (Bluetooth implied): recordings, downloads, start and stop, rename,
    /// copy, delete
    Sd {
        #[command(subcommand)]
        action: SdAction,
    },
    /// Check a BLE board's chips, SD card, EEPROM identity, LEDs and battery (Bluetooth implied)
    Selftest,
    /// Show or change a BLE board's settings over Bluetooth (kept in its flash, SD card, or both)
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Firmware over USB: what is on the board, backup and restore, install the stock (WiFi)
    /// or the emotibit-ble (Bluetooth) firmware, or any ESP32 image
    #[cfg(feature = "flash")]
    Firmware {
        #[command(subcommand)]
        action: emotibit::flasher::FirmwareCommand,
    },
    /// Set the device's power mode: normal, low, max-low, wireless-off, hibernate
    Power {
        mode: String,
        /// Needed for hibernate, which powers the device down until its button is pressed
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum SdAction {
    /// The recordings on the card: when, how long, how big, format, notes and LSL markers
    Sessions,
    /// List the files on the card
    List,
    /// Download a file over Bluetooth (about 5 kB/s); not while the board records
    Get {
        /// File name as `sessions` or `list` shows it, e.g. 2026-11-07_23-49-09-480343.csv
        name: String,
        /// Directory to save it in (default: current)
        #[arg(long, default_value = ".")]
        out: PathBuf,
        /// Also convert it to per-signal CSVs, like `parse`
        #[arg(long)]
        parse: bool,
    },
    /// Start recording on the card; it continues after this command exits
    Start {
        /// csv (stock lines) or bin (raw records, ~3x smaller)
        #[arg(long, value_parser = ["csv", "bin"], default_value = "csv")]
        format: String,
    },
    /// Stop the recording
    Stop,
    /// Copy a local file onto the card (8.3 name), e.g. a CONFIG.TXT with the board's settings
    Put {
        /// Local file
        file: PathBuf,
        /// Name on the card (default: the local name, upper-cased)
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete one file from the card, by the name `list` shows (CONFIG.TXT is refused)
    Rm { name: String },
    /// Delete a recording: its data file and its `_info.json`
    Delete {
        /// Data file as `sessions` shows it, e.g. 2026-11-08_10-00-02-000000.csv
        name: String,
    },
    /// Rename a recording (data file and `_info.json`); the extension stays
    Rename {
        /// Data file as `sessions` shows it
        name: String,
        /// New name without the extension: letters, digits, space, - _ . ( ), up to 36
        new_name: String,
    },
    /// Copy a recording on the card (data file and `_info.json`), about 300 kB/s
    Copy {
        /// Data file as `sessions` shows it
        name: String,
        /// Name of the copy without the extension
        new_name: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show the settings in effect and where they come from (the default)
    Show,
    /// Change settings: profile=<name> (see `config profiles`), name=<up to 9 chars, empty for
    /// the default>, leds=0|1, and any of ppg_hz, led_brightness, cpu_mhz, sensor_ms, conn_ms,
    /// adv_ms, tx_dbm, ppg_led. Keys after a profile override it. The name and CPU clock apply
    /// after a restart, the rest at once.
    Set {
        #[arg(value_name = "KEY=VALUE", required = true)]
        settings: Vec<String>,
        /// Restart the board afterwards (to apply a new name)
        #[arg(long)]
        restart: bool,
        /// Where to keep them: flash (the ESP32's own; default), sd (the card's CONFIG.TXT,
        /// which wins at boot; the rest of the file is kept, the original saved as CONFIG.BAK), or both
        #[arg(long, value_parser = ["flash", "sd", "both"], default_value = "flash")]
        store: String,
    },
    /// Remove the stored settings (they stay in effect until a restart)
    Clear {
        #[arg(long, value_parser = ["flash", "sd", "both"], default_value = "flash")]
        store: String,
    },
    /// Restart the board
    Restart,
    /// List the profiles and every numeric setting (no board needed)
    Profiles,
}

pub fn run() -> Result<()> {
    env_logger::init();
    let args = Args::parse();
    GZIP.store(args.gzip, std::sync::atomic::Ordering::Relaxed);
    // Serial and blocking: no runtime needed.
    #[cfg(feature = "flash")]
    if let Some(Command::Firmware { action }) = args.command {
        // Over Bluetooth: needs the runtime (without Bluetooth, `flasher::run` says so).
        #[cfg(feature = "ble")]
        if let emotibit::flasher::FirmwareCommand::Ota { image, dry_run, yes } = action {
            let timeout = Duration::from_secs(args.timeout);
            return tokio::runtime::Runtime::new()?.block_on(firmware_ota(args.device, timeout, image, dry_run, yes));
        }
        return emotibit::flasher::run(action);
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let config = EmotiBitClientConfig {
            scan_timeout: Duration::from_secs(args.timeout),
            target_ip: args.ip,
            device: args.device.clone(),
            transport: match args.transport.as_str() {
                "ble" => Transport::Ble,
                "both" => Transport::Both,
                _ => Transport::Wifi,
            },
            ble_compact: args.ble_format == "compact",
            ..Default::default()
        };
        let all = args.all;
        match args.command.unwrap_or(Command::Stream {
            raw: false,
            seconds: None,
            record: false,
            sd_format: "csv".into(),
            record_dir: None,
            outputs: OutputArgs::default(),
        }) {
            Command::Scan => scan(config).await,
            Command::Info { seconds } => info(config, all, seconds).await,
            Command::Stream { raw, seconds, record, sd_format, record_dir, outputs } => {
                let binary = sd_format == "bin";
                let compact = config.ble_compact;
                stream(config, all, StreamOpts { raw, seconds, record, binary, record_dir, outputs, link_report: false, compact }).await
            }
            Command::Tui { outputs } => tui(config, all, outputs).await,
            Command::Parse { file, out } => parse(&file, out.as_deref()),
            Command::Serve { ws, web, allow_origin, remote, record_dir, outputs } => {
                serve(config, ServeOpts { ws, web, allow_origin, remote, record_dir, outputs, open: false }).await
            }
            Command::Web { port, bind, no_open, web, allow_origin, remote, record_dir, outputs } => {
                web_cmd(config, port, bind, no_open, web, allow_origin, remote, record_dir, outputs).await
            }
            Command::Serial { port, signal, seconds, baud, no_reset, raw, verbose } => {
                serial_cmd(port, &signal, seconds, baud, no_reset, raw, verbose).await
            }
            Command::Ble { seconds, scan_seconds, raw, format, record, sd_format, record_dir, outputs } => {
                let mut config = config;
                config.transport = Transport::Ble;
                config.scan_timeout = Duration::from_secs(scan_seconds);
                if let Some(f) = format {
                    config.ble_compact = f == "compact";
                }
                let binary = sd_format == "bin";
                let opts = StreamOpts { raw, seconds: Some(seconds), record, binary, record_dir, outputs, link_report: true, compact: config.ble_compact };
                stream(config, all, opts).await
            }
            Command::Sd { action } => sd(config, action).await,
            Command::Selftest => selftest(config).await,
            Command::Config { action } => config_cmd(config, action.unwrap_or(ConfigAction::Show)).await,
            Command::Power { mode, yes } => power(config, &mode, yes).await,
            #[cfg(feature = "flash")]
            Command::Firmware { .. } => unreachable!("handled above"),
        }
    })
}

type Connection = (Receiver<EmotiBitEvent>, EmotiBitHandle);

/// `MD-V6-0000356 (10.0.0.149)` or `EmotiBit-BLE-E848 (Bluetooth)`.
fn describe(d: &EmotiBitDevice) -> String {
    if d.is_ble() { format!("{} (Bluetooth)", d.id) } else { format!("{} ({})", d.id, d.ip) }
}

async fn connect(config: EmotiBitClientConfig, all: bool) -> Result<Vec<Connection>> {
    let client = EmotiBitClient::new(config);
    if all {
        client.connect_all().await
    } else {
        Ok(vec![client.connect().await?])
    }
}

async fn scan(config: EmotiBitClientConfig) -> Result<()> {
    let config_transport = config.transport;
    eprintln!("Scanning for {} s...", config.scan_timeout.as_secs());
    let devices = EmotiBitClient::new(config).scan_all().await?;
    if devices.is_empty() {
        match config_transport {
            Transport::Wifi => eprintln!("No EmotiBit found. Same 2.4 GHz network? Powered, not on HIB? (BLE boards: --transport ble)"),
            _ => eprintln!("No EmotiBit found. Powered, in range, not hibernating or wireless-off?"),
        }
        std::process::exit(1);
    }
    for d in devices {
        let state = if d.available { "available" } else { "in use by another host" };
        let addr = if d.is_ble() { "Bluetooth".to_string() } else { d.ip.to_string() };
        println!("{}\t{}\t{}", d.id, addr, state);
    }
    Ok(())
}

// ── info ─────────────────────────────────────────────────────────────────────

async fn info(config: EmotiBitClientConfig, all: bool, seconds: u64) -> Result<()> {
    let conns = connect(config, all).await?;
    let mut tasks = Vec::new();
    for (rx, handle) in conns {
        tasks.push(tokio::spawn(info_one(rx, handle, seconds)));
    }
    for t in tasks {
        t.await??;
    }
    Ok(())
}

async fn info_one(mut rx: Receiver<EmotiBitEvent>, handle: EmotiBitHandle, seconds: u64) -> Result<()> {
    let dev = handle.device().clone();
    println!("Connected to {}", describe(&dev));

    let mut raw = handle.subscribe_raw();
    let mut sync = TimeSync::new();
    let sync_task = tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match raw.recv().await {
                Ok(line) => {
                    if let Ok(p) = Packet::parse(&line) {
                        sync.observe(&p, datafile::parse_local_timestamp);
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return sync,
            }
        }
    });

    let mut health = StreamHealth::new();
    let mut last: BTreeMap<&'static str, f64> = BTreeMap::new();
    let mut messages: BTreeMap<String, u64> = BTreeMap::new();
    let end = Instant::now() + Duration::from_secs(seconds);
    while let Ok(Some(ev)) = tokio::time::timeout(end.saturating_duration_since(Instant::now()), rx.recv()).await {
        match ev {
            EmotiBitEvent::Sensor(r) => {
                health.record(&r);
                if let Some(v) = r.samples.last() {
                    last.insert(r.channel.tag, *v);
                }
            }
            EmotiBitEvent::Message(p) => {
                health.observe_number(p.number);
                *messages.entry(p.tag).or_default() += 1
            }
            EmotiBitEvent::Disconnected => break,
            _ => {}
        }
    }

    println!("\n{:<4} {:<9} {:>8} {:>9} {:>5} {:>11}  unit", "tag", "channel", "packets", "samples/s", "under", "last");
    for (tag, h) in health.iter() {
        let c = channel(tag).expect("health only tracks known channels");
        let v = last.get(tag).map(|v| format!("{v:.4}")).unwrap_or_default();
        println!("{:<4} {:<9} {:>8} {:>9.1} {:>5} {:>11}  {}", tag, c.name, h.packets, h.rate_hz, h.underruns, v, c.unit);
    }

    let s = handle.status();
    println!(
        "\nstatus: battery {}  power {}  recording {}  sd {}  clipping {}  overflow {}",
        s.battery_percent.map_or("–".into(), |b| format!("{b:.0}%")),
        s.power_mode.map_or("–", |m| m.label()),
        s.recording.map_or("–", |r| if r { "yes" } else { "no" }),
        s.sd_percent.map_or("–".into(), |b| format!("{b:.0}%")),
        s.clipping_events,
        s.overflow_events,
    );
    println!("sequence gaps: {}   buffer underruns: {}", health.total_lost(), health.total_underruns());
    if !messages.is_empty() {
        println!("other packets: {}", messages.iter().map(|(t, n)| format!("{t}×{n}")).collect::<Vec<_>>().join(" "));
    }

    handle.disconnect().await?;
    drop(rx);
    if let Ok(sync) = tokio::time::timeout(Duration::from_secs(2), sync_task).await {
        let sync = sync?;
        let rtts: Vec<i64> = sync.points().iter().map(|p| p.round_trip).collect();
        match rtts.iter().min() {
            Some(min) => println!("clock sync: {} exchanges, best round trip {} ms", rtts.len(), min),
            None => println!("clock sync: no complete exchange yet (they arrive every few seconds)"),
        }
    }
    Ok(())
}

// ── stream ───────────────────────────────────────────────────────────────────

struct StreamOpts {
    raw: bool,
    seconds: Option<u64>,
    record: bool,
    /// SD recording in the BLE firmware's binary format.
    binary: bool,
    record_dir: Option<PathBuf>,
    outputs: OutputArgs,
    /// Print the `ble` command's end report (per-channel stats and link) instead of the
    /// once-a-second summary.
    link_report: bool,
    /// BLE wire format, for the report.
    compact: bool,
}

async fn stream(config: EmotiBitClientConfig, all: bool, opts: StreamOpts) -> Result<()> {
    eprintln!("Looking for an EmotiBit...");
    let conns = connect(config, all).await?;
    let multi = conns.len() > 1;
    let opts = std::sync::Arc::new(opts);
    let mut tasks = Vec::new();
    for (rx, handle) in conns {
        eprintln!("Connected to {}. Ctrl-C to stop.", describe(handle.device()));
        tasks.push(tokio::spawn(stream_one(rx, handle, opts.clone(), multi)));
    }
    // Ctrl-C ends every device's loop through its own signal handler.
    for t in tasks {
        t.await??;
    }
    Ok(())
}

/// One channel's numbers for the `ble` report.
struct ChanStats {
    n: u64,
    sum: f64,
    min: f64,
    max: f64,
    last: f64,
    first_at: f64,
    last_at: f64,
}

impl ChanStats {
    fn new(at: f64) -> Self {
        Self { n: 0, sum: 0.0, min: f64::MAX, max: f64::MIN, last: 0.0, first_at: at, last_at: at }
    }
    fn add(&mut self, v: f64, at: f64) {
        self.n += 1;
        self.sum += v;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.last = v;
        self.last_at = at;
    }
}

#[derive(Default)]
struct Last {
    last: Option<f64>,
}

async fn stream_one(
    mut rx: Receiver<EmotiBitEvent>,
    handle: EmotiBitHandle,
    opts: std::sync::Arc<StreamOpts>,
    multi: bool,
) -> Result<()> {
    let id = handle.device().id.clone();
    let prefix = if multi { format!("[{id}] ") } else { String::new() };

    let mut outputs = Outputs::start(&opts.outputs.config(), &handle)?;
    let logger = match &opts.record_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            let path = raw_log_path(dir, &id);
            eprintln!("{prefix}Logging raw packets to {}", path.display());
            Some((path.clone(), datafile::spawn_raw_logger(&handle, path).await?))
        }
        None => None,
    };
    if opts.record {
        for _ in 0..20 {
            let sent = if opts.binary { handle.record_begin_binary().await } else { handle.record_begin().await };
            if sent.is_ok() {
                eprintln!("{prefix}Recording on the device's SD card.");
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    let start = Instant::now();
    let end = opts.seconds.map(|s| start + Duration::from_secs(s));
    let mut health = StreamHealth::new();
    let mut last: BTreeMap<&'static str, (Channel, Last)> = BTreeMap::new();
    // `ble` report: per-channel statistics, and the latest link statistics.
    let mut stats: BTreeMap<&'static str, ChanStats> = BTreeMap::new();
    // Set from the BLE link report; stays None without the `ble` feature.
    #[cfg_attr(not(feature = "ble"), allow(unused_mut))]
    let mut link: Option<Vec<f64>> = None;
    let mut last_file: Option<String> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await; // the first tick fires immediately
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = &mut ctrl_c => break,
            ev = rx.recv() => match ev {
                Some(ev) => {
                    outputs.on_event(&ev);
                    match ev {
                        EmotiBitEvent::Sensor(r) => {
                            if opts.link_report {
                                for &v in &r.samples {
                                    stats.entry(r.channel.tag).or_insert_with(|| ChanStats::new(r.received_at_ms)).add(v, r.received_at_ms);
                                }
                            }
                            if opts.raw {
                                let vals = r.samples.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",");
                                println!("{prefix}{:.3}\t{}\t{}\t{}", r.received_at_ms, r.channel.tag, r.device_timestamp_ms, vals);
                            } else {
                                health.record(&r);
                                let e = last.entry(r.channel.tag).or_insert((r.channel, Last::default()));
                                e.1.last = r.samples.last().copied().or(e.1.last);
                            }
                        }
                        #[cfg(feature = "ble")]
                        EmotiBitEvent::Message(p) if p.tag == emotibit::ble::LINK_TAG => {
                            link = Some(p.values());
                        }
                        EmotiBitEvent::Message(p) => {
                            health.observe_number(p.number);
                            if opts.raw { println!("{prefix}msg\t{}\t{}", p.tag, p.payload.join(",")); }
                        }
                        EmotiBitEvent::Status(s) => {
                            if s.recording_file.is_some() && s.recording_file != last_file {
                                eprintln!("{prefix}SD card file: {}", s.recording_file.as_deref().unwrap_or(""));
                                last_file = s.recording_file.clone();
                            }
                            if opts.raw { eprintln!("{prefix}status: {s:?}"); }
                        }
                        EmotiBitEvent::Disconnected => { eprintln!("{prefix}Disconnected."); break }
                        _ => {}
                    }
                }
                None => { eprintln!("{prefix}Disconnected."); break }
            },
            _ = tick.tick(), if !opts.raw && !opts.link_report => {
                let s = handle.status();
                println!("{prefix}--- battery {}  power {}  rec {}  gaps {}  clip {}  over {} ---",
                    s.battery_percent.map_or("–".into(), |b| format!("{b:.0}%")),
                    s.power_mode.map_or("–", |m| m.label()),
                    if s.recording == Some(true) { "yes" } else { "no" },
                    health.total_lost(), s.clipping_events, s.overflow_events);
                for (tag, (c, l)) in &last {
                    let rate = health.channel(tag).map_or(0.0, |h| h.rate_hz);
                    let v = l.last.map(|v| format!("{v:.4}")).unwrap_or_default();
                    println!("{prefix}  {:<9} {:>6.1} samples/s  last {} {}", c.name, rate, v, c.unit);
                }
            }
        }
        if end.is_some_and(|e| Instant::now() >= e) {
            break;
        }
    }

    if opts.record {
        let _ = handle.record_end().await;
        // Let the command reach the board before the link goes.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if opts.link_report {
        let secs = start.elapsed().as_secs_f64();
        if let Some(l) = &link {
            // dropped, MTU, last batch, bytes on air, lost, received
            let g = |i: usize| l.get(i).copied().unwrap_or(0.0);
            println!("{prefix}{:.0} records in {secs:.1} s ({:.1}/s), {:.0} lost", g(5), g(5) / secs, g(4));
            println!(
                "{prefix}  link: {} format, ATT MTU {:.0}, {:.0} bytes on air ({:.0} B/s, {:.1} B/record), {:.0} dropped in firmware, last batch {:.0}",
                if opts.compact { "compact" } else { "raw" },
                g(1), g(3), g(3) / secs, g(3) / g(5).max(1.0), g(0), g(2)
            );
        }
        let s = handle.status();
        println!(
            "{prefix}  status: battery {}  power {}  recording {}",
            s.battery_percent.map_or("–".into(), |b| format!("{b:.0}%")),
            s.power_mode.map_or("–", |m| m.label()),
            s.recording.map_or("–", |r| if r { "yes" } else { "no" }),
        );
        for (tag, c) in &stats {
            let (name, unit) = channel(tag).map(|c| (c.name, c.unit)).unwrap_or((tag, ""));
            // Rate over the channel's own arrivals: with several boards, data queued while the
            // others connected would inflate a rate over the run time.
            let span = (c.last_at - c.first_at) / 1000.0;
            let rate = if span > 1.0 { (c.n - 1) as f64 / span } else { c.n as f64 / secs };
            println!(
                "{prefix}  {:<8} {:>7.1}/s  mean {:>12.4}  min {:>12.4}  max {:>12.4}  last {:>12.4} {}",
                name, rate, c.sum / c.n as f64, c.min, c.max, c.last, unit
            );
        }
    }
    handle.disconnect().await?;
    outputs.stop();
    if let Some((path, task)) = logger {
        let lines = task.await??;
        eprintln!("{prefix}Logged {lines} packets. Converting...");
        report(&datafile::parse_file(&path, path.parent(), gzip())?, &prefix);
    }
    Ok(())
}

fn report(r: &datafile::ParseReport, prefix: &str) {
    eprintln!(
        "{prefix}{} packets ({} malformed), {} clock syncs{}",
        r.packets,
        r.malformed,
        r.sync_points,
        if r.anchored { "" } else { " - none found, LocalTimestamp is relative to the start" }
    );
    for (tag, n) in &r.rows {
        eprintln!("{prefix}  {tag}: {n} rows");
    }
    if let Some(dir) = r.files.first().and_then(|f| f.parent()) {
        eprintln!("{prefix}Wrote {} files to {}", r.files.len(), dir.display());
    }
}

// ── parse / power / tui ──────────────────────────────────────────────────────

fn parse(file: &Path, out: Option<&Path>) -> Result<()> {
    let r = datafile::parse_file(file, out, gzip())?;
    report(&r, "");
    Ok(())
}

async fn power(config: EmotiBitClientConfig, mode: &str, yes: bool) -> Result<()> {
    let mode = PowerMode::from_name(mode)
        .with_context(|| format!("unknown mode {mode:?}; use normal, low, max-low, wireless-off or hibernate"))?;
    if mode == PowerMode::Hibernate && !yes {
        bail!("hibernate powers the device down until its button is pressed; pass --yes to do it");
    }
    let (mut rx, handle) = EmotiBitClient::new(config).connect().await?;
    // The control channel comes up a moment after the handshake.
    let mut sent = false;
    for _ in 0..20 {
        if handle.set_power_mode(mode).await.is_ok() {
            sent = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if !sent {
        bail!("control channel did not come up");
    }
    println!("Sent power mode {} to {}", mode.label(), handle.device().id);
    // Wait briefly for the device to confirm via its status packet.
    let confirmed = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(ev) = rx.recv().await {
            if let EmotiBitEvent::Status(s) = ev
                && s.power_mode == Some(mode) {
                    return true;
                }
        }
        false
    })
    .await
    .unwrap_or(false);
    println!("{}", if confirmed { "Confirmed by the device." } else { "No confirmation seen (it may already have been in that mode, or powered down)." });
    handle.disconnect().await?;
    Ok(())
}

// ── sd ───────────────────────────────────────────────────────────────────────

async fn sd(mut config: EmotiBitClientConfig, action: SdAction) -> Result<()> {
    if config.transport == Transport::Wifi {
        config.transport = Transport::Ble;
    }
    let (mut rx, handle) = EmotiBitClient::new(config).connect().await?;
    if !handle.device().is_ble() {
        bail!("the sd command needs a BLE board (--transport ble); the stock firmware's card is reached through the Oscilloscope's file transfer");
    }
    // Next message with one of `tags`, skipping sensor data; None after `wait` without one.
    async fn next(rx: &mut Receiver<EmotiBitEvent>, tags: &[&str], wait: Duration) -> Option<Packet> {
        let deadline = Instant::now() + wait;
        while let Ok(Some(ev)) = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), rx.recv()).await {
            match ev {
                EmotiBitEvent::Message(p) if tags.contains(&p.tag.as_str()) => return Some(p),
                EmotiBitEvent::Disconnected => return None,
                _ => {}
            }
        }
        None
    }
    let result = async {
        match action {
            SdAction::Sessions => {
                handle.send_control("FS", &[]).await?;
                let mut rows = Vec::new();
                while let Some(p) = next(&mut rx, &["FS"], Duration::from_secs(15)).await {
                    if p.payload.is_empty() {
                        print_sessions(&rows);
                        return Ok(());
                    }
                    rows.push(p.payload);
                }
                bail!("no answer from the board (no SD card, or firmware without sessions)")
            }
            SdAction::List => {
                handle.send_control("FL", &[]).await?;
                let mut n = 0;
                while let Some(p) = next(&mut rx, &["FL"], Duration::from_secs(10)).await {
                    match (p.payload.first(), p.payload.get(1)) {
                        (Some(name), Some(size)) => {
                            println!("{name}\t{size}");
                            n += 1;
                        }
                        _ => {
                            eprintln!("{n} files");
                            return Ok(());
                        }
                    }
                }
                bail!("no answer from the board (no SD card?)")
            }
            SdAction::Get { name, out, parse: convert } => {
                let started = Instant::now();
                // Chunks by offset. Notifications can be lost on the way; missing ranges are
                // asked for again at the end.
                let mut chunks: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
                let mut size = None;
                let mut range = (0usize, 0usize); // (offset, length; 0 = to the end)
                for round in 0..20 {
                    handle.send_control("FG", &[name.clone(), range.0.to_string(), range.1.to_string()]).await?;
                    let mut shown = Instant::now();
                    while let Some(p) = next(&mut rx, &["FG", "FD", "FE"], Duration::from_secs(10)).await {
                        let field = |i: usize| p.payload.get(i).map(String::as_str).unwrap_or("");
                        match p.tag.as_str() {
                            "FG" => size = field(1).parse::<usize>().ok().or(size),
                            "FD" => {
                                let offset: usize = field(0).parse().context("bad FD line")?;
                                chunks.entry(offset).or_insert(base64_decode(field(1)).context("bad base64")?);
                                if shown.elapsed() > Duration::from_secs(2) {
                                    shown = Instant::now();
                                    let have: usize = chunks.values().map(Vec::len).sum();
                                    eprintln!("  {have} / {} bytes", size.unwrap_or(0));
                                }
                            }
                            _ if field(1).parse::<usize>().is_err() => bail!("{name}: {}", field(1)),
                            _ => break, // FE: this request is done
                        }
                    }
                    if !handle.is_connected() {
                        bail!("disconnected during the download");
                    }
                    let Some(total) = size else { bail!("no answer from the board") };
                    // First missing range, if any.
                    let mut at = 0;
                    let mut missing = None;
                    for (&o, c) in &chunks {
                        if o > at {
                            missing = Some((at, o - at));
                            break;
                        }
                        at = at.max(o + c.len());
                    }
                    match missing.or((at < total).then_some((at, total - at))) {
                        None => break,
                        Some(m) => {
                            log::debug!("round {round}: asking again for {} bytes at {}", m.1, m.0);
                            range = m;
                        }
                    }
                }
                let total = size.unwrap_or(0);
                let mut data = Vec::with_capacity(total);
                for (o, c) in chunks {
                    if o == data.len() {
                        data.extend(c);
                    }
                }
                if data.len() != total {
                    bail!("got {} of {} bytes", data.len(), total);
                }
                std::fs::create_dir_all(&out)?;
                let path = out.join(&name);
                std::fs::write(&path, &data)?;
                let secs = started.elapsed().as_secs_f64();
                eprintln!("Saved {} ({} bytes, {:.1} kB/s)", path.display(), data.len(), data.len() as f64 / 1000.0 / secs);
                if convert {
                    report(&datafile::parse_file(&path, Some(&out), gzip())?, "");
                }
                Ok(())
            }
            SdAction::Start { format } => {
                if format == "bin" { handle.record_begin_binary().await? } else { handle.record_begin().await? }
                wait_recording(&mut rx, true).await
            }
            SdAction::Stop => {
                handle.record_end().await?;
                wait_recording(&mut rx, false).await
            }
            SdAction::Put { file, name } => {
                let data = std::fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?;
                let name = match name {
                    Some(n) => n,
                    None => file.file_name().and_then(|n| n.to_str()).context("no file name")?.to_ascii_uppercase(),
                };
                handle.send_control("FO", std::slice::from_ref(&name)).await?;
                for chunk in data.chunks(300) {
                    handle.send_control("FA", &[base64_encode(chunk)]).await?;
                }
                handle.send_control("FC", &[]).await?;
                let p = next(&mut rx, &["FC"], Duration::from_secs(10)).await.context("no answer from the board")?;
                match p.payload.get(1).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) if n == data.len() => println!("Wrote {name} ({n} bytes)"),
                    Some(n) => bail!("the board wrote {n} of {} bytes", data.len()),
                    None => bail!("{name}: {}", p.payload.get(1).map(String::as_str).unwrap_or("failed")),
                }
                Ok(())
            }
            SdAction::Rm { name } => {
                handle.send_control("FX", std::slice::from_ref(&name)).await?;
                let p = next(&mut rx, &["FX"], Duration::from_secs(10)).await.context("no answer from the board")?;
                let result = p.payload.get(1).cloned().unwrap_or_default();
                if result != "deleted" {
                    bail!("{name}: {result}");
                }
                println!("Deleted {name}");
                Ok(())
            }
            SdAction::Delete { name } => session_op(&handle, &mut rx, "FZ", &name, None).await,
            SdAction::Rename { name, new_name } => session_op(&handle, &mut rx, "FR", &name, Some(&new_name)).await,
            SdAction::Copy { name, new_name } => session_op(&handle, &mut rx, "FY", &name, Some(&new_name)).await,
        }
    }
    .await;
    handle.disconnect().await?;
    result
}

/// `FR` (rename), `FY` (copy) or `FZ` (delete) on a recording, waiting for the board's
/// `<tag>,<name>,ok,<new name>` or `<tag>,<name>,error,<why>`.
async fn session_op(handle: &EmotiBitHandle, rx: &mut Receiver<EmotiBitEvent>, tag: &str, name: &str, to: Option<&str>) -> Result<()> {
    if name.contains(',') || to.is_some_and(|t| t.contains(',')) {
        bail!("names cannot contain commas");
    }
    let mut fields = vec![name.to_string()];
    fields.extend(to.map(str::to_string));
    handle.send_control(tag, &fields).await?;
    // A copy reports progress; the answer comes when it is done.
    let wait = Duration::from_secs(if tag == "FY" { 30 } else { 15 });
    loop {
        let deadline = Instant::now() + wait;
        let p = loop {
            match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), rx.recv()).await {
                Ok(Some(EmotiBitEvent::Message(p))) if p.tag == tag && p.payload.first().map(String::as_str) == Some(name) => break p,
                Ok(Some(EmotiBitEvent::Disconnected)) | Ok(None) => bail!("disconnected"),
                Ok(Some(_)) => {}
                Err(_) => bail!("no answer from the board (firmware without session commands?)"),
            }
        };
        let field = |i: usize| p.payload.get(i).map(String::as_str).unwrap_or("");
        match field(1) {
            "copying" => eprintln!("  {} / {} bytes", field(2), field(3)),
            "ok" => {
                match tag {
                    "FZ" => println!("Deleted {name}"),
                    "FR" => println!("Renamed {name} to {}", field(2)),
                    _ => println!("Copied {name} to {}", field(2)),
                }
                return Ok(());
            }
            _ => bail!("{name}: {}", field(2)),
        }
    }
}

/// `config profiles`: the settings table and the profiles, from the code the firmware shares.
fn print_profiles() {
    use emotibit::ble::tuning::{Allowed, NUMBERS, PROFILES};
    println!("Settings (emotibit config set KEY=VALUE ...):");
    for s in &NUMBERS {
        let allowed = match s.allowed {
            Allowed::Range(lo, hi) => format!("{lo}-{hi}"),
            Allowed::OneOf(vs) => vs.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|"),
        };
        let default = s.default.map_or("radio's own".to_string(), |d| d.to_string());
        let unit = if s.unit.is_empty() { String::new() } else { format!(" {}", s.unit) };
        println!("  {:<15} {:<22} default {default}{unit}{}", s.key, allowed, if s.restart { ", after a restart" } else { "" });
        println!("  {:<15} {}", "", s.help);
    }
    println!("\nProfiles (emotibit config set profile=NAME [KEY=VALUE ...] --restart):");
    let header: Vec<String> = NUMBERS.iter().map(|s| s.key.to_string()).collect();
    println!("  {:<15} {:<5} {}", "", "leds", header.iter().map(|h| format!("{h:>15}")).collect::<String>());
    for p in &PROFILES {
        let values: String = p.values.iter().map(|v| format!("{:>15}", v.map_or("-".to_string(), |v| v.to_string()))).collect();
        println!("  {:<15} {:<5} {values}", p.name, if p.leds { "on" } else { "off" });
    }
    println!();
    for p in &PROFILES {
        println!("  {:<15} {}", p.name, p.help);
    }
    println!("\nThe CPU clock applies after a restart (--restart); the rest at once. Power use was not measured.");
}

/// `emotibit firmware ota`: the emotibit-ble app over Bluetooth to one board (named, or the only
/// one in range), then waits for it to come back.
#[cfg(all(feature = "flash", feature = "ble"))]
async fn firmware_ota(device: Option<String>, timeout: Duration, image: Option<PathBuf>, dry_run: bool, yes: bool) -> Result<()> {
    let (app, what) = emotibit::flasher::ota_image(image.as_deref())?;
    println!("Image: {what} ({} KB)", app.len() / 1024);
    let name = match device {
        Some(d) => d,
        None => {
            let found = emotibit::ble::scan(timeout).await?;
            match found.as_slice() {
                [(d, _)] => d.id.clone(),
                [] => bail!("no EmotiBit found over Bluetooth"),
                many => {
                    let names: Vec<&str> = many.iter().map(|(d, _)| d.id.as_str()).collect();
                    bail!("{} boards in range ({}): pick one with --device NAME", names.len(), names.join(", "))
                }
            }
        }
    };
    let bt = emotibit::ble::request(Some(&name), timeout.max(Duration::from_secs(10))).await?;
    println!("Board: {name}");
    if dry_run {
        println!("Dry run: the image looks right and the board answered; nothing was sent.");
        return Ok(());
    }
    if !yes {
        use std::io::Write;
        print!("Update {name} over Bluetooth now? The board restarts when done; a recording on it is closed. [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            bail!("not updated");
        }
    }
    let bar = indicatif::ProgressBar::new(app.len() as u64);
    bar.set_style(
        indicatif::ProgressStyle::with_template("{bar:40} {bytes}/{total_bytes} {bytes_per_sec}, {eta} left")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar()),
    );
    let started = Instant::now();
    let result = emotibit::ble::update_firmware(bt, &app, |sent, total| {
        bar.set_length(total as u64);
        bar.set_position(sent as u64);
    })
    .await;
    bar.finish_and_clear();
    let sent = result?;
    let how = if sent < app.len() { format!(" ({} KB compressed)", sent / 1024) } else { String::new() };
    println!(
        "Sent {} KB{how} in {:.0} s; the board checked it and is restarting into it.",
        app.len() / 1024,
        started.elapsed().as_secs_f64()
    );
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if emotibit::ble::scan(Duration::from_secs(8)).await?.iter().any(|(d, _)| d.id == name) {
            println!("{name} is back.");
            return Ok(());
        }
    }
    println!("{name} has not come back yet; if it does not, install over USB (emotibit firmware ble).");
    Ok(())
}

/// Sends `CR` and waits for the board to drop the link: on a slow connection interval the
/// command can take a few hundred milliseconds to go out, and disconnecting first would lose it.
async fn restart_board(handle: &EmotiBitHandle, rx: &mut Receiver<EmotiBitEvent>) -> Result<()> {
    handle.send_control("CR", &[]).await?;
    let gone = tokio::time::timeout(Duration::from_secs(12), async {
        while let Some(ev) = rx.recv().await {
            if matches!(ev, EmotiBitEvent::Disconnected) {
                return;
            }
        }
    })
    .await;
    match gone {
        Ok(()) => println!("Restarting the board."),
        Err(_) => println!("Asked the board to restart; it has not dropped the link yet."),
    }
    Ok(())
}

async fn config_cmd(mut config: EmotiBitClientConfig, action: ConfigAction) -> Result<()> {
    if let ConfigAction::Profiles = action {
        print_profiles();
        return Ok(());
    }
    if config.transport == Transport::Wifi {
        config.transport = Transport::Ble;
    }
    let (mut rx, handle) = EmotiBitClient::new(config).connect().await?;
    // Next message with one of `tags`.
    async fn answer(rx: &mut Receiver<EmotiBitEvent>, tags: &[&str]) -> Option<Packet> {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(ev) = rx.recv().await {
                match ev {
                    EmotiBitEvent::Message(p) if tags.contains(&p.tag.as_str()) => return Some(p),
                    EmotiBitEvent::Disconnected => return None,
                    _ => {}
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
    }
    let show = |p: &Packet| {
        let get = |k: &str| p.keyed(k).unwrap_or("");
        println!("{}", describe(handle.device()));
        println!("  {:<15} {}  (advertised as EmotiBit-BLE-{})", "name", get("name"), get("name"));
        if p.keyed("profile").is_some() {
            println!("  {:<15} {}", "profile", get("profile"));
        }
        println!("  {:<15} {}", "leds", if get("leds") == "1" { "on" } else { "off" });
        for s in &emotibit::ble::tuning::NUMBERS {
            let Some(v) = p.keyed(s.key) else { continue };
            let mut line = format!("  {:<15} {v}{}", s.key, if s.unit.is_empty() { String::new() } else { format!(" {}", s.unit) });
            if s.key == "cpu_mhz" && !get("cpu_mhz_next").is_empty() && get("cpu_mhz_next") != v {
                line += &format!("  ({} after a restart)", get("cpu_mhz_next"));
            }
            if s.key == "ppg_led" && let Ok(steps) = v.parse::<f64>() {
                line += &format!("  ({:.1} mA)", steps * 0.2);
            }
            println!("{line}");
        }
        println!("  {:<15} {}", "source", get("source"));
        if !get("device_id").is_empty() {
            println!("  {:<15} {}", "device id", get("device_id"));
        }
        if get("restart_needed") == "1" {
            println!("  a new name or CPU clock takes effect after a restart (emotibit config restart)");
        }
    };
    let result = async {
        match action {
            ConfigAction::Show => {
                handle.send_control("CG", &[]).await?;
                let p = answer(&mut rx, &["CG"]).await.context("no answer (firmware without settings commands?)")?;
                show(&p);
            }
            ConfigAction::Set { settings, restart, store } => {
                let mut payload = Vec::new();
                for kv in &settings {
                    let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: use KEY=VALUE"))?;
                    payload.extend([k.trim().to_string(), v.trim().to_string()]);
                }
                payload.extend(["store".to_string(), store.clone()]);
                handle.send_control("CS", &payload).await?;
                let r = answer(&mut rx, &["CS"]).await.context("no answer (firmware without settings commands?)")?;
                if r.payload.first().map(String::as_str) != Some("ok") {
                    bail!("{}", r.payload.get(1).map(String::as_str).unwrap_or("rejected"));
                }
                if let Some(note) = r.payload.get(1) {
                    println!("{note}");
                }
                if let Some(p) = answer(&mut rx, &["CG"]).await {
                    show(&p);
                }
                if store != "flash" {
                    let w = answer(&mut rx, &["CW"]).await.context("no answer from the SD card")?;
                    let what = w.payload.get(1).map(String::as_str).unwrap_or("");
                    if w.payload.first().map(String::as_str) != Some("ok") {
                        bail!("SD card: {what}");
                    }
                    println!("SD card: {what}");
                }
                if restart {
                    restart_board(&handle, &mut rx).await?;
                }
            }
            ConfigAction::Clear { store } => {
                handle.send_control("CC", std::slice::from_ref(&store)).await?;
                let r = answer(&mut rx, &["CC"]).await.context("no answer (firmware without settings commands?)")?;
                if r.payload.first().map(String::as_str) != Some("ok") {
                    bail!("{}", r.payload.get(1).map(String::as_str).unwrap_or("rejected"));
                }
                if store != "flash" {
                    let w = answer(&mut rx, &["CW"]).await.context("no answer from the SD card")?;
                    let what = w.payload.get(1).map(String::as_str).unwrap_or("");
                    if w.payload.first().map(String::as_str) != Some("ok") {
                        bail!("SD card: {what}");
                    }
                    println!("SD card: {what}");
                }
                if store != "sd" {
                    println!("Flash settings cleared.");
                }
                println!("The current settings stay in effect until a restart.");
            }
            ConfigAction::Restart => restart_board(&handle, &mut rx).await?,
            ConfigAction::Profiles => {}
        }
        anyhow::Ok(())
    }
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.disconnect().await?;
    result
}

async fn selftest(mut config: EmotiBitClientConfig) -> Result<()> {
    if config.transport == Transport::Wifi {
        config.transport = Transport::Ble;
    }
    let (mut rx, handle) = EmotiBitClient::new(config).connect().await?;
    println!("{}", describe(handle.device()));
    handle.send_control("ST", &[]).await?;
    let answer = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = rx.recv().await {
            if let EmotiBitEvent::Message(p) = ev
                && p.tag == "ST" {
                    return Some(p);
                }
        }
        None
    })
    .await;
    handle.disconnect().await?;
    let Ok(Some(p)) = answer else { bail!("no self-test answer (firmware without the ST command?)") };
    let mut failed = 0;
    for pair in p.payload.chunks(2) {
        if let [item, result] = pair {
            println!("  {item:<14} {result}");
            failed += (result == "FAIL") as u32;
        }
    }
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

/// Prints `FS` rows: data file, format, state, duration (s), bytes, notes, markers.
fn print_sessions(rows: &[Vec<String>]) {
    if rows.is_empty() {
        println!("No recordings on the card.");
        return;
    }
    println!("{:<20} {:>9} {:>9}  {:<6}  {:>5} {:>7}  {:<11}  file", "start", "duration", "size", "format", "notes", "markers", "state");
    // The start: the info file's `created_at` (field 8, kept through renames), else the stock
    // name's time (2026-11-07_20-23-15-026091.csv); None for undated 8.3 names.
    let start_of = |r: &Vec<String>| -> Option<String> {
        [r.get(7), r.first()].into_iter().flatten().find_map(|v| {
            let t = v.get(..19)?;
            datafile::parse_local_timestamp(&format!("{t}-000000")).map(|_| format!("{} {}", &t[..10], t[11..].replace('-', ":")))
        })
    };
    // Oldest first; undated ones come before them.
    let mut rows: Vec<(Option<String>, &Vec<String>)> = rows.iter().map(|r| (start_of(r), r)).collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.first().cmp(&b.1.first())));
    for (start, r) in &rows {
        let f = |i: usize| r.get(i).map(String::as_str).unwrap_or("");
        let name = f(0);
        let start = start.clone().unwrap_or_else(|| "–".into());
        let duration = f(3).parse::<f64>().map_or("–".into(), |s| {
            let s = s.round() as u64;
            if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
        });
        let size = f(4).parse::<f64>().map_or("–".into(), |b| if b >= 1e6 { format!("{:.1} MB", b / 1e6) } else { format!("{:.0} kB", b / 1e3) });
        let dash = |v: &str| if v.is_empty() { "–".to_string() } else { v.to_string() };
        println!("{:<20} {:>9} {:>9}  {:<6}  {:>5} {:>7}  {:<11}  {}", start, duration, size, f(1), dash(f(5)), dash(f(6)), f(2), name);
    }
    let total: f64 = rows.iter().filter_map(|(_, r)| r.get(4)?.parse::<f64>().ok()).sum();
    println!("{} recordings, {:.1} MB", rows.len(), total / 1e6);
}

/// Waits for the device's status to report recording on or off.
async fn wait_recording(rx: &mut Receiver<EmotiBitEvent>, on: bool) -> Result<()> {
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = rx.recv().await {
            if let EmotiBitEvent::Status(s) = ev
                && s.recording == Some(on) {
                    return s.recording_file;
                }
        }
        None
    })
    .await;
    match seen {
        Ok(file) if on => println!("Recording to {}", file.unwrap_or_default()),
        Ok(_) => println!("Recording stopped."),
        Err(_) => bail!("the board did not confirm (no SD card?)"),
    }
    Ok(())
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let v = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(v >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let b = s.trim().as_bytes();
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for q in b.chunks(4) {
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        let mut v = 0u32;
        for &c in &q[..4 - pad] {
            v = v << 6 | val(c)?;
        }
        v <<= 6 * pad as u32;
        let bytes = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

#[cfg(feature = "tui")]
async fn tui(config: EmotiBitClientConfig, all: bool, outputs: OutputArgs) -> Result<()> {
    let conns = connect(config, all).await?;
    let cfg = outputs.config();
    let outs = conns.iter().map(|(_, h)| Outputs::start(&cfg, h).map(Some)).collect::<Result<Vec<_>>>()?;
    emotibit::tui::run(conns, outs).await
}

#[cfg(not(feature = "tui"))]
async fn tui(_: EmotiBitClientConfig, _: bool, _: OutputArgs) -> Result<()> {
    bail!("built without the `tui` feature")
}

// ── serve ────────────────────────────────────────────────────────────────────

#[cfg_attr(not(feature = "ws"), allow(dead_code))] // web/allow_origin only feed the bridge
struct ServeOpts {
    ws: Option<SocketAddr>,
    web: Option<PathBuf>,
    allow_origin: Vec<String>,
    remote: Option<u16>,
    record_dir: Option<PathBuf>,
    outputs: OutputArgs,
    /// Open the page in a browser once the device is connected.
    open: bool,
}

/// How one connection ended.
enum Ended {
    Quit,
    Lost,
    Switch(String),
}

async fn serve(mut config: EmotiBitClientConfig, opts: ServeOpts) -> Result<()> {
    let mut remote_rx = match opts.remote {
        Some(port) => {
            let (task, rx) = remote::spawn_remote(port).await?;
            eprintln!("Remote control listening on UDP {port}");
            std::mem::forget(task); // lives for the whole process
            Some(rx)
        }
        None => None,
    };
    #[cfg(not(feature = "ws"))]
    if let Some(addr) = opts.ws {
        bail!("built without the `ws` feature; cannot serve the web bridge on {addr}");
    }
    #[cfg(feature = "ws")]
    if let Some(addr) = opts.ws {
        // Fail now rather than run on without a bridge if the port is taken.
        std::net::TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr} (port in use?)"))?;
        eprintln!(
            "Web bridge on http://{addr}/{}",
            if opts.web.is_some() {
                "  (serving the UI from disk)"
            } else if emotibit::webui::is_embedded() {
                "  (serving the built-in UI)"
            } else {
                "  (WebSocket only; pass --web DIR to serve the UI)"
            }
        );
        if !addr.ip().is_loopback() {
            eprintln!("warning: the bridge is reachable from other machines and can control the device");
        }
    }

    let mut opened = false;
    loop {
        eprintln!("Looking for an EmotiBit...");
        let (mut events, handle) = match EmotiBitClient::new(config.clone()).connect().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e:#}; retrying in 3 s (Ctrl-C to quit)");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => continue,
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
            }
        };
        eprintln!("Connected to {}", describe(handle.device()));

        let mut outputs = Outputs::start(&opts.outputs.config(), &handle)?;
        #[cfg(feature = "ws")]
        let bridge = opts.ws.map(|addr| {
            let cfg = emotibit::bridge::BridgeConfig { addr, web_dir: opts.web.clone(), allow_origins: opts.allow_origin.clone() };
            let h = handle.clone();
            tokio::spawn(async move {
                if let Err(e) = emotibit::bridge::serve(cfg, h).await {
                    eprintln!("bridge stopped: {e:#}");
                }
            })
        });
        #[cfg(not(feature = "ws"))]
        let bridge: Option<tokio::task::JoinHandle<()>> = None;
        if opts.open && !opened
            && let Some(addr) = opts.ws {
                opened = true;
                open_browser(&format!("http://{addr}/"));
            }
        let logger = match &opts.record_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                let path = raw_log_path(dir, &handle.device().id);
                eprintln!("Logging raw packets to {}", path.display());
                Some((path.clone(), datafile::spawn_raw_logger(&handle, path).await?))
            }
            None => None,
        };

        let ended = loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break Ended::Quit,
                ev = events.recv() => match ev {
                    Some(EmotiBitEvent::Disconnected) | None => break Ended::Lost,
                    Some(ev) => outputs.on_event(&ev),
                },
                cmd = async {
                    match remote_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => if let Some(cmd) = cmd {
                    match remote::apply(&handle, cmd.clone()).await {
                        Ok(Some(RemoteCommand::Connect(id))) => break Ended::Switch(id),
                        Ok(_) => {}
                        Err(e) => eprintln!("remote command {cmd:?} failed: {e:#}"),
                    }
                    if cmd == RemoteCommand::Disconnect { break Ended::Lost; }
                },
            }
        };

        let _ = handle.disconnect().await;
        outputs.stop();
        if let Some(b) = bridge {
            b.abort();
        }
        if let Some((path, task)) = logger
            && let Ok(Ok(lines)) = task.await {
                eprintln!("Logged {lines} packets. Converting...");
                match datafile::parse_file(&path, path.parent(), gzip()) {
                    Ok(r) => report(&r, ""),
                    Err(e) => eprintln!("conversion failed: {e:#}"),
                }
            }
        match ended {
            Ended::Quit => return Ok(()),
            Ended::Lost => {
                eprintln!("Connection ended; reconnecting in 2 s (Ctrl-C to quit)");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
            }
            Ended::Switch(id) => {
                eprintln!("Remote request: switching to {id}");
                config.device = Some(id);
            }
        }
    }
}

// ── web ──────────────────────────────────────────────────────────────────────

/// Open `url` in the default browser. Failure is not fatal: the URL is already printed.
fn open_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    if std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_err()
    {
        eprintln!("Could not open a browser; open {url} yourself.");
    }
}

/// First bindable port at or after `start` on `ip`.
#[cfg(feature = "ws")]
fn free_port(ip: IpAddr, start: u16) -> Result<u16> {
    (start..start.saturating_add(50))
        .find(|p| std::net::TcpListener::bind((ip, *p)).is_ok())
        .with_context(|| format!("no free port in {start}..{}", start.saturating_add(50)))
}

#[cfg(feature = "ws")]
#[allow(clippy::too_many_arguments)]
async fn web_cmd(
    config: EmotiBitClientConfig,
    port: u16,
    bind: Option<IpAddr>,
    no_open: bool,
    web: Option<PathBuf>,
    allow_origin: Vec<String>,
    remote: Option<u16>,
    record_dir: Option<PathBuf>,
    outputs: OutputArgs,
) -> Result<()> {
    let ip = bind.unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let chosen = free_port(ip, port)?;
    if chosen != port {
        eprintln!("Port {port} is in use; using {chosen}.");
    }
    let web = emotibit::webui::resolve(web)?;
    eprintln!(
        "Browser oscilloscope: http://{}:{chosen}/  ({})",
        if ip.is_unspecified() { IpAddr::from([127, 0, 0, 1]) } else { ip },
        if web.is_some() { "UI from disk" } else { "UI built into this binary" }
    );
    // The page needs a connected device to talk to, so the browser opens after that.
    serve(
        config,
        ServeOpts { ws: Some(SocketAddr::new(ip, chosen)), web, allow_origin, remote, record_dir, outputs, open: !no_open },
    )
    .await
}

#[cfg(not(feature = "ws"))]
#[allow(clippy::too_many_arguments)]
async fn web_cmd(
    _: EmotiBitClientConfig,
    _: u16,
    _: Option<IpAddr>,
    _: bool,
    _: Option<PathBuf>,
    _: Vec<String>,
    _: Option<u16>,
    _: Option<PathBuf>,
    _: OutputArgs,
) -> Result<()> {
    bail!("built without the `ws` feature")
}

// ── serial ───────────────────────────────────────────────────────────────────

#[cfg(feature = "serial")]
#[allow(clippy::too_many_arguments)]
async fn serial_cmd(port: Option<String>, signal: &str, seconds: u64, baud: u32, no_reset: bool, raw: bool, verbose: bool) -> Result<()> {
    use emotibit::serial::{Out, SerialSession, SerialSignal, SerialState};
    use std::io::{Read, Write};

    let signal = SerialSignal::from_name(signal).with_context(|| {
        format!("unknown signal {signal:?}; use {}", SerialSignal::ALL.map(|s| s.name()).join(", "))
    })?;
    let port_name = match port {
        Some(p) => p,
        None => {
            let found: Vec<String> = serialport::available_ports()?
                .into_iter()
                .filter(|p| matches!(&p.port_type, serialport::SerialPortType::UsbPort(i) if i.vid == 0x10C4) && !p.port_name.contains("/tty."))
                .map(|p| p.port_name)
                .collect();
            match found.as_slice() {
                [one] => one.clone(),
                [] => bail!("no CP210x (ESP32 Feather) serial port found; pass --port"),
                _ => bail!("several CP210x ports found ({}); pass --port", found.join(", ")),
            }
        }
    };

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut port = serialport::new(&port_name, baud)
            .timeout(Duration::from_millis(100))
            .open()
            .with_context(|| format!("could not open {port_name} (is another program using it?)"))?;
        eprintln!("Reading {} from {port_name}", signal.name());
        // The usual ESP32 auto-reset: pulse EN via RTS with DTR (GPIO0) released.
        let reset = |port: &mut Box<dyn serialport::SerialPort>| -> Result<()> {
            port.write_data_terminal_ready(false)?;
            port.write_request_to_send(true)?;
            std::thread::sleep(Duration::from_millis(100));
            port.write_request_to_send(false)?;
            Ok(())
        };
        let mut last_reset = Instant::now();
        let mut cycles = 1;
        const MAX_CYCLES: u32 = 4;
        // Debug-mode input on this firmware is unreliable (keys are sometimes ignored for a whole
        // boot), so a handshake that has not produced values soon after entering debug mode is
        // abandoned and the board is rebooted for another go.
        const DEBUG_PATIENCE: Duration = Duration::from_secs(45);
        let mut starting_since: Option<Instant> = None;
        if no_reset {
            eprintln!("Not resetting: press the Feather's Reset button now.");
        } else {
            eprintln!("Resetting the board (it will reboot and rejoin WiFi)...");
            reset(&mut port)?;
        }

        let start = Instant::now();
        let ms = |s: &Instant| s.elapsed().as_secs_f64() * 1000.0;
        let mut session = SerialSession::new(signal);
        let mut values: Vec<(f64, f64)> = Vec::new();
        let mut streaming_since: Option<Instant> = None;
        let mut buf = [0u8; 1024];
        let handle = |outs: Vec<Out>,
                          port: &mut Box<dyn serialport::SerialPort>,
                          values: &mut Vec<(f64, f64)>,
                          streaming_since: &mut Option<Instant>|
         -> Result<()> {
            for o in outs {
                match o {
                    Out::Send(b) => {
                        if verbose {
                            eprintln!("  host: sent {:?}", String::from_utf8_lossy(&b));
                        }
                        port.write_all(&b)?;
                        port.flush()?;
                    }
                    Out::Value(v) => {
                        let t = ms(&start);
                        if raw {
                            println!("{t:.0}\t{v}");
                        }
                        values.push((t, v));
                    }
                    Out::Log(l) if verbose => eprintln!("  board: {l}"),
                    Out::Log(_) => {}
                    Out::State(s) => {
                        if s == SerialState::Streaming {
                            *streaming_since = Some(Instant::now());
                        }
                        eprintln!(
                            "{}",
                            match s {
                                SerialState::WaitingForBoot => "waiting for the board to boot",
                                SerialState::EnteringDebug => "boot prompt seen; entering debug mode",
                                SerialState::Starting => "debug mode on; starting the data stream",
                                SerialState::Streaming => "streaming",
                            }
                        );
                    }
                }
            }
            Ok(())
        };

        // The handshake has to finish first, so the read window starts when values do.
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            if let Some(t0) = streaming_since {
                if t0.elapsed() >= Duration::from_secs(seconds) {
                    break;
                }
            } else if !no_reset {
                // Restart the whole cycle if the boot prompt never came, or debug mode never
                // produced values.
                let stalled = match session.state() {
                    SerialState::WaitingForBoot => last_reset.elapsed() > Duration::from_secs(12),
                    SerialState::EnteringDebug | SerialState::Starting => {
                        starting_since.is_some_and(|t| t.elapsed() > DEBUG_PATIENCE)
                    }
                    SerialState::Streaming => false,
                };
                if stalled {
                    if cycles >= MAX_CYCLES {
                        bail!(
                            "no data after {MAX_CYCLES} reboot attempts. The firmware's debug input is unreliable on some boots; \
                             try again, or use --verbose to watch the board (WiFi credentials are hidden)."
                        );
                    }
                    cycles += 1;
                    eprintln!("Stream did not start; rebooting the board (attempt {cycles} of {MAX_CYCLES})...");
                    reset(&mut port)?;
                    last_reset = Instant::now();
                    starting_since = None;
                    session = SerialSession::new(signal);
                }
            } else if start.elapsed() > Duration::from_secs(120) {
                bail!("no debug-mode handshake after 120 s; did you press the Feather's Reset button?");
            }
            match port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let outs = session.feed(&String::from_utf8_lossy(&buf[..n]), ms(&start));
                    handle(outs, &mut port, &mut values, &mut streaming_since)?;
                    if starting_since.is_none() && session.state() == SerialState::EnteringDebug {
                        starting_since = Some(Instant::now());
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e.into()),
            }
            let outs = session.tick(ms(&start));
            handle(outs, &mut port, &mut values, &mut streaming_since)?;
        }

        if values.is_empty() {
            eprintln!("No values received.");
            return Ok(());
        }
        let n = values.len() as f64;
        let span = (values.last().unwrap().0 - values.first().unwrap().0) / 1000.0;
        let (lo, hi) = values.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), (_, v)| (l.min(*v), h.max(*v)));
        let mean = values.iter().map(|(_, v)| v).sum::<f64>() / n;
        let c = emotibit::types::channel(signal.tag());
        println!(
            "{} ({}): {} values in {:.1} s ({:.1}/s)  min {lo}  mean {mean:.6}  max {hi}  last {} {}",
            signal.name(),
            c.map_or("", |c| c.name),
            values.len(),
            span,
            if span > 0.0 { n / span } else { 0.0 },
            values.last().unwrap().1,
            c.map_or("", |c| c.unit)
        );
        Ok(())
    })
    .await?
}

#[cfg(not(feature = "serial"))]
async fn serial_cmd(_: Option<String>, _: &str, _: u64, _: u32, _: bool, _: bool, _: bool) -> Result<()> {
    bail!("built without the `serial` feature")
}
