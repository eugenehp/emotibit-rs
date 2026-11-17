//! Bluetooth transmit power, through the ESP32 controller's own functions (the ones ESP-IDF's
//! `esp_ble_tx_power_set`/`_get` call; esp-radio has no API for them on the ESP32). Only valid
//! once the controller is running.

use core::sync::atomic::{AtomicI8, Ordering::Relaxed};

use crate::tuning::TX_DBM;

unsafe extern "C" {
    fn ble_txpwr_set(power_type: i32, power_level: i32) -> i32;
    fn ble_txpwr_get(power_type: i32) -> i32;
}

/// `esp_ble_power_type_t`: connection handles 0-8, then advertising, scanning, the default.
const CONN_HANDLES: core::ops::Range<i32> = 0..9;
const ADV: i32 = 9;
const DEFAULT: i32 = 11;

/// The controller's own level, read before any change (-1: not read yet).
static RADIO_DEFAULT: AtomicI8 = AtomicI8::new(-1);

/// Records the controller's default level; call once the controller is up.
pub fn init() {
    // SAFETY: the controller is running; the call only reads its setting.
    let level = unsafe { ble_txpwr_get(DEFAULT) };
    if (0..TX_DBM.len() as i32).contains(&level) {
        RADIO_DEFAULT.store(level as i8, Relaxed);
    }
}

/// Sets the transmit power for advertising and every connection; `None` puts the controller's
/// default back. False if the controller refused it.
pub fn set_tx_dbm(dbm: Option<i16>) -> bool {
    let level = match dbm {
        Some(d) => match TX_DBM.iter().position(|v| *v == d) {
            Some(l) => l as i32,
            None => return false,
        },
        None => match RADIO_DEFAULT.load(Relaxed) {
            l if l >= 0 => l as i32,
            _ => return true,
        },
    };
    // SAFETY: the controller is running and `level` is one of its levels (0-7).
    unsafe {
        let ok = ble_txpwr_set(DEFAULT, level) == 0 && ble_txpwr_set(ADV, level) == 0;
        for handle in CONN_HANDLES {
            ble_txpwr_set(handle, level);
        }
        ok
    }
}

/// The transmit power in use for connections, dBm.
pub fn tx_dbm() -> i16 {
    // SAFETY: the controller is running; the call only reads its setting.
    let level = unsafe { ble_txpwr_get(DEFAULT) };
    TX_DBM.get(level as usize).copied().unwrap_or(0)
}
