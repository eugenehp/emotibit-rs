//! ESP32 ADC1 calibration at 11 dB attenuation, as esp-idf's `esp_adc_cal` (and therefore
//! Arduino's `analogReadMilliVolts`, which the stock firmware uses for the battery) does it.
//!
//! The factory burns either a two-point calibration (ADC readings at 150 mV and 850 mV) or the
//! chip's reference voltage into eFuse; without either, 1100 mV is assumed. Each gives a line
//! `mV = a * raw / 65536 + b`. esp-idf also applies a lookup table above a raw reading of 2880
//! (about 2.4 V at the pin); the battery divider stays well below that, so it is left out.
//!
//! Only `core` is used so the host can run the tests.

const LIN_SCALE: u32 = 65536;
/// esp-idf `adc1_tp_atten_scale[3]` / `adc1_tp_atten_offset[3]` (11 dB).
const TP_SCALE: u32 = 224_310;
const TP_OFFSET: u32 = 54;
/// esp-idf `adc1_vref_atten_scale[3]` / `adc1_vref_atten_offset[3]` (11 dB).
const VREF_SCALE: u32 = 197_611;
const VREF_OFFSET_MV: u32 = 142;
pub const DEFAULT_VREF_MV: u32 = 1100;

/// Which calibration a [`Line`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    TwoPoint,
    Vref,
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub a: u32,
    pub b: u32,
    pub source: Source,
}

impl Line {
    /// Builds the line from the raw eFuse fields: `tp_burned` (BLK3_PART_RESERVE),
    /// `tp_low` (ADC1_TP_LOW, 7 bits), `tp_high` (ADC1_TP_HIGH, 9 bits), `vref` (ADC_VREF, 5 bits).
    pub fn from_efuse(tp_burned: bool, tp_low: u32, tp_high: u32, vref: u32) -> Self {
        if tp_burned {
            // Readings at 150 mV and 850 mV, stored as offsets in two's complement, 4 counts per step.
            let low = (278 + 4 * twos_complement(tp_low, 7)) as u32;
            let high = (3265 + 4 * twos_complement(tp_high, 9)) as u32;
            let (dx, dv) = (high - low, 850 - 150);
            let a = (dv * TP_SCALE + dx / 2) / dx;
            let b = 850 - (dv * high + dx / 2) / dx + TP_OFFSET;
            return Self { a, b, source: Source::TwoPoint };
        }
        let (mv, source) = if vref != 0 {
            // Sign-magnitude, 7 mV per step around 1100 mV.
            let mag = (vref & 0x0F) as i32 * 7;
            ((DEFAULT_VREF_MV as i32 + if vref & 0x10 != 0 { -mag } else { mag }) as u32, Source::Vref)
        } else {
            (DEFAULT_VREF_MV, Source::Default)
        };
        Self { a: mv * VREF_SCALE / 4096, b: VREF_OFFSET_MV, source }
    }

    /// Millivolts at the pin for a 12-bit reading.
    pub fn millivolts(&self, raw: u16) -> u32 {
        (self.a * raw as u32 + LIN_SCALE / 2) / LIN_SCALE + self.b
    }
}

fn twos_complement(bits: u32, width: u32) -> i32 {
    let bits = bits & ((1 << width) - 1);
    if bits & (1 << (width - 1)) != 0 { bits as i32 - (1 << width) } else { bits as i32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_vref_matches_esp_idf() {
        let l = Line::from_efuse(false, 0, 0, 0);
        assert_eq!(l.source, Source::Default);
        // a = 1100 * 197611 / 4096 = 53069; 53069 * 2048 / 65536 + 142 = 1658 + 142.
        assert_eq!(l.a, 53069);
        assert_eq!(l.millivolts(2048), 1800);
        assert_eq!(l.millivolts(0), 142);
    }

    #[test]
    fn vref_is_sign_magnitude_in_7_mv_steps() {
        assert_eq!(Line::from_efuse(false, 0, 0, 0b00011).a, (1100 + 21) * 197_611 / 4096);
        assert_eq!(Line::from_efuse(false, 0, 0, 0b10011).a, (1100 - 21) * 197_611 / 4096);
    }

    #[test]
    fn two_point_takes_precedence_and_is_close_to_nominal() {
        let l = Line::from_efuse(true, 0, 0, 0b00011);
        assert_eq!(l.source, Source::TwoPoint);
        // Nominal points (278 @ 150 mV, 3265 @ 850 mV) give a line close to the default one.
        let d = Line::from_efuse(false, 0, 0, 0);
        assert!((l.millivolts(2000) as i32 - d.millivolts(2000) as i32).abs() < 40);
        // Negative offsets (two's complement) lower the readings, so the same raw means more mV.
        let shifted = Line::from_efuse(true, 0x7F, 0x1FF, 0);
        assert!(shifted.millivolts(2000) > l.millivolts(2000));
    }
}
