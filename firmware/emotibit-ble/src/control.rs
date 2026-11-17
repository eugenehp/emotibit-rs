//! Host commands and the device state they change.
//!
//! The host writes stock EmotiBit packet lines (`timestamp,number,count,TAG,1,100,...`) to the
//! control characteristic, the same packets the stock firmware takes over its TCP control
//! channel: `RB`/`RE` (SD recording), `UN` (note), `LM`/`TX` (LSL markers), `TL`/`AK` (time
//! sync replies) and `MN`/`ML`/`MM`/`MO`/`MH` (power modes). The device answers with stock lines
//! on the events characteristic: `EM` (mode), `RD` (time-sync request), and the `TL`/`AK` it
//! received, restamped with its own clock as the stock firmware writes them to the SD card.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8, Ordering::Relaxed};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as M;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;

use crate::stock::{self, Fixed, Line};
use crate::tuning::{self, n};

/// One stock packet line (file downloads use long ones).
pub type Text = Fixed<480>;

/// Lines for the host, sent on the events characteristic.
pub static EVENTS: Channel<M, Text, 16> = Channel::new();

/// Power modes, as the stock firmware has them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Power {
    /// Stream and record.
    Normal,
    /// Record only; the link stays up for commands and status.
    Low,
    /// Like `Low`, with a slow connection interval to save radio power.
    MaxLow,
    /// Bluetooth off (recording continues); a short button press turns it back on.
    WirelessOff,
    /// Stop recording, power the sensors down, deep sleep until reset.
    Hibernate,
}

impl Power {
    pub fn tag(self) -> &'static str {
        match self {
            Power::Normal => "MN",
            Power::Low => "ML",
            Power::MaxLow => "MM",
            Power::WirelessOff => "MO",
            Power::Hibernate => "MH",
        }
    }

    fn from_tag(t: &str) -> Option<Self> {
        Some(match t {
            "MN" => Power::Normal,
            "ML" => Power::Low,
            "MM" => Power::MaxLow,
            "MO" => Power::WirelessOff,
            "MH" => Power::Hibernate,
            _ => return None,
        })
    }

    fn from_u8(v: u8) -> Self {
        [Power::Normal, Power::Low, Power::MaxLow, Power::WirelessOff, Power::Hibernate][(v as usize).min(4)]
    }
}

static POWER: AtomicU8 = AtomicU8::new(0);

pub fn power() -> Power {
    Power::from_u8(POWER.load(Relaxed))
}

pub fn set_power(p: Power) {
    if power() != p {
        log::info!("power mode {:?}", p);
        POWER.store(p as u8, Relaxed);
    }
}

/// Whether sensor records go out over Bluetooth (not during a firmware update).
pub fn streaming() -> bool {
    power() == Power::Normal && !crate::ota::ACTIVE.load(Relaxed)
}

// ── SD card ──────────────────────────────────────────────────────────────────

/// A card answered at boot.
pub static SD_PRESENT: AtomicBool = AtomicBool::new(false);
/// A file is open.
pub static RECORDING: AtomicBool = AtomicBool::new(false);
/// Name of the open file, for `EM`.
pub static FILE_NAME: Mutex<M, RefCell<Fixed<48>>> = Mutex::new(RefCell::new(Fixed::new()));
/// Set when recording starts, so the sensor task sends calibration words again right away.
pub static RESEND_CAL: AtomicBool = AtomicBool::new(false);
/// The host asked for a self-test (`ST`); the sensor task answers.
pub static SELF_TEST: AtomicBool = AtomicBool::new(false);

pub enum SdCmd {
    /// Start a file. `stamp` is the host's local time (`2026-11-07_01-20-44-744868`).
    Begin { stamp: Fixed<40>, binary: bool },
    End,
    /// A stock line to write (notes, markers, time sync).
    Line(Text),
    /// List the root directory: `FL,<name>,<size>` lines, then `FL` alone.
    List,
    /// Send `len` bytes (0: to the end) of a file from `offset`: `FG,<name>,<size>`,
    /// `FD,<offset>,<base64>` lines, then `FE,<name>,<end offset>`.
    Get { name: Fixed<48>, offset: u32, len: u32 },
    /// Report the card's fill level now (`SD,<percent>`).
    Fill,
    /// List the recordings (`FS` lines, see `sdcard::sessions`).
    Sessions,
    /// Put the settings into `CONFIG.TXT`'s `EmotiBitBle` section (`None`: remove the section),
    /// keeping the rest of the file; answers `CW,ok|error,...`.
    SaveConfig(Option<Config>),
    /// Write a file (8.3 name): open (create or truncate), append base64 data, close with an
    /// `FC,<name>,<bytes>` answer.
    PutOpen(Fixed<16>),
    PutData(Text),
    PutClose,
    /// Delete one file, by long or 8.3 name: `FX,<name>,deleted|not found|protected|busy recording`.
    Delete(Fixed<48>),
    /// Act on a recording, named by its data file: rename it (`FR`), copy it (`FY`) or delete it
    /// (`FZ`), its `_info.json` with it. `to` is the new name without the extension.
    Session { op: SessionOp, name: Fixed<48>, to: Fixed<40> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionOp {
    Rename,
    Copy,
    Delete,
}

impl SessionOp {
    pub fn tag(self) -> &'static str {
        match self {
            SessionOp::Rename => "FR",
            SessionOp::Copy => "FY",
            SessionOp::Delete => "FZ",
        }
    }
}

pub static SD_CMD: Channel<M, SdCmd, 8> = Channel::new();
/// Sensor records with the device time they were taken, while recording.
pub static SD_DATA: Channel<M, (u32, [u8; 20]), 192> = Channel::new();
/// Records the SD writer could not keep up with.
pub static SD_DROPPED: AtomicU32 = AtomicU32::new(0);

// ── Board state for the LEDs, self-test and info file ─────────────────────────

/// A central is connected.
pub static CONNECTED: AtomicBool = AtomicBool::new(false);
/// The button is held down.
pub static BUTTON_HELD: AtomicBool = AtomicBool::new(false);
/// Last battery charge, percent (255: not measured yet).
pub static BATTERY_PERCENT: AtomicU8 = AtomicU8::new(255);

/// Chips that answered at boot, for the self-test (`found::*` bits).
pub static FOUND: AtomicU32 = AtomicU32::new(0);
pub mod found {
    pub const EDA: u32 = 1 << 0;
    pub const IMU: u32 = 1 << 1;
    pub const PPG: u32 = 1 << 2;
    pub const MAG: u32 = 1 << 3;
    pub const THERMOPILE: u32 = 1 << 4;
    pub const SI7013: u32 = 1 << 5;
    pub const EEPROM: u32 = 1 << 6;
    pub const LEDS: u32 = 1 << 7;
}

/// What the EEPROM says about the board, and the settings in effect.
#[derive(Clone)]
pub struct Board {
    /// `MD-V6-0000356`, or empty when the EEPROM has no variant info.
    pub device_id: Fixed<24>,
    /// `V06a`, or empty.
    pub hardware: Fixed<8>,
    pub sku: Fixed<4>,
    /// Name to advertise after `EmotiBit-BLE-` (device id, a configured name, or MAC bytes).
    pub name: Fixed<24>,
    /// The name without settings: the serial number, or MAC bytes.
    pub default_name: Fixed<24>,
    /// Drive the status LEDs.
    pub leds: bool,
    /// The numeric settings in effect (`tuning::NUMBERS` order); `None`: the radio's default
    /// transmit power. `cpu_mhz` is the clock the board runs at.
    pub values: [Option<i16>; tuning::N],
}

impl Board {
    pub const fn new() -> Self {
        let mut values = [None; tuning::N];
        let mut i = 0;
        while i < tuning::N {
            values[i] = tuning::NUMBERS[i].default;
            i += 1;
        }
        Self { device_id: Fixed::new(), hardware: Fixed::new(), sku: Fixed::new(), name: Fixed::new(), default_name: Fixed::new(), leds: true, values }
    }

    /// Setting `i` in effect, or its default (0 for the radio's own transmit power).
    pub fn get(&self, i: usize) -> i16 {
        self.values[i].or(tuning::NUMBERS[i].default).unwrap_or(0)
    }

    /// PPG output rate, Hz: 25 (stock) or 100 (stock's PPG-100 variant).
    pub fn ppg_hz(&self) -> u8 {
        self.get(n::PPG_HZ) as u8
    }

    /// Takes the settings that apply while running from `cfg`; `None` fields go back to the
    /// defaults. The CPU clock is left alone (it changes at a restart).
    pub fn apply(&mut self, cfg: &Config) {
        self.leds = cfg.leds.unwrap_or(true);
        for i in 0..tuning::N {
            if !tuning::NUMBERS[i].restart {
                self.values[i] = cfg.values[i].or(tuning::NUMBERS[i].default);
            }
        }
    }
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

pub static BOARD: Mutex<M, RefCell<Board>> = Mutex::new(RefCell::new(Board::new()));

/// Settings read from the card's `CONFIG.TXT` (`None`: no card, file or `EmotiBitBle`
/// section), valid once `CONFIG_DONE` is set.
pub static CONFIG: Mutex<M, RefCell<Option<Config>>> = Mutex::new(RefCell::new(None));
/// The card task has looked for `CONFIG.TXT`.
pub static CONFIG_DONE: AtomicBool = AtomicBool::new(false);

/// Settings as stored (flash, or `CONFIG.TXT`'s `"EmotiBitBle"` section); absent ones keep the
/// defaults.
#[derive(Clone, Copy, Default)]
pub struct Config {
    pub name: Option<Fixed<24>>,
    pub leds: Option<bool>,
    /// In `tuning::NUMBERS` order.
    pub values: [Option<i16>; tuning::N],
}

impl Config {
    pub const EMPTY: Self = Self { name: None, leds: None, values: [None; tuning::N] };

    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.leds.is_none() && self.values.iter().all(Option::is_none)
    }
}

pub fn board() -> Board {
    BOARD.lock(|b| b.borrow().clone())
}

// ── Settings over Bluetooth (`CG` get, `CS` set, `CR` restart) ───────────────────

/// Where the settings in effect came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    /// Set over Bluetooth, kept in flash (`settings`).
    Flash,
    /// The card's `CONFIG.TXT`, which overrides flash at boot.
    Card,
}

/// The settings kept in flash, where the ones in effect came from, and whether a change waits
/// for a restart (the advertised name, the CPU clock).
pub static STORED: Mutex<M, RefCell<(Config, Source, bool)>> = Mutex::new(RefCell::new((Config::EMPTY, Source::Default, false)));
/// Settings to write to flash (the main task owns the flash); `None` erases them.
pub static SAVE: Channel<M, Option<Config>, 2> = Channel::new();
/// The PPG rate or LED current changed; the sensor task sets the chip up again.
pub static PPG_CHANGED: AtomicBool = AtomicBool::new(false);
/// The sensor task's wake interval, ms (`sensor_ms`, read on every wake).
pub static SENSOR_INTERVAL: AtomicU8 = AtomicU8::new(2);
/// The host asked for a restart (`CR`).
pub static RESTART: AtomicBool = AtomicBool::new(false);

/// Sends `CG,name,…,leds,…,<each of tuning::NUMBERS>,…,cpu_mhz_next,…,profile,…,source,
/// default|flash|card,restart_needed,0|1,device_id,…`. `cpu_mhz` is the clock the board runs at,
/// `cpu_mhz_next` the one it starts with next time; `tx_dbm` is what the radio uses; `profile`
/// the one these settings match, or `custom`.
pub fn report_settings() {
    use core::fmt::Write as _;
    let b = board();
    let (stored, source, pending) = STORED.lock(|s| *s.borrow());
    let mut nums = [Fixed::<8>::new(); tuning::N];
    for (i, f) in nums.iter_mut().enumerate() {
        let v = if i == n::TX_DBM { crate::radio::tx_dbm() } else { b.get(i) };
        let _ = write!(f, "{v}");
    }
    let mut cpu_next = Fixed::<8>::new();
    let _ = write!(cpu_next, "{}", stored.values[n::CPU_MHZ].or(tuning::NUMBERS[n::CPU_MHZ].default).unwrap_or(0));
    // The profile is matched on what is in effect, with the next start's CPU clock.
    let mut values = b.values;
    values[n::CPU_MHZ] = stored.values[n::CPU_MHZ];
    let profile = tuning::matching(b.leds, &values).map_or("custom", |p| p.name);
    let src = match source {
        Source::Default => "default",
        Source::Flash => "flash",
        Source::Card => "card",
    };
    let mut fields: [&str; 2 * (tuning::N + 7)] = [""; 2 * (tuning::N + 7)];
    let mut at = 0;
    for (key, value) in [("name", b.name.as_str()), ("leds", if b.leds { "1" } else { "0" })] {
        fields[at] = key;
        fields[at + 1] = value;
        at += 2;
    }
    for (i, f) in nums.iter().enumerate() {
        fields[at] = tuning::NUMBERS[i].key;
        fields[at + 1] = f.as_str();
        at += 2;
    }
    for (key, value) in [
        ("cpu_mhz_next", cpu_next.as_str()),
        ("profile", profile),
        ("source", src),
        ("restart_needed", if pending { "1" } else { "0" }),
        ("device_id", b.device_id.as_str()),
    ] {
        fields[at] = key;
        fields[at + 1] = value;
        at += 2;
    }
    emit("CG", &fields[..at], true);
}

/// Where `CS` keeps the settings: flash (default), the card's `CONFIG.TXT`, or both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Store {
    Flash,
    Card,
    Both,
}

fn store(v: &str) -> Option<Store> {
    Some(match v {
        "flash" | "rom" => Store::Flash,
        "sd" | "card" => Store::Card,
        "both" => Store::Both,
        _ => return None,
    })
}

/// `CC,flash|sd|both`: removes the stored settings (the flash record, or the card's
/// `EmotiBitBle` section). They stay in effect until a restart.
fn clear_settings(fields: &[&str]) {
    let Some(target) = store(fields.first().copied().unwrap_or("flash")) else {
        emit("CC", &["error", "store: flash or sd or both"], true);
        return;
    };
    if target != Store::Card {
        let _ = SAVE.try_send(None);
    }
    if target != Store::Flash {
        let _ = SD_CMD.try_send(SdCmd::SaveConfig(None));
    }
    // Where the settings would come from at the next boot.
    STORED.lock(|s| {
        let mut s = s.borrow_mut();
        if target != Store::Card {
            s.0 = Config::default();
        }
        let flash_has = !s.0.is_empty();
        s.1 = match (s.1, target) {
            (Source::Card, Store::Flash) => Source::Card,
            _ if flash_has => Source::Flash,
            _ => Source::Default,
        };
    });
    emit("CC", &["ok"], true);
}

/// `CS,key,value,...`: `name` (up to 9 letters, digits, '-' or '_'; empty restores the default),
/// `leds` (0/1), any of `tuning::NUMBERS`, `profile` (`tuning::PROFILES`: sets the LEDs and every
/// number; keys after it override it), and `store` (`flash`, the default, `sd` or `both`). The
/// CPU clock is always kept in flash too, since it is set before the card can be read. The name
/// and CPU clock apply after a restart, the rest at once.
fn set_settings(fields: &[&str]) {
    use core::fmt::Write as _;
    let (mut cfg, _, _) = STORED.lock(|s| *s.borrow());
    let mut target = Store::Flash;
    let mut why = Fixed::<96>::new();
    for kv in fields.chunks(2) {
        let [key, value] = kv else {
            emit("CS", &["error", "fields come in key/value pairs"], true);
            return;
        };
        match *key {
            "name" => {
                if value.len() > 9 || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    emit("CS", &["error", "name: up to 9 letters or digits or - or _"], true);
                    return;
                }
                let mut name = Fixed::<24>::new();
                let _ = name.write_str(value);
                cfg.name = (!value.is_empty()).then_some(name);
            }
            "leds" => match *value {
                "1" | "true" => cfg.leds = Some(true),
                "0" | "false" => cfg.leds = Some(false),
                _ => {
                    emit("CS", &["error", "leds: 0 or 1"], true);
                    return;
                }
            },
            "profile" => match tuning::profile(value) {
                Some(p) => {
                    cfg.leds = Some(p.leds);
                    cfg.values = p.values;
                }
                None => {
                    let _ = why.write_str("profile:");
                    for p in &tuning::PROFILES {
                        let _ = write!(why, " {}", p.name);
                    }
                    emit("CS", &["error", why.as_str()], true);
                    return;
                }
            },
            "store" => match store(value) {
                Some(t) => target = t,
                None => {
                    emit("CS", &["error", "store: flash or sd or both"], true);
                    return;
                }
            },
            key => {
                let Some(i) = tuning::number(key) else {
                    emit("CS", &["error", "unknown setting (name; leds; profile; store; ppg_hz; led_brightness; cpu_mhz; sensor_ms; conn_ms; adv_ms; tx_dbm; ppg_led)"], true);
                    return;
                };
                let setting = &tuning::NUMBERS[i];
                match value.parse::<i16>().ok().filter(|v| setting.allowed.contains(*v)) {
                    Some(v) => cfg.values[i] = Some(v),
                    None => {
                        let _ = write!(why, "{}: ", setting.key);
                        let _ = match setting.allowed {
                            tuning::Allowed::Range(lo, hi) => write!(why, "{lo} to {hi}"),
                            tuning::Allowed::OneOf(vs) => vs.iter().enumerate().try_for_each(|(j, v)| write!(why, "{}{v}", if j > 0 { " or " } else { "" })),
                        };
                        emit("CS", &["error", why.as_str()], true);
                        return;
                    }
                }
            }
        }
    }
    let source = STORED.lock(|s| s.borrow().1);
    // A restart is needed when the name to advertise differs from the one on the air, or the
    // CPU clock from the one the board runs at.
    let b = board();
    let wanted = cfg.name.as_ref().map_or(b.default_name.as_str(), |n| n.as_str());
    let name_changed = wanted != b.name.as_str();
    let cpu_changed = cfg.values[n::CPU_MHZ].or(tuning::NUMBERS[n::CPU_MHZ].default) != b.values[n::CPU_MHZ];
    // Apply what can change while running.
    BOARD.lock(|b| {
        let mut b = b.borrow_mut();
        let before = b.clone();
        b.apply(&cfg);
        if b.get(n::PPG_HZ) != before.get(n::PPG_HZ) || b.get(n::PPG_LED) != before.get(n::PPG_LED) {
            PPG_CHANGED.store(true, Relaxed);
        }
        SENSOR_INTERVAL.store(b.get(n::SENSOR_MS) as u8, Relaxed);
    });
    if cfg.values[n::TX_DBM] != b.values[n::TX_DBM] && !crate::radio::set_tx_dbm(cfg.values[n::TX_DBM]) {
        log::warn!("settings: the radio refused transmit power {:?} dBm", cfg.values[n::TX_DBM]);
    }
    STORED.lock(|s| {
        let mut s = s.borrow_mut();
        s.0 = cfg;
        s.1 = match (target, source) {
            (Store::Flash, Source::Card) => Source::Card,
            (Store::Flash, _) => Source::Flash,
            _ => Source::Card,
        };
        s.2 = name_changed || cpu_changed;
    });
    // The CPU clock is read from flash at boot, wherever the rest is kept.
    if target != Store::Card || cpu_changed {
        let _ = SAVE.try_send(Some(cfg));
    }
    if target != Store::Flash {
        // The card task answers with `CW` once CONFIG.TXT is written.
        let _ = SD_CMD.try_send(SdCmd::SaveConfig(Some(cfg)));
    }
    let note = match (target, source) {
        (Store::Flash, Source::Card) => "saved to flash; note: CONFIG.TXT on the card overrides these at boot",
        (Store::Flash, _) => "saved to flash",
        (Store::Card, _) => "saving to CONFIG.TXT",
        (Store::Both, _) => "saved to flash; saving to CONFIG.TXT",
    };
    emit("CS", &["ok", note], true);
    report_settings();
}

// ── Clipping and overflow (`DC` / `DO`, as the stock firmware reports them) ──────

/// Stock tags whose clipping and overflow are counted.
pub const COUNTED: [&str; 16] = ["EA", "EL", "PR", "PI", "PG", "AX", "AY", "AZ", "GX", "GY", "GZ", "MX", "MY", "MZ", "TH", "T1"];
static CLIPPED: [AtomicU16; 16] = [const { AtomicU16::new(0) }; 16];
static OVERFLOWED: [AtomicU16; 16] = [const { AtomicU16::new(0) }; 16];

fn index(tag: &str) -> Option<usize> {
    COUNTED.iter().position(|t| *t == tag)
}

/// A sample of `tag` hit the sensor's range limit.
pub fn clipped(tag: &str) {
    if let Some(i) = index(tag) {
        CLIPPED[i].fetch_add(1, Relaxed);
    }
}

/// A record (raw tag, see `stock::id`) could not be queued for the link or the card.
pub fn overflowed(record_tag: u8) {
    for t in crate::stock::tags_of(record_tag) {
        if let Some(i) = index(t) {
            OVERFLOWED[i].fetch_add(1, Relaxed);
        }
    }
}

/// Sends `DC` and `DO` lines (`TAG,count,...`) for what was counted since the last call.
pub fn report_clipping_and_overflow() {
    for (tag, counts) in [("DC", &CLIPPED), ("DO", &OVERFLOWED)] {
        let mut fields: [Fixed<8>; 32] = [const { Fixed::new() }; 32];
        let mut n = 0;
        for (i, c) in counts.iter().enumerate() {
            let v = c.swap(0, Relaxed);
            if v > 0 && n + 1 < fields.len() {
                let _ = core::fmt::Write::write_str(&mut fields[n], COUNTED[i]);
                let _ = core::fmt::write(&mut fields[n + 1], format_args!("{v}"));
                n += 2;
            }
        }
        if n > 0 {
            let refs: [&str; 32] = core::array::from_fn(|i| fields[i].as_str());
            emit(tag, &refs[..n], true);
        }
    }
}

// ── Lines ────────────────────────────────────────────────────────────────────

/// Packet numbers of lines sent to the host, consecutive so it can count gaps.
static NUMBER: AtomicU16 = AtomicU16::new(0);
/// Packet numbers of lines that only go to the SD card (data, notes, markers).
static SD_NUMBER: AtomicU16 = AtomicU16::new(0);

pub fn now_ms() -> u32 {
    embassy_time::Instant::now().as_millis() as u32
}

pub fn next_number() -> u16 {
    NUMBER.fetch_add(1, Relaxed)
}

pub fn next_sd_number() -> u16 {
    SD_NUMBER.fetch_add(1, Relaxed)
}

/// A stock line stamped with the device clock and the next host packet number.
pub fn line(tag: &str, payload: &[&str]) -> Text {
    let mut t = Text::new();
    let _ = stock::write_line(&mut t, now_ms(), next_number(), tag, payload);
    t
}

/// Sends a line to the host (if `host`) and to the SD file (while recording).
pub fn emit(tag: &str, payload: &[&str], host: bool) {
    let mut t = Text::new();
    let number = if host { next_number() } else { next_sd_number() };
    let _ = stock::write_line(&mut t, now_ms(), number, tag, payload);
    if RECORDING.load(Relaxed) {
        let mut copy = Text::new();
        let _ = core::fmt::Write::write_str(&mut copy, t.as_str());
        let _ = SD_CMD.try_send(SdCmd::Line(copy));
    }
    if host {
        let _ = EVENTS.try_send(t);
    }
}

/// The `EM` mode line: `RS,RE,PS,MN` or `RS,RB,<file>,PS,MN`.
pub fn mode_line() -> Text {
    let file = FILE_NAME.lock(|f| {
        let mut c = Fixed::<48>::new();
        let _ = core::fmt::Write::write_str(&mut c, f.borrow().as_str());
        c
    });
    if RECORDING.load(Relaxed) {
        line("EM", &["RS", "RB", file.as_str(), "PS", power().tag()])
    } else {
        line("EM", &["RS", "RE", "PS", power().tag()])
    }
}

/// Handles bytes written to the control characteristic: one or more stock packet lines.
pub fn handle(bytes: &[u8]) {
    let Ok(text) = core::str::from_utf8(bytes) else { return };
    for l in text.split('\n').filter_map(Line::parse) {
        command(&l);
    }
}

fn command(l: &Line) {
    let mut fields: [&str; 8] = [""; 8];
    let mut n = 0;
    for f in l.fields().take(8) {
        fields[n] = f;
        n += 1;
    }
    let fields = &fields[..n];
    match l.tag {
        "RB" => {
            if !SD_PRESENT.load(Relaxed) {
                log::warn!("record begin: no SD card");
                return;
            }
            let mut stamp = Fixed::<40>::new();
            let _ = core::fmt::Write::write_str(&mut stamp, fields.first().copied().unwrap_or(""));
            // Optional format choice after the timestamp: `FMT,BIN` or `FMT,CSV` (default).
            let binary = fields.windows(2).any(|w| w[0] == "FMT" && w[1] == "BIN");
            let _ = SD_CMD.try_send(SdCmd::Begin { stamp, binary });
        }
        "RE" => {
            let _ = SD_CMD.try_send(SdCmd::End);
        }
        "FL" => {
            let _ = SD_CMD.try_send(SdCmd::List);
        }
        "ST" => SELF_TEST.store(true, Relaxed),
        "FS" => {
            let _ = SD_CMD.try_send(SdCmd::Sessions);
        }
        "CG" => report_settings(),
        "CC" => clear_settings(fields),
        "CS" => {
            // Empty values count here (`name,` restores the default name), so split by hand.
            let mut raw: [&str; 32] = [""; 32];
            let mut k = 0;
            for f in l.payload.split(',').take(32) {
                raw[k] = f;
                k += 1;
            }
            set_settings(&raw[..k]);
        }
        "CR" => RESTART.store(true, Relaxed),
        "FO" => {
            let mut name = Fixed::<16>::new();
            if core::fmt::Write::write_str(&mut name, fields.first().copied().unwrap_or("")).is_ok() {
                let _ = SD_CMD.try_send(SdCmd::PutOpen(name));
            }
        }
        "FX" => {
            let mut name = Fixed::<48>::new();
            if core::fmt::Write::write_str(&mut name, fields.first().copied().unwrap_or("")).is_ok() {
                let _ = SD_CMD.try_send(SdCmd::Delete(name));
            }
        }
        "FR" | "FY" | "FZ" => {
            let op = match l.tag {
                "FR" => SessionOp::Rename,
                "FY" => SessionOp::Copy,
                _ => SessionOp::Delete,
            };
            let mut name = Fixed::<48>::new();
            let mut to = Fixed::<40>::new();
            let first = fields.first().copied().unwrap_or("");
            if core::fmt::Write::write_str(&mut name, first).is_err() {
                emit(l.tag, &[first, "error", "name too long"], true);
            } else if core::fmt::Write::write_str(&mut to, fields.get(1).copied().unwrap_or("")).is_err() {
                emit(l.tag, &[first, "error", "new name too long"], true);
            } else {
                let _ = SD_CMD.try_send(SdCmd::Session { op, name, to });
            }
        }
        "FA" => {
            let mut data = Text::new();
            let _ = core::fmt::Write::write_str(&mut data, fields.first().copied().unwrap_or(""));
            let _ = SD_CMD.try_send(SdCmd::PutData(data));
        }
        "FC" => {
            let _ = SD_CMD.try_send(SdCmd::PutClose);
        }
        "FG" => {
            let mut name = Fixed::<48>::new();
            let offset = fields.get(1).and_then(|o| o.parse().ok()).unwrap_or(0);
            let len = fields.get(2).and_then(|o| o.parse().ok()).unwrap_or(0);
            if core::fmt::Write::write_str(&mut name, fields.first().copied().unwrap_or("")).is_ok() {
                let _ = SD_CMD.try_send(SdCmd::Get { name, offset, len });
            }
        }
        // Notes and LSL markers go to the file, restamped with the device clock.
        "UN" | "LM" | "TX" => emit(l.tag, fields, false),
        // Time-sync replies: to the file, and back to the host so its own log has them too.
        "TL" | "AK" => emit(l.tag, fields, true),
        t => {
            if let Some(p) = Power::from_tag(t) {
                set_power(p);
            }
        }
    }
}
