//! Board settings kept in the ESP32's own flash, so they can be set over Bluetooth (`CS`) and
//! survive a reboot without an SD card. A `CONFIG.TXT` on the card overrides them (see
//! `sdcard::read_config`).
//!
//! One 4 KB sector at the start of the `nvs` partition, which this firmware does not otherwise
//! use: `"EBCF"`, format version, name length, name (up to 9 bytes), PPG Hz, LEDs (0/1), and a
//! one-byte sum.

use embedded_storage::{ReadStorage, Storage};

use crate::control::Config;
use crate::stock::Fixed;

/// Flash offset of the settings sector (the default partition table's `nvs` partition).
pub const OFFSET: u32 = 0x9000;
const MAGIC: &[u8; 4] = b"EBCF";
const VERSION: u8 = 1;
const LEN: usize = 4 + 1 + 1 + 9 + 1 + 1 + 1;

fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
}

/// Settings stored in flash, if any.
pub fn load<S: ReadStorage>(flash: &mut S) -> Option<Config> {
    let mut b = [0u8; LEN];
    flash.read(OFFSET, &mut b).ok()?;
    if &b[..4] != MAGIC || b[4] != VERSION || sum(&b[..LEN - 1]) != b[LEN - 1] {
        return None;
    }
    let n = (b[5] as usize).min(9);
    let mut cfg = Config::default();
    if n > 0 {
        let mut name = Fixed::<24>::new();
        let _ = core::fmt::Write::write_str(&mut name, core::str::from_utf8(&b[6..6 + n]).ok()?);
        cfg.name = Some(name);
    }
    cfg.ppg_hz = match b[15] {
        25 | 100 => Some(b[15]),
        _ => None,
    };
    cfg.leds = match b[16] {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    Some(cfg)
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
        let n = name.as_str().len().min(9);
        b[5] = n as u8;
        b[6..6 + n].copy_from_slice(&name.as_bytes()[..n]);
    }
    b[15] = cfg.ppg_hz.unwrap_or(0);
    b[16] = match cfg.leds {
        Some(false) => 0,
        Some(true) => 1,
        None => 255,
    };
    b[LEN - 1] = sum(&b[..LEN - 1]);
    flash.write(OFFSET, &b)
}
