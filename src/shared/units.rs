//! Raw device units to the stock firmware's units. Shared verbatim with the host
//! (`emotibit::ble`), so a value converted on the board (SD card) and on the host (live
//! stream) is the same number. `core` + `libm` only.

/// Raw ADS1114 conversions summed per EDA record (must match `sensors::EDA_AVERAGED`).
pub const EDA_AVERAGED: f64 = 16.0;

/// EDL: the averaged raw ADS1114 count, as the stock firmware reports it on V4+ boards.
pub fn edl(sum: f64) -> f64 {
    sum / EDA_AVERAGED
}

/// EDA in micro-siemens from the summed counts (the stock transform for V4+ boards).
pub fn eda_us(sum: f64) -> f64 {
    let ohms = (edl(sum) * 728.8406523 + 14179797.05).max(100.0);
    1_000_000.0 / ohms
}

/// BMI160 at +-8 g: 4096 counts per g.
pub fn accel_g(raw: f64) -> f64 {
    raw * 8.0 / 32768.0
}

/// BMI160 at +-1000 deg/s.
pub fn gyro_dps(raw: f64) -> f64 {
    raw * 1000.0 / 32768.0
}

/// MAX30101 die temperature from milli-degrees.
pub fn temp1_c(milli: f64) -> f64 {
    milli / 1000.0
}

/// Battery voltage: twice the calibrated pin millivolts.
pub fn battery_volts_from_pin_mv(mv: f64) -> f64 {
    mv * 2.0 / 1000.0
}

/// The stock firmware's ESP32 battery curve (3.4 V empty, 4.05 V full).
pub fn battery_percent(volts: f64) -> f64 {
    libm::floor(((volts - 3.4) / (4.05 - 3.4) * 100.0).clamp(0.0, 100.0))
}

/// Si7013 temperature (after a humidity measurement), degrees C.
pub fn si7013_temp_c(code: f64) -> f64 {
    175.72 * code / 65536.0 - 46.85
}

/// Si7013 relative humidity, percent.
pub fn si7013_humidity(code: f64) -> f64 {
    (125.0 * code / 65536.0 - 6.0).clamp(0.0, 100.0)
}

/// BMM150 compensation (Bosch reference, as in the stock firmware), micro-tesla.
/// `t` is x1, y1, x2, y2, z1, z2, z3, z4, xy1, xy2, xyz1.
pub fn magnetometer(t: &[f64; 11], x: f64, y: f64, z: f64, rhall: f64) -> [f64; 3] {
    let [x1, y1, x2, y2, z1, z2, z3, z4, xy1, xy2, xyz1] = *t;
    let xy = |raw: f64, trim_a: f64, trim_b: f64| {
        if raw == -4096.0 || rhall == 0.0 || xyz1 == 0.0 {
            return 0.0;
        }
        let r = xyz1 * 16384.0 / rhall - 16384.0;
        let c1 = xy2 * (r * r / 268435456.0);
        let c2 = c1 + r * xy1 / 16384.0;
        let c4 = raw * ((c2 + 256.0) * (trim_a + 160.0));
        (c4 / 8192.0 + trim_b * 8.0) / 16.0
    };
    let zc = if z == -16384.0 || z2 == 0.0 || z1 == 0.0 || xyz1 == 0.0 || rhall == 0.0 {
        0.0
    } else {
        let c0 = z - z4;
        let c1 = rhall - xyz1;
        let c2 = z3 * c1;
        let c3 = z1 * rhall / 32768.0;
        let c4 = z2 + c3;
        let c5 = c0 * 131072.0 - c2;
        (c5 / (c4 * 4.0)) / 16.0
    };
    [xy(x, x2, x1), xy(y, y2, y1), zc]
}

/// MLX90632 object temperature (as the stock library computes it), degrees C.
/// `c` is P_R, P_G, P_T, P_O, Ea, Eb, Fa, Fb, Ga, Gb, Ka, Ha, Hb.
pub fn thermopile(c: &[f64; 13], six: f64, nine: f64, s_sum: f64) -> Option<f64> {
    let p = |e: i32| libm::pow(2.0, e as f64);
    let (ea, eb) = (c[4] * p(-16), c[5] * p(-8));
    // c[8] (Ga) multiplies `TO - 25` in the stock library, with its first-pass object
    // estimate TO = 25: that term is zero.
    let (fa, fb) = (c[6] * p(-46), c[7] * p(-36));
    let (gb, ka) = (c[9] * p(-10), c[10] * p(-10));
    let (ha, hb) = (c[11] * p(-14), c[12] * p(-14));
    let vrta = nine + gb * (six / 12.0);
    let amb = (six / 12.0) / vrta * p(19);
    let vrto = nine + ka * (six / 12.0);
    let sto = ((s_sum / 2.0) / 12.0) / vrto * p(19);
    let ta_dut = (amb - eb) / ea + 25.0;
    let ambient_k = ta_dut + 273.15;
    let big = sto / (fa * ha * (1.0 + fb * (ta_dut - 25.0)));
    let k4 = ambient_k * ambient_k * ambient_k * ambient_k;
    let obj = libm::pow(big + k4, 0.25) - 273.15 - hb;
    obj.is_finite().then_some(obj)
}
