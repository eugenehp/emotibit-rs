//! The emotibit-ble firmware's numeric settings (CPU clock, wake interval, radio, LEDs, PPG) and
//! the profiles that set them together. Shared by the firmware, which applies them, and the
//! host, which shows and checks them; `no_std`, no allocation.

/// What a numeric setting accepts.
#[derive(Clone, Copy, Debug)]
pub enum Allowed {
    Range(i16, i16),
    OneOf(&'static [i16]),
}

impl Allowed {
    pub fn contains(&self, v: i16) -> bool {
        match self {
            Allowed::Range(lo, hi) => (*lo..=*hi).contains(&v),
            Allowed::OneOf(vs) => vs.contains(&v),
        }
    }
}

/// One numeric setting.
#[derive(Clone, Copy, Debug)]
pub struct Number {
    /// Key in `CS`/`CG` and in `CONFIG.TXT`'s `EmotiBitBle` section.
    pub key: &'static str,
    /// Value when not set (`tx_dbm`: the radio's own default, reported as `None`).
    pub default: Option<i16>,
    pub allowed: Allowed,
    pub unit: &'static str,
    /// Applies after a restart (the rest apply at once).
    pub restart: bool,
    pub help: &'static str,
}

/// Indexes into [`NUMBERS`] (and every `[_; N]` of values).
pub mod n {
    pub const PPG_HZ: usize = 0;
    pub const LED_BRIGHTNESS: usize = 1;
    pub const CPU_MHZ: usize = 2;
    pub const SENSOR_MS: usize = 3;
    pub const CONN_MS: usize = 4;
    pub const ADV_MS: usize = 5;
    pub const TX_DBM: usize = 6;
    pub const PPG_LED: usize = 7;
}

pub const N: usize = 8;

/// Transmit powers the ESP32's Bluetooth controller has, dBm (its levels 0-7).
pub const TX_DBM: [i16; 8] = [-12, -9, -6, -3, 0, 3, 6, 9];

pub const NUMBERS: [Number; N] = [
    Number { key: "ppg_hz", default: Some(25), allowed: Allowed::OneOf(&[25, 100]), unit: "Hz", restart: false, help: "PPG output rate (stock 25; 100 as the stock PPG-100 variant)" },
    Number { key: "led_brightness", default: Some(8), allowed: Allowed::Range(1, 31), unit: "", restart: false, help: "status LED brightness (stock 8)" },
    Number { key: "cpu_mhz", default: Some(240), allowed: Allowed::OneOf(&[80, 160, 240]), unit: "MHz", restart: true, help: "CPU clock (the stock firmware runs at 80)" },
    Number { key: "sensor_ms", default: Some(2), allowed: Allowed::Range(2, 20), unit: "ms", restart: false, help: "how often the sensor task wakes; EDA is read every 4 ms or the first wake after, the other chips every 10 ms or the first wake after" },
    Number { key: "conn_ms", default: Some(15), allowed: Allowed::Range(15, 500), unit: "ms", restart: false, help: "Bluetooth connection interval asked for (up to this + 15 ms), after the first 5 s of a connection (at most 30 ms until then); longer saves radio power" },
    Number { key: "adv_ms", default: Some(160), allowed: Allowed::Range(20, 10240), unit: "ms", restart: false, help: "advertising interval while not connected; longer saves power, the board takes longer to find (Apple lists 152, 211, 318, 417, 546, 760, 852, 1022 and 1285 as found quickly)" },
    Number { key: "tx_dbm", default: None, allowed: Allowed::OneOf(&TX_DBM), unit: "dBm", restart: false, help: "Bluetooth transmit power; lower saves power, shortens the range" },
    Number { key: "ppg_led", default: Some(47), allowed: Allowed::Range(1, 255), unit: "x 0.2 mA", restart: false, help: "PPG LED current, 0.2 mA steps (stock 47 = 9.4 mA); higher is a stronger signal" },
];

/// A set of values for every numeric setting, and the status LEDs on or off.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub name: &'static str,
    pub help: &'static str,
    pub leds: bool,
    /// In [`NUMBERS`] order; `None` leaves the radio's default (`tx_dbm`).
    pub values: [Option<i16>; N],
}

pub const PROFILES: [Profile; 6] = [
    Profile {
        name: "default",
        help: "as shipped: every chip read as often as it can be, fastest CPU and link",
        leds: true,
        values: [Some(25), Some(8), Some(240), Some(2), Some(15), Some(160), None, Some(47)],
    },
    Profile {
        name: "high_frequency",
        help: "PPG at 100 Hz, fastest CPU and link",
        leds: true,
        values: [Some(100), Some(8), Some(240), Some(2), Some(15), Some(160), Some(3), Some(47)],
    },
    Profile {
        name: "low_noise",
        help: "every EDA conversion averaged, a brighter PPG LED, status LEDs off",
        leds: false,
        values: [Some(25), Some(8), Some(160), Some(2), Some(30), Some(160), Some(3), Some(79)],
    },
    Profile {
        name: "balanced",
        help: "the stock firmware's 80 MHz clock; EDA still read at every conversion; a slower link",
        leds: true,
        values: [Some(25), Some(8), Some(80), Some(4), Some(30), Some(160), Some(0), Some(47)],
    },
    Profile {
        name: "energy_saving",
        help: "80 MHz, sensors every 10 ms, a slow link and advertising, dim LEDs",
        leds: true,
        values: [Some(25), Some(4), Some(80), Some(10), Some(60), Some(318), Some(0), Some(47)],
    },
    Profile {
        name: "low_energy",
        help: "the least power: sensors every 20 ms, a slow link at -6 dBm, a dimmer PPG LED, status LEDs off",
        leds: false,
        values: [Some(25), Some(1), Some(80), Some(20), Some(120), Some(418), Some(-6), Some(31)],
    },
];

pub fn number(key: &str) -> Option<usize> {
    NUMBERS.iter().position(|s| s.key == key)
}

pub fn profile(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

/// The profile these settings equal, if any (unset values count as their defaults).
pub fn matching(leds: bool, values: &[Option<i16>; N]) -> Option<&'static Profile> {
    let resolved = |v: &[Option<i16>; N]| core::array::from_fn::<_, N, _>(|i| v[i].or(NUMBERS[i].default));
    let mine = resolved(values);
    PROFILES.iter().find(|p| p.leds == leds && resolved(&p.values) == mine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_valid_and_distinct() {
        for p in &PROFILES {
            for (v, s) in p.values.iter().zip(&NUMBERS) {
                if let Some(v) = v {
                    assert!(s.allowed.contains(*v), "{} {} = {}", p.name, s.key, v);
                }
            }
            assert_eq!(matching(p.leds, &p.values).map(|m| m.name), Some(p.name));
        }
        for s in &NUMBERS {
            assert!(s.default.is_none_or(|d| s.allowed.contains(d)), "{}", s.key);
        }
        // Nothing set is the default profile.
        assert_eq!(matching(true, &[None; N]).map(|p| p.name), Some("default"));
    }
}
