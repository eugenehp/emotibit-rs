//! The EmotiBit board around the sensors: its identity in the EEPROM, and the status LEDs.

use core::fmt::Write as _;

use embedded_hal_async::i2c::I2c;

use crate::stock::Fixed;

// ── Identity (EEPROM at 0x50, as the stock firmware's `EmotiBitNvmController` writes it) ──

const EEPROM: u8 = 0x50;

/// Hardware version names, by the stock `EmotiBitVersion` number.
const HW_NAMES: [&str; 10] = ["V01b", "V01c", "V02b", "V02f", "V02h", "V03b", "V04a", "V05c", "V06a", "V07a"];

/// What the EEPROM says about the board.
pub struct Identity {
    /// `MD-V6-0000356`; empty for boards whose EEPROM has no serial number (V2/V3).
    pub device_id: Fixed<24>,
    /// `V06a`.
    pub hardware: Fixed<8>,
    /// Stock `EmotiBitVersion` number (8 = V06a).
    pub hw_version: u8,
    pub sku: Fixed<4>,
}

async fn read<I: I2c>(i2c: &mut I, addr: u8, out: &mut [u8]) -> bool {
    // The 256-byte EEPROM takes a one-byte address; read in small pieces.
    for (k, chunk) in out.chunks_mut(16).enumerate() {
        if i2c.write_read(EEPROM, &[addr.wrapping_add((k * 16) as u8)], chunk).await.is_err() {
            return false;
        }
    }
    true
}

/// Reads the variant info: a memory map at byte 1 (`address: u32, size: u32` per data type,
/// little endian), variant info first; its last byte is the format version. Format 1 is
/// `serial: u32, sku: [u8; 3], hw_version: u8`; format 0 is `hw_version: u8`.
pub async fn identity<I: I2c>(i2c: &mut I) -> Option<Identity> {
    let mut count = [0u8];
    if !read(i2c, 0, &mut count).await || count[0] == 0 || count[0] == 255 {
        return None;
    }
    let mut map = [0u8; 8];
    if !read(i2c, 1, &mut map).await {
        return None;
    }
    let address = u32::from_le_bytes(map[0..4].try_into().ok()?);
    let size = u32::from_le_bytes(map[4..8].try_into().ok()?) as usize;
    if address > 255 || !(2..=16).contains(&size) {
        return None;
    }
    let mut data = [0u8; 16];
    if !read(i2c, address as u8, &mut data[..size]).await {
        return None;
    }
    let version = data[size - 1];
    let mut id = Identity { device_id: Fixed::new(), hardware: Fixed::new(), hw_version: 0, sku: Fixed::new() };
    match version {
        1 if size == 9 => {
            let serial = u32::from_le_bytes(data[0..4].try_into().ok()?);
            id.hw_version = data[7];
            for &c in &data[4..7] {
                if c.is_ascii_alphanumeric() {
                    let _ = id.sku.write_char(c as char);
                }
            }
            let hw = HW_NAMES.get(id.hw_version as usize)?;
            let _ = id.hardware.write_str(hw);
            // Barcode form: SKU-V<major>-<7-digit serial>, as the stock firmware builds it.
            let major: u32 = hw[1..3].parse().ok()?;
            let _ = write!(id.device_id, "{}-V{}-{:07}", id.sku.as_str(), major, serial);
        }
        0 => {
            id.hw_version = data[0];
            let _ = id.hardware.write_str(HW_NAMES.get(id.hw_version as usize)?);
            let _ = id.sku.write_str("MD"); // V2/V3 boards were all MD
        }
        _ => return None,
    }
    Some(id)
}

// ── Status LEDs: NCP5623 on V4-V6 boards, KTD2026B on V7 (as the stock firmware) ──────────

/// The three LEDs.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Leds {
    pub red: bool,
    pub blue: bool,
    pub yellow: bool,
}

#[derive(Clone, Copy)]
enum Chip {
    /// NCP5623 at 0x38 (V5) or 0x39 (V6): one register per channel.
    Ncp(u8),
    /// KTD2026B at 0x31 (V7): one channel-control register, two bits per channel.
    Ktd,
}

/// The LED driver, with the state and brightness last sent.
pub struct LedDriver {
    chip: Chip,
    sent: Option<Leds>,
    /// 1-31: the NCP5623's PWM level; twice that in the KTD2026's 0.125 mA current steps. The
    /// stock firmware's 8 is PWM 8 of 31, or 2 mA.
    brightness: u8,
}

/// NCP5623 driver current, as the stock firmware sets it.
const NCP_CURRENT: u8 = 1;
const KTD: u8 = 0x31;

/// KTD2026 channel current registers (0x06-0x08) for a brightness.
async fn ktd_current<I: I2c>(i2c: &mut I, brightness: u8) -> bool {
    let mut ok = true;
    for reg in [0x06, 0x07, 0x08] {
        ok &= i2c.write(KTD, &[reg, brightness.clamp(1, 31) * 2]).await.is_ok();
    }
    ok
}

impl LedDriver {
    /// Finds the driver for this hardware version (stock numbering: 7 = V05c, 8 = V06a,
    /// 9 = V07a; unknown: try all) and turns the LEDs off.
    pub async fn new<I: I2c>(i2c: &mut I, hw_version: Option<u8>, brightness: u8) -> Option<Self> {
        let chips: &[Chip] = match hw_version {
            Some(v) if v >= 9 => &[Chip::Ktd],
            Some(v) if v <= 7 => &[Chip::Ncp(0x38), Chip::Ncp(0x39)],
            Some(_) => &[Chip::Ncp(0x39), Chip::Ncp(0x38)],
            None => &[Chip::Ncp(0x39), Chip::Ncp(0x38), Chip::Ktd],
        };
        for &chip in chips {
            let ok = match chip {
                // Register 1 (ILED): the driver current.
                Chip::Ncp(addr) => i2c.write(addr, &[(1 << 5) | NCP_CURRENT]).await.is_ok(),
                Chip::Ktd => {
                    let ok = i2c.write(KTD, &[0x00, 0x00]).await.is_ok(); // enable
                    ok && ktd_current(i2c, brightness).await
                }
            };
            if ok {
                let mut d = Self { chip, sent: None, brightness: brightness.clamp(1, 31) };
                d.set(i2c, Leds::default()).await;
                return Some(d);
            }
        }
        None
    }

    /// Sends what changed.
    pub async fn set<I: I2c>(&mut self, i2c: &mut I, leds: Leds) {
        if self.sent == Some(leds) {
            return;
        }
        let want = [leds.red, leds.blue, leds.yellow];
        match self.chip {
            Chip::Ncp(addr) => {
                let had = self.sent.map(|s| [s.red, s.blue, s.yellow]);
                for (i, &on) in want.iter().enumerate() {
                    if had.is_some_and(|h| h[i] == on) {
                        continue;
                    }
                    // Registers 2-4: one PWM channel each.
                    let reg = (2 + i as u8) << 5;
                    let _ = i2c.write(addr, &[if on { reg | self.brightness } else { reg }]).await;
                }
            }
            Chip::Ktd => {
                // Register 4: channel n on = 0b01 at bits 2n..2n+1.
                let control = want.iter().enumerate().fold(0u8, |c, (i, &on)| c | ((on as u8) << (2 * i)));
                let _ = i2c.write(KTD, &[0x04, control]).await;
            }
        }
        self.sent = Some(leds);
    }

    /// Changes the brightness (1-31) of the LEDs that are on, and of the ones turned on later.
    pub async fn set_brightness<I: I2c>(&mut self, i2c: &mut I, brightness: u8) {
        let brightness = brightness.clamp(1, 31);
        if brightness == self.brightness {
            return;
        }
        self.brightness = brightness;
        match self.chip {
            // The PWM level is sent with each LED: send the lit ones again.
            Chip::Ncp(_) => {
                if let Some(leds) = self.sent.take() {
                    self.set(i2c, leds).await;
                }
            }
            Chip::Ktd => {
                ktd_current(i2c, brightness).await;
            }
        }
    }
}
