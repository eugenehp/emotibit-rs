//! EmotiBit sensor drivers. One task owns the I2C bus and polls every sensor; raw readings are
//! turned into 20-byte packets (see `emotibit::ble`) and queued for the BLE link and the SD
//! card. The same task drives the status LEDs and answers the self-test.
//!
//! The firmware streams *raw device units*; the host converts them, and the SD writer for CSV
//! files, with the same shared code (`units`, `stock`).

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer};
use embedded_hal_async::i2c::I2c;

pub type Packet = [u8; 20];
/// Wire format chosen by the host: 0 = raw 20-byte records, 1 = compact (see `codec`).
pub static MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
/// Incremented on every write of the mode characteristic: the encoder then restarts with key
/// frames, so a host that subscribes (or reconnects) after the write can decode from there.
pub static RESYNC: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
/// Bytes handed to the radio since the connection started.
pub static BYTES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Records dropped because the queue was full (the link could not keep up).
pub static DROPPED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Records for the BLE link, with the device milliseconds they were taken.
pub static QUEUE: Channel<CriticalSectionRawMutex, (u32, Packet), 96> = Channel::new();

/// Channel tags, shared with `emotibit::ble::tag`.
pub mod tag {
    pub const HEARTBEAT: u8 = 0;
    /// Sum of `EDA_AVERAGED` raw ADS1114 conversions (AIN0-AIN1, +-2.048 V, 250 SPS).
    pub const EDA: u8 = 1;
    /// Raw 18-bit MAX30101 counts: red, infrared, green.
    pub const PPG: u8 = 2;
    /// Raw BMI160 accelerometer counts x, y, z (+-8 g).
    pub const ACCEL: u8 = 3;
    /// Raw BMI160 gyroscope counts x, y, z (+-1000 dps).
    pub const GYRO: u8 = 4;
    /// Raw BMM150 magnetometer counts x, y, z, rhall.
    pub const MAG: u8 = 5;
    /// MLX90632 raw ambient / object words (see `Mlx`), sent as a pair.
    pub const THERMOPILE_RAW: u8 = 6;
    /// MAX30101 die temperature, milli-degrees C.
    pub const TEMP1: u8 = 7;
    /// Battery pin ADC: raw 12-bit counts, then the pin voltage in mV after the chip's eFuse
    /// calibration (see `adc_cal`). The pin sees half the battery voltage.
    pub const BATTERY: u8 = 8;
    /// MLX90632 EEPROM calibration words, sent once per connection start (index, value).
    pub const MLX_CAL: u8 = 9;
    /// BMM150 trim values, sent like `MLX_CAL`: (index, value).
    pub const MAG_CAL: u8 = 10;
    /// Link diagnostics, about once a second: dropped records, ATT MTU, records in the last batch.
    pub const LINK: u8 = 11;
    /// Si7013 raw codes: temperature, relative humidity (absent on V4+ boards).
    pub const SI7013: u8 = 12;
    /// Device milliseconds of the records that follow in the same notification.
    pub const CLOCK: u8 = 13;
    /// Board settings, every 10 s: PPG rate in Hz.
    pub const INFO: u8 = 14;
}

const NTAGS: usize = 16;

struct Seq([u16; NTAGS]);

impl Seq {
    fn send(&mut self, tag: u8, samples: &[i32]) {
        let n = samples.len().min(4);
        let mut p = [0u8; 20];
        p[0] = tag;
        p[1] = n as u8;
        // Numbers count streamed records only, so a host sees no gap across a stretch of low
        // power (no streaming); records the link could not take still use up a number.
        let streaming = crate::control::streaming();
        let s = &mut self.0[tag as usize];
        p[2..4].copy_from_slice(&s.to_le_bytes());
        if streaming {
            *s = s.wrapping_add(1);
        }
        for (i, v) in samples.iter().take(n).enumerate() {
            p[4 + 4 * i..8 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        use core::sync::atomic::Ordering::Relaxed;
        let now = crate::control::now_ms();
        if crate::control::RECORDING.load(Relaxed) && crate::control::SD_DATA.try_send((now, p)).is_err() {
            crate::control::SD_DROPPED.fetch_add(1, Relaxed);
            crate::control::overflowed(tag);
        }
        // Drop when nobody is draining (no central connected) or the link is behind; only the
        // latter is an overflow.
        if streaming && QUEUE.try_send((now, p)).is_err() {
            DROPPED.fetch_add(1, Relaxed);
            if crate::control::CONNECTED.load(Relaxed) {
                crate::control::overflowed(tag);
            }
        }
    }
}

// ── ADS1114 / ADS1115 (EDA) ───────────────────────────────────────────────────

const ADS: u8 = 0x48;
/// AIN0-AIN1, PGA +-2.048 V, continuous conversion, 250 SPS, comparator off.
const ADS_CONFIG: u16 = 0x8000 | 0x0400 | 0x00A0 | 0x0003;
/// EDA records carry the window's mean conversion times this; the host divides it out.
pub const EDA_AVERAGED: i32 = 16;
/// EDA record interval (16 conversions at 250 SPS).
const EDA_PERIOD: Duration = Duration::from_millis(64);

async fn ads_start<I: I2c>(i2c: &mut I) -> bool {
    let c = ADS_CONFIG.to_be_bytes();
    i2c.write(ADS, &[0x01, c[0], c[1]]).await.is_ok()
}

async fn ads_read<I: I2c>(i2c: &mut I) -> Option<i16> {
    let mut v = [0u8; 2];
    i2c.write_read(ADS, &[0x00], &mut v).await.ok()?;
    Some(i16::from_be_bytes(v))
}

// ── BMI160 (accelerometer + gyroscope) ────────────────────────────────────────

const BMI: u8 = 0x68;

async fn bmi_init<I: I2c>(i2c: &mut I) -> bool {
    let id = {
        let mut b = [0u8];
        if i2c.write_read(BMI, &[0x00], &mut b).await.is_err() {
            return false;
        }
        b[0]
    };
    if id != 0xD1 {
        return false;
    }
    let w = |r: u8, v: u8| [r, v];
    let _ = i2c.write(BMI, &w(0x7E, 0xB6)).await; // soft reset
    Timer::after(Duration::from_millis(100)).await;
    let _ = i2c.write(BMI, &w(0x7E, 0x11)).await; // accel normal mode
    Timer::after(Duration::from_millis(5)).await;
    let _ = i2c.write(BMI, &w(0x7E, 0x15)).await; // gyro normal mode
    Timer::after(Duration::from_millis(90)).await;
    let _ = i2c.write(BMI, &w(0x40, 0x26)).await; // ACC_CONF: 25 Hz, normal filter
    let _ = i2c.write(BMI, &w(0x41, 0x08)).await; // ACC_RANGE: +-8 g
    let _ = i2c.write(BMI, &w(0x42, 0x26)).await; // GYR_CONF: 25 Hz, normal filter
    let _ = i2c.write(BMI, &w(0x43, 0x01)).await; // GYR_RANGE: +-1000 dps
    true
}

// ── MAX30101 (PPG) ────────────────────────────────────────────────────────────

const PPG: u8 = 0x57;
/// Die-temperature interval: 7.5 Hz, the stock firmware's TEMP1 rate.
const TEMP1_PERIOD: Duration = Duration::from_millis(133);
/// Largest believable change between two die-temperature reads, milli-degrees.
const TEMP1_MAX_JUMP: i32 = 5000;

/// PPG at 25 Hz (stock: 400 sps averaged by 16, 215 us pulses) or 100 Hz (stock's PPG-100
/// variant: 800 sps averaged by 8, 118 us pulses).
async fn ppg_init<I: I2c>(i2c: &mut I, hz: u8) -> bool {
    let mut id = [0u8];
    if i2c.write_read(PPG, &[0xFF], &mut id).await.is_err() || id[0] != 0x15 {
        return false;
    }
    let w = |r: u8, v: u8| [r, v];
    let _ = i2c.write(PPG, &w(0x09, 0x40)).await; // reset
    Timer::after(Duration::from_millis(10)).await;
    let (fifo, spo2) = if hz == 100 { (0x7F, 0x31) } else { (0x9F, 0x2E) };
    let _ = i2c.write(PPG, &w(0x08, fifo)).await; // averaging, rollover, almost-full 15
    let _ = i2c.write(PPG, &w(0x0A, spo2)).await; // 4096 nA range, sample rate, pulse width
    let _ = i2c.write(PPG, &w(0x0C, 0x2F)).await; // red LED current
    let _ = i2c.write(PPG, &w(0x0D, 0x2F)).await; // IR LED current
    let _ = i2c.write(PPG, &w(0x0E, 0x2F)).await; // green LED current
    let _ = i2c.write(PPG, &w(0x11, 0x21)).await; // slot1 red, slot2 IR
    let _ = i2c.write(PPG, &w(0x12, 0x03)).await; // slot3 green
    let _ = i2c.write(PPG, &w(0x04, 0x00)).await; // clear FIFO pointers
    let _ = i2c.write(PPG, &w(0x05, 0x00)).await;
    let _ = i2c.write(PPG, &w(0x06, 0x00)).await;
    let _ = i2c.write(PPG, &w(0x03, 0x02)).await; // DIE_TEMP_RDY flag
    let _ = i2c.write(PPG, &w(0x09, 0x07)).await; // multi-LED mode
    true
}

/// Drains the FIFO, returning how many samples were forwarded.
async fn ppg_drain<I: I2c>(i2c: &mut I, seq: &mut Seq) -> usize {
    let mut ptr = [0u8; 3];
    if i2c.write_read(PPG, &[0x04], &mut ptr).await.is_err() {
        return 0;
    }
    let n = (ptr[0].wrapping_sub(ptr[2]) & 0x1F) as usize;
    for _ in 0..n {
        let mut b = [0u8; 9];
        if i2c.write_read(PPG, &[0x07], &mut b).await.is_err() {
            break;
        }
        let ch = |i: usize| (((b[i] as u32) << 16 | (b[i + 1] as u32) << 8 | b[i + 2] as u32) & 0x3FFFF) as i32;
        let v = [ch(0), ch(3), ch(6)];
        for (t, x) in ["PR", "PI", "PG"].into_iter().zip(v) {
            if x >= 0x3FFFF {
                crate::control::clipped(t);
            }
        }
        seq.send(tag::PPG, &v);
    }
    n
}

/// Starts a die-temperature conversion (ready about 30 ms later).
async fn ppg_temp_start<I: I2c>(i2c: &mut I) -> bool {
    i2c.write(PPG, &[0x21, 0x01]).await.is_ok()
}

/// Reads the die temperature in milli-degrees C (also the stock "bottom temperature").
/// `None` until the conversion has finished (DIE_TEMP_RDY, as the stock library checks):
/// some reads still return about 111 degrees (about 1 in 900), which are dropped.
async fn ppg_temp_read<I: I2c>(i2c: &mut I) -> Option<i32> {
    let mut st = [0u8];
    i2c.write_read(PPG, &[0x01], &mut st).await.ok()?;
    if st[0] & 0x02 == 0 {
        return None;
    }
    // Integer and fraction read separately, as the stock library does (reading the fraction
    // clears the flag).
    let (mut int, mut frac) = ([0u8], [0u8]);
    i2c.write_read(PPG, &[0x1F], &mut int).await.ok()?;
    i2c.write_read(PPG, &[0x20], &mut frac).await.ok()?;
    let int = int[0] as i8 as i32;
    // Outside the chip's rated -40..85 C is a bad read.
    if !(-40..=85).contains(&int) {
        log::warn!("TEMP1: dropped die temperature {}", int);
        return None;
    }
    Some(int * 1000 + (frac[0] as i32) * 62 + (frac[0] as i32) / 2)
}

// ── BMM150 magnetometer (behind the BMI160 auxiliary interface) ───────────────

/// BMM150 trim values in the order the host expects:
/// x1, y1, x2, y2, z1, z2, z3, z4, xy1, xy2, xyz1.
pub const MAG_TRIM_LEN: usize = 11;

async fn bmi_w<I: I2c>(i2c: &mut I, reg: u8, val: u8) {
    let _ = i2c.write(BMI, &[reg, val]).await;
}

/// Reads up to 8 BMM150 bytes starting at `reg` while the interface is in manual mode.
async fn bmm_read<I: I2c>(i2c: &mut I, reg: u8, out: &mut [u8]) -> bool {
    bmi_w(i2c, 0x4D, reg).await; // MAG_IF_2: read address
    Timer::after(Duration::from_millis(4)).await;
    let mut d = [0u8; 8];
    if i2c.write_read(BMI, &[0x04], &mut d).await.is_err() {
        return false;
    }
    let n = out.len().min(8);
    out[..n].copy_from_slice(&d[..n]);
    true
}

/// Brings up the BMM150 through the BMI160 and returns its trim values.
async fn mag_init<I: I2c>(i2c: &mut I) -> Option<[i32; MAG_TRIM_LEN]> {
    bmi_w(i2c, 0x7E, 0x19).await; // MAG interface normal power
    Timer::after(Duration::from_millis(60)).await;
    // Bosch's sequence for enabling the auxiliary pull-ups.
    bmi_w(i2c, 0x69, 0x40).await;
    for cmd in [0x37, 0x9A, 0xC0] {
        bmi_w(i2c, 0x7E, cmd).await;
    }
    bmi_w(i2c, 0x7F, 0x90).await;
    let mut r = [0u8];
    i2c.write_read(BMI, &[0x05], &mut r).await.ok()?;
    bmi_w(i2c, 0x05, (r[0] & !0x30) | 0x20).await;
    bmi_w(i2c, 0x7F, 0x80).await;
    bmi_w(i2c, 0x4B, 0x20).await; // MAG_IF_0: BMM150 address
    bmi_w(i2c, 0x4C, 0x83).await; // MAG_IF_1: manual mode, 8-byte burst
    i2c.write_read(BMI, &[0x6B], &mut r).await.ok()?;
    bmi_w(i2c, 0x6B, (r[0] & !0x30) | 0x20).await; // IF_CONF: enable the MAG interface
    // BMM150: leave suspend, set repetitions.
    for (data, reg) in [(0x01u8, 0x4Bu8), (4, 0x51), (14, 0x52)] {
        bmi_w(i2c, 0x4F, data).await; // MAG_IF_4: value
        bmi_w(i2c, 0x4E, reg).await; // MAG_IF_3: BMM150 register to write
        Timer::after(Duration::from_millis(4)).await;
    }

    let mut a = [0u8; 2];
    let mut b = [0u8; 4];
    let mut c = [0u8; 8];
    let mut d = [0u8; 2];
    if !(bmm_read(i2c, 0x5D, &mut a).await
        && bmm_read(i2c, 0x62, &mut b).await
        && bmm_read(i2c, 0x68, &mut c).await
        && bmm_read(i2c, 0x70, &mut d).await)
    {
        return None;
    }
    let trim = [
        a[0] as i8 as i32,                               // x1
        a[1] as i8 as i32,                               // y1
        b[2] as i8 as i32,                               // x2
        b[3] as i8 as i32,                               // y2
        u16::from_le_bytes([c[2], c[3]]) as i32,         // z1
        i16::from_le_bytes([c[0], c[1]]) as i32,         // z2
        i16::from_le_bytes([c[6], c[7]]) as i32,         // z3
        i16::from_le_bytes([b[0], b[1]]) as i32,         // z4
        d[1] as i32,                                     // xy1
        d[0] as i8 as i32,                               // xy2
        (u16::from_le_bytes([c[4], c[5] & 0x7F])) as i32, // xyz1
    ];

    // Data mode: the BMI160 triggers a forced BMM150 measurement and fetches the result itself.
    bmi_w(i2c, 0x4F, 0x02).await; // MAG_IF_4: BMM150 op-mode value (forced)
    bmi_w(i2c, 0x4E, 0x4C).await; // MAG_IF_3: op-mode register
    bmi_w(i2c, 0x4D, 0x42).await; // MAG_IF_2: first data register
    bmi_w(i2c, 0x44, 0x06).await; // MAG_CONF: 25 Hz
    bmi_w(i2c, 0x4C, 0x03).await; // MAG_IF_1: manual mode off, 8-byte burst
    Timer::after(Duration::from_millis(10)).await;
    Some(trim)
}

// ── MLX90632 (thermopile) ─────────────────────────────────────────────────────

const MLX: u8 = 0x3A;
/// Calibration words in the order the host expects:
/// P_R, P_G, P_T, P_O, Ea, Eb, Fa, Fb, Ga (32-bit) then Gb, Ka, Ha, Hb (16-bit).
pub const MLX_CAL_LEN: usize = 13;

async fn mlx_r16<I: I2c>(i2c: &mut I, addr: u16) -> Option<u16> {
    let mut v = [0u8; 2];
    i2c.write_read(MLX, &addr.to_be_bytes(), &mut v).await.ok()?;
    Some(u16::from_be_bytes(v))
}

async fn mlx_r32<I: I2c>(i2c: &mut I, addr: u16) -> Option<i32> {
    let mut v = [0u8; 4];
    i2c.write_read(MLX, &addr.to_be_bytes(), &mut v).await.ok()?;
    // The first 16-bit word is the low half.
    let lo = u16::from_be_bytes([v[0], v[1]]) as u32;
    let hi = u16::from_be_bytes([v[2], v[3]]) as u32;
    Some((hi << 16 | lo) as i32)
}

async fn mlx_w16<I: I2c>(i2c: &mut I, addr: u16, val: u16) -> bool {
    let a = addr.to_be_bytes();
    let v = val.to_be_bytes();
    i2c.write(MLX, &[a[0], a[1], v[0], v[1]]).await.is_ok()
}

async fn mlx_set_mode<I: I2c>(i2c: &mut I, mode: u16) {
    if let Some(c) = mlx_r16(i2c, 0x3001).await {
        mlx_w16(i2c, 0x3001, (c & !(3 << 1)) | (mode << 1)).await;
    }
}

async fn mlx_init<I: I2c>(i2c: &mut I) -> Option<[i32; MLX_CAL_LEN]> {
    mlx_set_mode(i2c, 0b01).await; // sleep while reading EEPROM
    Timer::after(Duration::from_millis(5)).await;
    let mut cal = [0i32; MLX_CAL_LEN];
    for (i, addr) in [0x240Cu16, 0x240E, 0x2410, 0x2412, 0x2424, 0x2426, 0x2428, 0x242A, 0x242C].into_iter().enumerate() {
        cal[i] = mlx_r32(i2c, addr).await?;
    }
    for (i, addr) in [0x242Eu16, 0x242F, 0x2481, 0x2482].into_iter().enumerate() {
        cal[9 + i] = mlx_r16(i2c, addr).await? as i16 as i32;
    }
    mlx_set_mode(i2c, 0b10).await; // step mode
    Some(cal)
}

/// Starts one measurement (step mode) and clears the new-data flag.
async fn mlx_start<I: I2c>(i2c: &mut I) {
    if let Some(c) = mlx_r16(i2c, 0x3001).await {
        mlx_w16(i2c, 0x3001, c | (1 << 3)).await;
    }
    if let Some(st) = mlx_r16(i2c, 0x3FFF).await {
        mlx_w16(i2c, 0x3FFF, st & !1).await;
    }
}

/// If a measurement finished returns (RAM_6, RAM_9, RAM_lower + RAM_upper).
async fn mlx_poll<I: I2c>(i2c: &mut I) -> Option<[i32; 3]> {
    let st = mlx_r16(i2c, 0x3FFF).await?;
    if st & 1 == 0 {
        return None;
    }
    let six = mlx_r16(i2c, 0x4005).await? as i16 as i32;
    let nine = mlx_r16(i2c, 0x4008).await? as i16 as i32;
    let (lo, up) = if (st >> 2) & 0x1F == 2 { (0x4006, 0x4007) } else { (0x4003, 0x4004) };
    let l = mlx_r16(i2c, lo).await? as i16 as i32;
    let u = mlx_r16(i2c, up).await? as i16 as i32;
    Some([six, nine, l + u])
}

/// Measurements in a row with bit-identical raw words after which the thermopile counts as
/// stuck. Real readings always carry a few counts of noise.
const MLX_STUCK_AFTER: u8 = 8;

/// Logs the registers of a stuck MLX90632 and brings it back to step mode.
async fn mlx_recover<I: I2c>(i2c: &mut I) {
    let st = mlx_r16(i2c, 0x3FFF).await.unwrap_or(0);
    let ctrl = mlx_r16(i2c, 0x3001).await.unwrap_or(0);
    log::warn!("thermopile stuck (status {:#06x}, control {:#06x}); restarting it", st, ctrl);
    mlx_set_mode(i2c, 0b01).await;
    Timer::after(Duration::from_millis(5)).await;
    mlx_set_mode(i2c, 0b10).await;
    mlx_start(i2c).await;
}

// ── Si7013 (temperature + humidity; fitted on V2/V3 boards only) ──────────────

const SI: u8 = 0x40;
/// Measurement interval.
const SI_PERIOD: Duration = Duration::from_millis(500);

/// True if an Si7013/20/21 answers: the electronic ID's first byte names the part.
async fn si_init<I: I2c>(i2c: &mut I) -> bool {
    let mut id = [0u8; 6];
    i2c.write_read(SI, &[0xFC, 0xC9], &mut id).await.is_ok() && matches!(id[0], 0x0D | 0x14 | 0x15)
}

/// Starts a humidity conversion (no clock stretching); the chip measures temperature with it.
async fn si_start<I: I2c>(i2c: &mut I) -> bool {
    i2c.write(SI, &[0xF5]).await.is_ok()
}

/// Raw (temperature, humidity) codes once the conversion is done (the chip NACKs until then).
async fn si_read<I: I2c>(i2c: &mut I) -> Option<(i32, i32)> {
    let mut h = [0u8; 2];
    i2c.read(SI, &mut h).await.ok()?;
    let mut t = [0u8; 2];
    i2c.write_read(SI, &[0xE0], &mut t).await.ok()?;
    Some((u16::from_be_bytes(t) as i32, u16::from_be_bytes(h) as i32))
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

/// Counts IMU samples at the ends of the range as clipped.
fn clip_check(tags: &[&str; 3], v: &[i32; 3]) {
    for (t, &x) in tags.iter().zip(v) {
        if x == i16::MAX as i32 || x == i16::MIN as i32 {
            crate::control::clipped(t);
        }
    }
}

/// Answers `ST`: one `ST` line of `item,PASS|FAIL` pairs for each chip, the SD card, the
/// EEPROM identity and the battery.
async fn self_test<I: I2c>(i2c: &mut I, found: &Found) {
    use crate::control::{self, found as bit};
    use core::sync::atomic::Ordering::Relaxed;
    let bits = control::FOUND.load(Relaxed);
    // Chips are probed again, so a sensor lost since boot shows up.
    let mut b = [0u8];
    let eda = found.eda && i2c.write_read(ADS, &[0x01], &mut [0u8; 2]).await.is_ok();
    let imu = found.imu && i2c.write_read(BMI, &[0x00], &mut b).await.is_ok() && b[0] == 0xD1;
    let ppg = found.ppg && i2c.write_read(PPG, &[0xFF], &mut b).await.is_ok() && b[0] == 0x15;
    let mlx = found.thermopile && mlx_r16(i2c, 0x3001).await.is_some();
    let pct = control::BATTERY_PERCENT.load(Relaxed);
    let pass = |ok: bool| if ok { "PASS" } else { "FAIL" };
    let items = [
        ("EDA", pass(eda)),
        ("IMU", pass(imu)),
        ("PPG", pass(ppg)),
        ("MAG", pass(bits & bit::MAG != 0)),
        ("THERMOPILE", pass(mlx)),
        ("TEMP_HUMIDITY", if bits & bit::SI7013 != 0 { "PASS" } else { "ABSENT" }),
        ("EEPROM_ID", pass(bits & bit::EEPROM != 0)),
        ("LEDS", if !control::board().leds { "OFF" } else { pass(bits & bit::LEDS != 0) }),
        ("SD_CARD", pass(control::SD_PRESENT.load(Relaxed))),
        ("BATTERY", pass(pct != 255)),
    ];
    let mut fields: [&str; 20] = [""; 20];
    for (i, (k, v)) in items.iter().enumerate() {
        fields[2 * i] = k;
        fields[2 * i + 1] = v;
    }
    log::info!("self-test: {:?}", &fields[..]);
    control::emit("ST", &fields, true);
}

/// Which chips answered at boot; logged by the caller.
#[derive(Default, Clone, Copy)]
pub struct Found {
    pub eda: bool,
    pub imu: bool,
    pub ppg: bool,
    pub mag: bool,
    pub thermopile: bool,
    pub si7013: bool,
}

/// Runs the sensors forever. `board` carries the settings (PPG rate, LEDs); `hw_version` the
/// stock hardware-version number from the EEPROM, which picks the LED driver's address.
pub async fn run<I: I2c, B: FnMut() -> Option<(u16, u32)>>(mut i2c: I, mut read_battery: B, board: crate::control::Board, hw_version: Option<u8>) -> ! {
    use crate::control::{self, found as bit};
    use core::sync::atomic::Ordering::Relaxed;
    let mut seq = Seq([0; NTAGS]);
    let eda = ads_start(&mut i2c).await;
    let imu = bmi_init(&mut i2c).await;
    let mag_trim = if imu { mag_init(&mut i2c).await } else { None };
    let mlx_cal = mlx_init(&mut i2c).await;
    let found = Found {
        eda,
        imu,
        ppg: ppg_init(&mut i2c, board.ppg_hz).await,
        mag: mag_trim.is_some(),
        thermopile: mlx_cal.is_some(),
        si7013: si_init(&mut i2c).await,
    };
    // Set up even when the settings turn LEDs off: they can be turned on over Bluetooth.
    let mut leds = crate::board::LedDriver::new(&mut i2c, hw_version).await;
    log::info!(
        "sensors: eda={} imu={} ppg={} ({} Hz) mag={} thermopile={} si7013={} leds={}",
        found.eda, found.imu, found.ppg, board.ppg_hz, found.mag, found.thermopile, found.si7013, leds.is_some()
    );
    let bits = [
        (found.eda, bit::EDA),
        (found.imu, bit::IMU),
        (found.ppg, bit::PPG),
        (found.mag, bit::MAG),
        (found.thermopile, bit::THERMOPILE),
        (found.si7013, bit::SI7013),
        (leds.is_some(), bit::LEDS),
    ];
    control::FOUND.fetch_or(bits.iter().filter(|(on, _)| *on).fold(0, |a, (_, b)| a | b), Relaxed);
    let mut led_due = Instant::now();
    let mut red_toggled = Instant::now();
    let mut red = false;
    let mut low_battery = false;
    let mut report_due = Instant::now() + Duration::from_secs(1);
    let mut eda_clipped = false;
    if found.thermopile {
        mlx_start(&mut i2c).await;
    }
    let mut cal_due = Instant::now();
    let (mut mlx_last, mut mlx_same) = ([0i32; 3], 0u8);

    let mut eda_sum = 0i32;
    let mut eda_n = 0;
    let mut temp_due = Instant::now() + Duration::from_secs(1);
    let mut temp_read_at: Option<Instant> = None;
    let (mut temp_last, mut temp_rejected): (Option<i32>, u8) = (None, 0);
    let mut next_batt = Instant::now();
    let mut si_due = Instant::now();
    let mut si_read_at: Option<Instant> = None;
    let mut tick = 0u32;
    let mut ticker = Ticker::every(Duration::from_millis(2));

    let mut eda_due = Instant::now() + EDA_PERIOD;
    loop {
        tick = tick.wrapping_add(1);
        // EDA: continuous 250 SPS, read every 4 ms and averaged over 64 ms windows -> 15.625 Hz.
        // The window is timed rather than counted: while the radio is busy the loop runs
        // slower and fewer conversions land in a window, but the rate stays the same.
        if found.eda && tick.is_multiple_of(2)
            && let Some(v) = ads_read(&mut i2c).await {
                eda_sum += v as i32;
                eda_n += 1;
                eda_clipped |= v == i16::MAX || v == i16::MIN;
            }
        if found.eda && Instant::now() >= eda_due {
            eda_due += EDA_PERIOD;
            if eda_n > 0 {
                if eda_clipped {
                    control::clipped("EA");
                    control::clipped("EL");
                }
                // Scaled to a sum of EDA_AVERAGED conversions, which the host divides out.
                seq.send(tag::EDA, &[eda_sum * EDA_AVERAGED / eda_n]);
            }
            eda_clipped = false;
            eda_sum = 0;
            eda_n = 0;
        }
        // IMU: the chip's data-ready flags avoid duplicates at 25 Hz.
        if found.imu && tick % 5 == 1 {
            let mut st = [0u8];
            if i2c.write_read(BMI, &[0x1B], &mut st).await.is_ok() {
                // Read each stream on its own data-ready flag so none is skipped or repeated.
                if st[0] & 0x40 != 0 {
                    let mut d = [0u8; 6];
                    if i2c.write_read(BMI, &[0x0C], &mut d).await.is_ok() {
                        let v = |i: usize| i16::from_le_bytes([d[2 * i], d[2 * i + 1]]) as i32;
                        clip_check(&["GX", "GY", "GZ"], &[v(0), v(1), v(2)]);
                        seq.send(tag::GYRO, &[v(0), v(1), v(2)]);
                    }
                }
                if st[0] & 0x80 != 0 {
                    let mut d = [0u8; 6];
                    if i2c.write_read(BMI, &[0x12], &mut d).await.is_ok() {
                        let v = |i: usize| i16::from_le_bytes([d[2 * i], d[2 * i + 1]]) as i32;
                        clip_check(&["AX", "AY", "AZ"], &[v(0), v(1), v(2)]);
                        seq.send(tag::ACCEL, &[v(0), v(1), v(2)]);
                    }
                }
                if found.mag && st[0] & 0x20 != 0 {
                    let mut d = [0u8; 8];
                    if i2c.write_read(BMI, &[0x04], &mut d).await.is_ok() {
                        let x = i16::from_le_bytes([d[0], d[1]]) >> 3;
                        let y = i16::from_le_bytes([d[2], d[3]]) >> 3;
                        let z = i16::from_le_bytes([d[4], d[5]]) >> 1;
                        let r = u16::from_le_bytes([d[6], d[7]]) >> 2;
                        // The BMM150's overflow codes.
                        for (t, over) in [("MX", x == -4096), ("MY", y == -4096), ("MZ", z == -16384)] {
                            if over {
                                control::clipped(t);
                            }
                        }
                        seq.send(tag::MAG, &[x as i32, y as i32, z as i32, r as i32]);
                    }
                }
            }
        }
        if found.ppg && tick % 5 == 3 {
            ppg_drain(&mut i2c, &mut seq).await;
        }
        if found.ppg {
            let now = Instant::now();
            if temp_read_at.is_none() && now >= temp_due {
                if ppg_temp_start(&mut i2c).await {
                    temp_read_at = Some(now + Duration::from_millis(40));
                }
                temp_due = now + TEMP1_PERIOD;
            } else if temp_read_at.is_some_and(|t| now >= t) {
                temp_read_at = None;
                if let Some(t) = ppg_temp_read(&mut i2c).await {
                    // Die temperature moves slowly; a read far from the last good one is one of
                    // the chip's bad reads (some land inside the rated range). Three in a row
                    // are a real change.
                    let jump = temp_last.is_some_and(|l: i32| (t - l).abs() > TEMP1_MAX_JUMP);
                    if jump && temp_rejected < 2 {
                        temp_rejected += 1;
                        log::warn!("TEMP1: dropped die temperature {} (last {})", t / 1000, temp_last.unwrap_or(0) / 1000);
                    } else {
                        temp_rejected = 0;
                        temp_last = Some(t);
                        seq.send(tag::TEMP1, &[t]);
                    }
                } else if now < temp_due {
                    temp_read_at = Some(now + Duration::from_millis(5));
                }
            }
        }
        if found.thermopile && tick % 5 == 4
            && let Some(raw) = mlx_poll(&mut i2c).await {
                seq.send(tag::THERMOPILE_RAW, &raw);
                mlx_same = if raw == mlx_last { mlx_same.saturating_add(1) } else { 0 };
                mlx_last = raw;
                if mlx_same >= MLX_STUCK_AFTER {
                    mlx_same = 0;
                    mlx_recover(&mut i2c).await;
                } else {
                    mlx_start(&mut i2c).await;
                }
            }
        if found.si7013 {
            let now = Instant::now();
            if si_read_at.is_none() && now >= si_due {
                si_due = now + SI_PERIOD;
                if si_start(&mut i2c).await {
                    si_read_at = Some(now + Duration::from_millis(25));
                }
            } else if si_read_at.is_some_and(|t| now >= t) {
                si_read_at = None;
                match si_read(&mut i2c).await {
                    Some((t, h)) => seq.send(tag::SI7013, &[t, h]),
                    None if now < si_due => si_read_at = Some(now + Duration::from_millis(5)),
                    None => {}
                }
            }
        }
        // Calibration is repeated so a central that connects later still receives it, and
        // sent again when a recording starts so the file has it at the top.
        if crate::control::RESEND_CAL.swap(false, core::sync::atomic::Ordering::Relaxed) {
            cal_due = Instant::now();
        }
        if Instant::now() >= cal_due {
            cal_due = Instant::now() + Duration::from_secs(10);
            if let Some(cal) = &mlx_cal {
                for (i, v) in cal.iter().enumerate() {
                    seq.send(tag::MLX_CAL, &[i as i32, *v]);
                }
            }
            if let Some(trim) = &mag_trim {
                for (i, v) in trim.iter().enumerate() {
                    seq.send(tag::MAG_CAL, &[i as i32, *v]);
                }
            }
            seq.send(tag::INFO, &[control::board().ppg_hz as i32]);
        }
        if Instant::now() >= next_batt {
            next_batt = Instant::now() + Duration::from_secs(5);
            if let Some((raw, mv)) = read_battery() {
                seq.send(tag::BATTERY, &[raw as i32, mv as i32]);
                let pct = crate::units::battery_percent(crate::units::battery_volts_from_pin_mv(mv as f64));
                control::BATTERY_PERCENT.store(pct as u8, Relaxed);
            }
        }
        // Status LEDs, as the stock firmware drives them: all on while the button is held; blue
        // on while connected, blinking while advertising; yellow below 20% battery until 50%;
        // red blinking while recording.
        if let Some(d) = leds.as_mut() {
            let now = Instant::now();
            if now >= led_due {
                led_due = now + Duration::from_millis(50);
                let pct = control::BATTERY_PERCENT.load(Relaxed);
                if pct != 255 {
                    low_battery = if low_battery { pct <= 50 } else { pct < 20 };
                }
                if control::RECORDING.load(Relaxed) {
                    if red_toggled.elapsed() >= Duration::from_millis(500) {
                        red = !red;
                        red_toggled = now;
                    }
                } else {
                    red = false;
                }
                let radio_on = !matches!(control::power(), control::Power::WirelessOff | control::Power::Hibernate);
                let blink = now.as_millis() % 500 < 125;
                let state = if !control::board().leds {
                    crate::board::Leds::default()
                } else if control::BUTTON_HELD.load(Relaxed) {
                    crate::board::Leds { red: true, blue: true, yellow: true }
                } else {
                    crate::board::Leds {
                        red,
                        blue: control::CONNECTED.load(Relaxed) || (radio_on && blink),
                        yellow: low_battery,
                    }
                };
                d.set(&mut i2c, state).await;
                if control::power() == control::Power::Hibernate {
                    d.set(&mut i2c, crate::board::Leds::default()).await;
                    leds = None;
                }
            }
        }
        if Instant::now() >= report_due {
            report_due += Duration::from_secs(1);
            control::report_clipping_and_overflow();
        }
        // PPG rate changed over Bluetooth: set the chip up again, and tell the host at once.
        if control::PPG_CHANGED.swap(false, Relaxed) && found.ppg {
            let hz = control::board().ppg_hz;
            ppg_init(&mut i2c, hz).await;
            log::info!("PPG now {} Hz", hz);
            cal_due = Instant::now();
        }
        if control::SELF_TEST.swap(false, Relaxed) {
            self_test(&mut i2c, &found).await;
        }
        ticker.next().await;
    }
}
