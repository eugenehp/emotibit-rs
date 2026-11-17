//! Firmware records to stock EmotiBit tags and units, and the stock packet line format.
//! Shared verbatim with the host (`emotibit::ble`): the board uses it to write stock-format
//! SD files, the host to read binary SD files and to convert the live stream.

use super::units;

/// Record tags (the first byte of a record). Must match `sensors::tag` and `emotibit::ble::id`.
pub mod id {
    pub const HEARTBEAT: u8 = 0;
    pub const EDA: u8 = 1;
    pub const PPG: u8 = 2;
    pub const ACCEL: u8 = 3;
    pub const GYRO: u8 = 4;
    pub const MAG: u8 = 5;
    pub const THERMOPILE_RAW: u8 = 6;
    pub const TEMP1: u8 = 7;
    pub const BATTERY: u8 = 8;
    pub const MLX_CAL: u8 = 9;
    pub const MAG_CAL: u8 = 10;
    pub const LINK: u8 = 11;
    /// Si7013 raw temperature and humidity codes.
    pub const SI7013: u8 = 12;
    /// Device milliseconds of the records that follow in the same notification.
    pub const CLOCK: u8 = 13;
    /// Board settings, every 10 s: PPG rate in Hz.
    pub const INFO: u8 = 14;
}

/// The stock tags a record produces (for overflow counting).
pub fn tags_of(record_tag: u8) -> &'static [&'static str] {
    match record_tag {
        id::EDA => &["EA", "EL"],
        id::PPG => &["PR", "PI", "PG"],
        id::ACCEL => &["AX", "AY", "AZ"],
        id::GYRO => &["GX", "GY", "GZ"],
        id::MAG => &["MX", "MY", "MZ"],
        id::THERMOPILE_RAW => &["TH"],
        id::TEMP1 => &["T1"],
        _ => &[],
    }
}

/// Turns records into `(stock tag, value)` pairs. Keeps the calibration words the firmware
/// repeats every 10 s; magnetometer and thermopile values appear once they have arrived.
#[derive(Debug, Clone)]
pub struct Records {
    mlx: [Option<i32>; 13],
    mag: [Option<i32>; 11],
}

impl Default for Records {
    fn default() -> Self {
        Self::new()
    }
}

impl Records {
    pub const fn new() -> Self {
        Self { mlx: [None; 13], mag: [None; 11] }
    }

    /// Calls `out(tag, value)` for every stock sample in the record. Calibration and link
    /// records produce nothing.
    pub fn convert(&mut self, tag: u8, s: &[i32], mut out: impl FnMut(&'static str, f64)) {
        let f = |i: usize| s.get(i).copied().unwrap_or(0) as f64;
        match tag {
            id::EDA => {
                out("EA", units::eda_us(f(0)));
                out("EL", units::edl(f(0)));
            }
            id::PPG => {
                out("PR", f(0));
                out("PI", f(1));
                out("PG", f(2));
            }
            id::ACCEL => {
                out("AX", units::accel_g(f(0)));
                out("AY", units::accel_g(f(1)));
                out("AZ", units::accel_g(f(2)));
            }
            id::GYRO => {
                out("GX", units::gyro_dps(f(0)));
                out("GY", units::gyro_dps(f(1)));
                out("GZ", units::gyro_dps(f(2)));
            }
            id::MAG => {
                if let Some(t) = all(&self.mag) {
                    let [x, y, z] = units::magnetometer(&t, f(0), f(1), f(2), f(3));
                    out("MX", x);
                    out("MY", y);
                    out("MZ", z);
                }
            }
            id::THERMOPILE_RAW => {
                if let Some(v) = all(&self.mlx).and_then(|c| units::thermopile(&c, f(0), f(1), f(2))) {
                    out("TH", v);
                }
            }
            id::TEMP1 => out("T1", units::temp1_c(f(0))),
            id::BATTERY => {
                // Older firmware sent only the raw count; see `emotibit::ble::battery_volts`.
                if s.len() >= 2 {
                    let v = units::battery_volts_from_pin_mv(f(1));
                    out("BV", v);
                    out("B%", units::battery_percent(v));
                }
            }
            id::SI7013 => {
                out("T0", units::si7013_temp_c(f(0)));
                out("H0", units::si7013_humidity(f(1)));
            }
            id::MLX_CAL => store(&mut self.mlx, s),
            id::MAG_CAL => store(&mut self.mag, s),
            _ => {}
        }
    }
}

fn store<const N: usize>(slots: &mut [Option<i32>; N], s: &[i32]) {
    if let (Some(&i), Some(&v)) = (s.first(), s.get(1))
        && let Some(slot) = slots.get_mut(i as usize) {
            *slot = Some(v);
        }
}

fn all<const N: usize>(slots: &[Option<i32>; N]) -> Option<[f64; N]> {
    let mut out = [0.0; N];
    for (o, v) in out.iter_mut().zip(slots) {
        *o = (*v)? as f64;
    }
    Some(out)
}

/// Writes one stock packet line, newline included:
/// `timestamp,number,count,TAG,1,100,payload...`.
pub fn write_line<W: core::fmt::Write>(w: &mut W, timestamp: u32, number: u16, tag: &str, payload: &[&str]) -> core::fmt::Result {
    write!(w, "{timestamp},{number},{},{tag},1,100", payload.len())?;
    for p in payload {
        write!(w, ",{p}")?;
    }
    w.write_char('\n')
}

/// Writes one stock data line with a single value. Integers print without decimals, the rest
/// with up to six (trailing zeros dropped), close to what the stock firmware writes.
pub fn write_value<W: core::fmt::Write>(w: &mut W, timestamp: u32, number: u16, tag: &str, v: f64) -> core::fmt::Result {
    write!(w, "{timestamp},{number},1,{tag},1,100,")?;
    if v == libm::trunc(v) && libm::fabs(v) < 1e9 {
        write!(w, "{}", v as i64)?;
    } else {
        use core::fmt::Write as _;
        // Six decimals, then drop trailing zeros.
        let mut buf = Fixed::<32>::new();
        let _ = write!(buf, "{v:.6}");
        let s = buf.as_str().trim_end_matches('0').trim_end_matches('.');
        w.write_str(s)?;
    }
    w.write_char('\n')
}

/// A stock packet line, split without allocating.
pub struct Line<'a> {
    pub timestamp: u32,
    pub number: u16,
    pub tag: &'a str,
    /// Payload elements after the six header fields, comma-separated.
    pub payload: &'a str,
}

impl<'a> Line<'a> {
    pub fn parse(line: &'a str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        let mut it = line.splitn(7, ',');
        let timestamp = it.next()?.trim().parse().ok()?;
        let number = it.next()?.trim().parse().ok()?;
        let _count = it.next()?;
        let tag = it.next()?;
        let _version = it.next()?;
        let _reliability = it.next()?;
        Some(Self { timestamp, number, tag, payload: it.next().unwrap_or("") })
    }

    /// The payload elements.
    pub fn fields(&self) -> impl Iterator<Item = &'a str> {
        self.payload.split(',').filter(|s| !s.is_empty())
    }
}

/// A small fixed-capacity string for formatting without allocation.
#[derive(Clone, Copy)]
pub struct Fixed<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Default for Fixed<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Fixed<N> {
    pub const fn new() -> Self {
        Self { buf: [0; N], len: 0 }
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl<const N: usize> core::fmt::Write for Fixed<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        if self.len + b.len() > N {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
        Ok(())
    }
}

/// What goes into a recording's `_info.json`.
pub struct InfoJson<'a> {
    pub device_id: &'a str,
    pub hardware: &'a str,
    pub sku: &'a str,
    pub firmware_version: &'a str,
    /// The host's local time at the start (`2026-11-07_20-23-15-026091`).
    pub created_at: &'a str,
    pub ppg_hz: f64,
    /// An Si7013 is fitted (TEMP0 and humidity).
    pub temp_humidity: bool,
    /// What the recording holds so far; `None` leaves the `session` member out.
    pub session: Option<SessionInfo<'a>>,
}

/// The `session` member of an `_info.json` (emotibit-ble's addition to the stock format): the
/// data file, its format, and, once the recording has ended, how long it ran and what it holds.
pub struct SessionInfo<'a> {
    /// The data file's name (`2026-11-07_20-23-15-026091.csv`).
    pub data_file: &'a str,
    /// `csv` or `binary`.
    pub format: &'a str,
    /// `recording` while the file is open, `done` after.
    pub state: &'a str,
    pub duration_s: f64,
    pub bytes: u64,
    /// `UN` notes (the start note not counted) and `LM` LSL markers in the file.
    pub notes: u32,
    pub markers: u32,
}

/// Writes the stock firmware's `_info.json`: a JSON array of `{"info": {...}}` objects, the
/// device first, then one per signal group with its tags, rate and units.
pub fn write_info_json<W: core::fmt::Write>(w: &mut W, i: &InfoJson) -> core::fmt::Result {
    write!(
        w,
        r#"[{{"info":{{"name":"EmotiBitData","type":"Multimodal","source_id":"EmotiBit FeatherWing","hardware_version":"{}","sku":"{}","device_id":"{}","feather_version":"Adafruit Feather HUZZAH32","firmware_version":"{}","firmware_variant":"emotibit-ble","created_at":"{}""#,
        i.hardware, i.sku, i.device_id, i.firmware_version, i.created_at
    )?;
    if let Some(s) = &i.session {
        write!(
            w,
            r#","session":{{"data_file":"{}","format":"{}","state":"{}","duration_s":{:.1},"bytes":{},"notes":{},"markers":{}}}"#,
            s.data_file, s.format, s.state, s.duration_s, s.bytes, s.notes, s.markers
        )?;
    }
    w.write_str("}}")?;
    let groups: [(&str, &str, &[&str], f64, &str); 12] = [
        ("Accelerometer", "Accelerometer", &["AX", "AY", "AZ"], 25.0, "g"),
        ("Gyroscope", "Gyroscope", &["GX", "GY", "GZ"], 25.0, "degrees/second"),
        ("Magnetometer", "Magnetometer", &["MX", "MY", "MZ"], 25.0, "microteslas"),
        ("ElectrodermalActivity", "EDA", &["EA"], 15.625, "microsiemens"),
        ("ElectrodermalLevel", "EDA", &["EL"], 15.625, "raw units"),
        ("Humidity0", "Humidity", &["H0"], 2.0, "Percent"),
        ("Temperature0", "Temperature", &["T0"], 2.0, "degrees celcius"),
        ("Temperature1", "Temperature", &["T1"], 7.5, "degrees celcius"),
        ("Thermopile", "Temperature", &["TH"], 7.5, "degrees celcius"),
        ("PPG", "PPG", &["PI", "PR", "PG"], i.ppg_hz, "raw units"),
        ("HeartRate", "PPG", &["HR"], 0.0, "bpm"),
        ("InterBeatInterval", "PPG", &["BI"], 0.0, "mS"),
    ];
    for (name, ty, tags, rate, units) in groups {
        if !i.temp_humidity && (name == "Humidity0" || name == "Temperature0") {
            continue;
        }
        write!(w, r#",{{"info":{{"name":"{name}","type":"{ty}","typeTags":["#)?;
        for (k, t) in tags.iter().enumerate() {
            write!(w, r#"{}"{t}""#, if k > 0 { "," } else { "" })?;
        }
        write!(w, r#"],"channel_count":{},"nominal_srate":{rate},"channel_format":"float","units":"{units}"}}}}"#, tags.len())?;
    }
    w.write_str(r#",{"info":{"name":"SkinConductanceResponse","type":"EDA","typeTags":["SA","SR","SF"],"channel_count":3,"nominal_srate":0,"channel_format":"float","units":"microsiemens, seconds, per minute"}}]"#)
}

/// The text of `"key": { ... }` in a JSON document (the braces included), without a parser.
pub fn json_object<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let at = find_key(text, key)?;
    let rest = &text[at..];
    let open = rest.find('{')?;
    let mut depth = 0;
    let mut in_str = false;
    let mut prev = ' ';
    for (i, c) in rest[open..].char_indices() {
        match c {
            '"' if prev != '\\' => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[open..open + i + 1]);
                }
            }
            _ => {}
        }
        prev = c;
    }
    None
}

/// The value of `"key": value` in a flat JSON object: a string's contents, or a bare number
/// or literal.
pub fn json_value<'a>(object: &'a str, key: &str) -> Option<&'a str> {
    let at = find_key(object, key)?;
    let rest = object[at..].trim_start();
    if let Some(s) = rest.strip_prefix('"') {
        return s.find('"').map(|e| &s[..e]);
    }
    let end = rest.find([',', '}', ' ', '\n', '\r']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Writes `text` (a JSON object, e.g. the stock `CONFIG.TXT`) with its `"key": {...}` member
/// replaced by `section` (inserted when missing; removed when `section` is `None`). Everything
/// else is copied unchanged. An empty `text` counts as `{}`.
pub fn json_with_member<W: core::fmt::Write>(text: &str, key: &str, section: Option<&str>, w: &mut W) -> Result<(), &'static str> {
    let body = text.trim();
    let body = if body.is_empty() { "{}" } else { body };
    if !(body.starts_with('{') && body.ends_with('}')) {
        return Err("not a JSON object");
    }
    let put = |w: &mut W, s: &str| w.write_str(s).map_err(|_| "too long");
    match member_span(body, key) {
        Some((start, end)) => {
            // Replace the member in place, or drop it with one adjacent comma.
            let (mut a, mut b) = (start, end);
            if section.is_none() {
                let after = &body[b..];
                let trimmed = after.trim_start();
                if let Some(rest) = trimmed.strip_prefix(',') {
                    b = body.len() - rest.len();
                } else if let Some(i) = body[..a].trim_end().strip_suffix(',').map(|s| s.len()) {
                    a = i;
                }
            }
            put(w, &body[..a])?;
            if let Some(sec) = section {
                put(w, "\"")?;
                put(w, key)?;
                put(w, "\": ")?;
                put(w, sec)?;
            }
            put(w, &body[b..])?;
        }
        None => {
            let Some(sec) = section else { return put(w, body) };
            let inner = body[1..body.len() - 1].trim();
            let close = body.rfind('}').ok_or("not a JSON object")?;
            put(w, body[..close].trim_end())?;
            if !inner.is_empty() {
                put(w, ",")?;
            }
            put(w, "\n  \"")?;
            put(w, key)?;
            put(w, "\": ")?;
            put(w, sec)?;
            put(w, "\n}")?;
        }
    }
    put(w, "\n")
}

/// Byte range of `"key": {...}` in a JSON document, from the key's opening quote to the
/// object's closing brace.
fn member_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let value = json_object(text, key)?;
    let end = value.as_ptr() as usize - text.as_ptr() as usize + value.len();
    let colon = find_key(text, key)?;
    let start = text[..colon].rfind(':')?;
    let quote_end = text[..start].rfind('"')?;
    let quote_start = text[..quote_end].rfind('"')?;
    Some((quote_start, end))
}

/// The `EmotiBitBle` section for `CONFIG.TXT`: the name, the LEDs, and numeric settings by key
/// (absent ones left out).
pub fn write_settings_section<W: core::fmt::Write>(w: &mut W, name: Option<&str>, leds: Option<bool>, numbers: &[(&str, Option<i16>)]) -> core::fmt::Result {
    w.write_char('{')?;
    let mut first = true;
    let mut sep = |w: &mut W| -> core::fmt::Result {
        if !core::mem::take(&mut first) {
            w.write_str(", ")?;
        }
        Ok(())
    };
    if let Some(n) = name {
        sep(w)?;
        write!(w, "\"name\": \"{n}\"")?;
    }
    if let Some(l) = leds {
        sep(w)?;
        write!(w, "\"leds\": {l}")?;
    }
    for (key, value) in numbers {
        if let Some(v) = value {
            sep(w)?;
            write!(w, "\"{key}\": {v}")?;
        }
    }
    w.write_char('}')
}

/// Position just after `"key"` and its colon.
fn find_key(text: &str, key: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = text[from..].find('"') {
        let start = from + i + 1;
        let end = start + text[start..].find('"')?;
        if &text[start..end] == key {
            let after = text[end + 1..].trim_start();
            if let Some(v) = after.strip_prefix(':') {
                return Some(text.len() - v.len());
            }
        }
        from = end + 1;
    }
    None
}

/// Binary SD file framing (`.EBB`): a stream of frames, each starting with a kind byte.
/// - `FRAME_RECORD`: u32 LE device milliseconds, then one 20-byte record.
/// - `FRAME_TEXT`: u16 LE length, then that many bytes of one stock packet line (with newline).
pub const FRAME_RECORD: u8 = 0xEB;
pub const FRAME_TEXT: u8 = 0x54;
/// First bytes of a binary file.
pub const BINARY_MAGIC: &[u8; 8] = b"EMOTIBB1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_print_like_stock() {
        let mut s = Fixed::<64>::new();
        write_value(&mut s, 10, 3, "AX", -0.171875).unwrap();
        assert_eq!(s.as_str(), "10,3,1,AX,1,100,-0.171875\n");
        s.clear();
        write_value(&mut s, 10, 4, "PI", 1094.0).unwrap();
        assert_eq!(s.as_str(), "10,4,1,PI,1,100,1094\n");
        s.clear();
        write_line(&mut s, 5, 1, "RD", &["TL", "TU"]).unwrap();
        assert_eq!(s.as_str(), "5,1,2,RD,1,100,TL,TU\n");
        let l = Line::parse("7,2,1,TL,1,100,2026-11-07_01-20-44-744868").unwrap();
        assert_eq!((l.timestamp, l.number, l.tag), (7, 2, "TL"));
        assert_eq!(l.fields().next(), Some("2026-11-07_01-20-44-744868"));
        assert_eq!(l.fields().count(), 1);
    }

    #[test]
    fn reads_the_settings_section_only() {
        let cfg = r#"{"WifiCredentials":[{"ssid":"x","password":"y"}],"EmotiBitBle":{"name":"lab-3","ppg_hz": 100,"leds":false}}"#;
        let sec = json_object(cfg, "EmotiBitBle").unwrap();
        assert_eq!(json_value(sec, "name"), Some("lab-3"));
        assert_eq!(json_value(sec, "ppg_hz"), Some("100"));
        assert_eq!(json_value(sec, "leds"), Some("false"));
        assert_eq!(json_value(sec, "ssid"), None);
        assert!(json_object(r#"{"WifiCredentials":[]}"#, "EmotiBitBle").is_none());
    }

    #[test]
    fn settings_merge_keeps_the_rest_of_config_txt() {
        let stock = "{\n  \"WifiCredentials\": [{\"ssid\": \"a,b}\", \"password\": \"p{\"}]\n}\n";
        let mut sec = Fixed::<320>::new();
        // Every key at its longest: what the firmware's buffer must hold.
        let numbers = [("ppg_hz", Some(100)), ("led_brightness", Some(31)), ("cpu_mhz", Some(240)), ("sensor_ms", Some(20)), ("conn_ms", Some(500)), ("adv_ms", Some(10240)), ("tx_dbm", Some(-12)), ("ppg_led", Some(255))];
        write_settings_section(&mut sec, Some("lab3-long"), Some(false), &numbers).unwrap();
        assert!(sec.as_str().len() <= 256, "{}", sec.as_str());
        // Insert.
        let mut once = Fixed::<512>::new();
        json_with_member(stock, "EmotiBitBle", Some(sec.as_str()), &mut once).unwrap();
        let v: serde_json::Value = serde_json::from_str(once.as_str()).unwrap();
        assert_eq!(v["WifiCredentials"][0]["ssid"], "a,b}");
        assert_eq!(v["WifiCredentials"][0]["password"], "p{");
        assert_eq!(v["EmotiBitBle"]["ppg_hz"], 100);
        assert_eq!(v["EmotiBitBle"]["leds"], false);
        assert_eq!((v["EmotiBitBle"]["led_brightness"].as_u64(), v["EmotiBitBle"]["cpu_mhz"].as_u64(), v["EmotiBitBle"]["sensor_ms"].as_u64()), (Some(31), Some(240), Some(20)));
        // Replace: the credentials text is untouched byte for byte.
        let mut sec2 = Fixed::<64>::new();
        write_settings_section(&mut sec2, None, None, &[("ppg_hz", Some(25)), ("cpu_mhz", None)]).unwrap();
        let mut twice = Fixed::<512>::new();
        json_with_member(once.as_str(), "EmotiBitBle", Some(sec2.as_str()), &mut twice).unwrap();
        let v: serde_json::Value = serde_json::from_str(twice.as_str()).unwrap();
        assert_eq!(v["EmotiBitBle"], serde_json::json!({"ppg_hz": 25}));
        assert!(twice.as_str().contains("[{\"ssid\": \"a,b}\", \"password\": \"p{\"}]"));
        // Remove.
        let mut gone = Fixed::<512>::new();
        json_with_member(twice.as_str(), "EmotiBitBle", None, &mut gone).unwrap();
        let v: serde_json::Value = serde_json::from_str(gone.as_str()).unwrap();
        assert!(v.get("EmotiBitBle").is_none() && v["WifiCredentials"][0]["ssid"] == "a,b}");
        // A new file, and one that is not an object.
        let mut new = Fixed::<384>::new();
        json_with_member("", "EmotiBitBle", Some(sec.as_str()), &mut new).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(new.as_str()).unwrap()["EmotiBitBle"]["name"], "lab3-long");
        assert!(json_with_member("[1]", "EmotiBitBle", Some("{}"), &mut Fixed::<16>::new()).is_err());
    }

    #[test]
    fn info_json_is_valid_json() {
        let mut s = Fixed::<3072>::new();
        let session = SessionInfo { data_file: "2026-11-07_20-23-15-026091.csv", format: "csv", state: "done", duration_s: 61.5, bytes: 700_000, notes: 2, markers: 0 };
        let info = InfoJson { device_id: "MD-V6-0000356", hardware: "V06a", sku: "MD", firmware_version: "0.1.0", created_at: "2026-11-07_20-23-15-026091", ppg_hz: 25.0, temp_humidity: false, session: Some(session) };
        write_info_json(&mut s, &info).unwrap();
        let v: serde_json::Value = serde_json::from_str(s.as_str()).unwrap();
        assert_eq!(v[0]["info"]["device_id"], "MD-V6-0000356");
        assert_eq!(v[0]["info"]["session"]["duration_s"], 61.5);
        assert_eq!(v[1]["info"]["typeTags"][2], "AZ");
        // The reader the firmware uses for `FS` finds the session again.
        let sec = json_object(s.as_str(), "session").unwrap();
        assert_eq!(json_value(sec, "state"), Some("done"));
        assert_eq!(json_value(sec, "notes"), Some("2"));
    }
}
