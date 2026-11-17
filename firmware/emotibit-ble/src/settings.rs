//! Board settings kept in the ESP32's own flash, so they can be set over Bluetooth (`CS`) and
//! survive a reboot without an SD card. A `CONFIG.TXT` on the card overrides them (see
//! `sdcard::read_config`).
//!
//! One 4 KB sector at the start of the `nvs` partition, which this firmware does not otherwise
//! use: `"EBCF"`, format version, name length, name (up to 9 bytes), PPG Hz, LEDs (0/1), then
//! (format 2) the other numeric settings of `tuning::NUMBERS` as little-endian `i16`s
//! (`i16::MIN`: not set), and a one-byte sum. Format 1 records (without those) are still read.

use embedded_storage::{ReadStorage, Storage};

use crate::control::Config;
use crate::stock::Fixed;
use crate::tuning::{n, N, NUMBERS};

/// Flash offset of the settings sector (the `nvs` partition in `partitions.csv`, and in the
/// default table of older images).
pub const OFFSET: u32 = 0x9000;
const MAGIC: &[u8; 4] = b"EBCF";
const VERSION: u8 = 2;
const LEN_V1: usize = 4 + 1 + 1 + 9 + 1 + 1 + 1;
/// Where the numbers after the PPG rate start.
const NUMBERS_AT: usize = 17;
const LEN: usize = NUMBERS_AT + 2 * (N - 1) + 1;
const UNSET: i16 = i16::MIN;

fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
}

/// Byte offset of numeric setting `i` (not the PPG rate, which has its own byte).
fn at(i: usize) -> usize {
    NUMBERS_AT + 2 * (i - 1)
}

/// Settings in a stored record, if it is one.
fn parse(b: &[u8; LEN]) -> Option<Config> {
    let len = match b[4] {
        1 => LEN_V1,
        2 => LEN,
        _ => return None,
    };
    if &b[..4] != MAGIC || sum(&b[..len - 1]) != b[len - 1] {
        return None;
    }
    let count = (b[5] as usize).min(9);
    let mut cfg = Config::default();
    if count > 0 {
        let mut name = Fixed::<24>::new();
        let _ = core::fmt::Write::write_str(&mut name, core::str::from_utf8(&b[6..6 + count]).ok()?);
        cfg.name = Some(name);
    }
    cfg.values[n::PPG_HZ] = Some(b[15] as i16).filter(|v| NUMBERS[n::PPG_HZ].allowed.contains(*v));
    cfg.leds = match b[16] {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    if len == LEN {
        for i in 1..N {
            let v = i16::from_le_bytes([b[at(i)], b[at(i) + 1]]);
            cfg.values[i] = Some(v).filter(|v| *v != UNSET && NUMBERS[i].allowed.contains(*v));
        }
    }
    Some(cfg)
}

/// Settings stored in flash, if any.
pub fn load<S: ReadStorage>(flash: &mut S) -> Option<Config> {
    let mut b = [0u8; LEN];
    flash.read(OFFSET, &mut b).ok()?;
    parse(&b)
}

/// The CPU clock stored in flash, read before `esp_hal::init` (which sets the clock) through the
/// ROM's flash functions; the second core is not running yet.
pub fn early_cpu_mhz() -> Option<i16> {
    let mut words = [0u32; LEN.div_ceil(4)];
    // SAFETY: the read stays inside the flash and fills a word-aligned buffer of at least `LEN`
    // bytes; nothing else uses the flash this early.
    unsafe { esp_storage::ll::spiflash_read(OFFSET, words.as_mut_ptr(), (words.len() * 4) as u32) }.ok()?;
    let mut b = [0u8; LEN];
    for (chunk, w) in b.chunks_mut(4).zip(words) {
        chunk.copy_from_slice(&w.to_le_bytes()[..chunk.len()]);
    }
    parse(&b)?.values[n::CPU_MHZ]
}

/// Erases the stored settings.
pub fn clear<S: Storage>(flash: &mut S) -> Result<(), S::Error> {
    flash.write(OFFSET, &[0u8; LEN])
}

/// Writes `cfg` (absent fields as "not set").
pub fn save<S: Storage>(flash: &mut S, cfg: &Config) -> Result<(), S::Error> {
    let mut b = [0u8; LEN];
    b[..4].copy_from_slice(MAGIC);
    b[4] = VERSION;
    if let Some(name) = &cfg.name {
        let count = name.as_str().len().min(9);
        b[5] = count as u8;
        b[6..6 + count].copy_from_slice(&name.as_bytes()[..count]);
    }
    b[15] = cfg.values[n::PPG_HZ].map_or(0, |v| v as u8);
    b[16] = match cfg.leds {
        Some(false) => 0,
        Some(true) => 1,
        None => 255,
    };
    for i in 1..N {
        b[at(i)..at(i) + 2].copy_from_slice(&cfg.values[i].unwrap_or(UNSET).to_le_bytes());
    }
    b[LEN - 1] = sum(&b[..LEN - 1]);
    flash.write(OFFSET, &b)
}
