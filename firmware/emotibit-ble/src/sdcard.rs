//! SD card recording. Runs on the second core, so card writes (which can take tens of
//! milliseconds) never stall the sensor loop or the radio.
//!
//! Two file formats, chosen by the host at `RB`:
//! - **CSV** (`<time>.csv`): stock EmotiBit packet lines in stock units, with the derived
//!   heart rate and SCR channels, readable by the official DataParser and `emotibit parse`.
//! - **Binary** (`<time>.ebb`): the raw 20-byte records with device time plus the text lines
//!   (see `stock::FRAME_*`), about 3x smaller; `emotibit parse` converts it.
//!
//! `<time>` is the host's local time at `RB` (`2026-11-07_20-23-15-026091`), the stock firmware's
//! file name; each file also has an 8.3 alias (`EBnnnnnn.CSV`) and a `<time>_info.json` beside
//! it, as the stock firmware writes. The card also holds the settings file: the `EmotiBitBle`
//! section of `CONFIG.TXT` (see `control::Config`). Nothing else in that file is read or logged.

use core::cell::Cell;
use core::fmt::Write as _;
use core::ops::ControlFlow;
use core::sync::atomic::Ordering::Relaxed;

use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as M;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Duration, Instant};
use embedded_sdmmc::{Mode, RawDirectory, RawFile, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};

use crate::control::{self, SdCmd, SessionOp};
use crate::stock::{self, Fixed, Records};

/// File times come from the host's clock at `RB`.
static FILE_TIME: Mutex<M, Cell<Timestamp>> = Mutex::new(Cell::new(Timestamp {
    year_since_1970: 56,
    zero_indexed_month: 0,
    zero_indexed_day: 0,
    hours: 0,
    minutes: 0,
    seconds: 0,
}));

struct Clock;

impl TimeSource for Clock {
    fn get_timestamp(&self) -> Timestamp {
        FILE_TIME.lock(|t| t.get())
    }
}

/// `2026-11-07_01-20-44-744868` to a FAT timestamp.
fn parse_stamp(s: &str) -> Option<Timestamp> {
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<u16>().ok();
    Some(Timestamp {
        year_since_1970: (n(0, 4)?.checked_sub(1970)?) as u8,
        zero_indexed_month: (n(5, 7)?.checked_sub(1)?) as u8,
        zero_indexed_day: (n(8, 10)?.checked_sub(1)?) as u8,
        hours: n(11, 13)? as u8,
        minutes: n(14, 16)? as u8,
        seconds: n(17, 19)? as u8,
    })
}

/// Bytes collected before a card write.
const CHUNK: usize = 4096;
/// Directory entry (file size) update interval, so a power loss loses little.
const FLUSH_EVERY: Duration = Duration::from_secs(2);

struct Open {
    file: RawFile,
    binary: bool,
    buf: [u8; CHUNK + 512],
    len: usize,
    last_flush: Instant,
    /// For the session summary in `_info.json`: the host's start time, the data file's name,
    /// the info file's 8.3 alias, the device time at the start, and what was written.
    stamp: Fixed<40>,
    name: Fixed<48>,
    info: Option<Fixed<16>>,
    start_ms: u32,
    bytes: u64,
    notes: u32,
    markers: u32,
}

/// Runs the writer. `fast` raises the SPI clock once the card is initialised.
pub async fn run<S, D>(spi: S, delay: D, fast: impl FnOnce(&mut S)) -> !
where
    S: embedded_hal::spi::SpiDevice<u8>,
    D: embedded_hal::delay::DelayNs,
{
    // Cards can be slow to come up: a few tries.
    let card = SdCard::new(spi, delay);
    let mut size = card.num_bytes();
    for _ in 0..3 {
        if size.is_ok() {
            break;
        }
        embassy_time::Timer::after(Duration::from_millis(300)).await;
        card.mark_card_uninit();
        size = card.num_bytes();
    }
    let card_bytes = match size {
        Ok(n) => {
            log::info!("SD card: {} MB, {:?}", n / 1_000_000, card.get_card_type());
            n
        }
        Err(e) => {
            log::info!("SD card: none ({:?})", e);
            set_config(None);
            loop {
                // Nothing to record to; keep the channels from filling.
                match select(control::SD_CMD.receive(), control::SD_DATA.receive()).await {
                    Either::First(_) | Either::Second(_) => {}
                }
            }
        }
    };
    card.spi(fast);
    let mgr: VolumeManager<_, _, 2, 2, 1> = VolumeManager::new_with_limits(card, Clock, 5000);
    let root = match mgr.open_raw_volume(VolumeIdx(0)).and_then(|v| mgr.open_root_dir(v)) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("SD card: no FAT volume ({:?})", e);
            set_config(None);
            loop {
                let _ = select(control::SD_CMD.receive(), control::SD_DATA.receive()).await;
            }
        }
    };
    control::SD_PRESENT.store(true, Relaxed);
    set_config(read_config(&mgr, root));

    let mut records = Records::new();
    let mut derived = crate::derive::Derived::new(25.0, 15.625);
    let mut open: Option<Open> = None;
    // A file being written by the host (`FO`/`FA`/`FC`): name, handle, bytes so far.
    let mut put: Option<(Fixed<16>, RawFile, u32)> = None;
    let mut fill_due = Instant::now();
    loop {
        if Instant::now() >= fill_due {
            fill_due = Instant::now() + Duration::from_secs(10);
            report_fill(&mgr, root, card_bytes, open.as_ref().map_or(0, |o| o.len as u64));
        }
        match select(control::SD_CMD.receive(), control::SD_DATA.receive()).await {
            Either::First(SdCmd::Begin { stamp, binary }) => {
                if let Some(o) = open.take() {
                    close(&mgr, o);
                }
                if let Some(t) = parse_stamp(stamp.as_str()) {
                    FILE_TIME.lock(|c| c.set(t));
                }
                let (name, file) = match create(&mgr, root, stamp.as_str(), if binary { "ebb" } else { "csv" }) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("SD card: cannot create a file ({:?})", e);
                        continue;
                    }
                };
                log::info!("recording to {} ({})", name.as_str(), if binary { "binary" } else { "csv" });
                let board = control::board();
                derived = crate::derive::Derived::new(board.ppg_hz() as f64, 15.625);
                let mut o = Open {
                    file,
                    binary,
                    buf: [0; CHUNK + 512],
                    len: 0,
                    last_flush: Instant::now(),
                    stamp,
                    name,
                    info: None,
                    start_ms: control::now_ms(),
                    bytes: 0,
                    notes: 0,
                    markers: 0,
                };
                o.info = write_info(&mgr, root, &o, "recording");
                if binary {
                    append(&mut o, stock::BINARY_MAGIC);
                }
                // The stock file name, and the time the file starts.
                let mut l = control::Text::new();
                let _ = stock::write_line(&mut l, control::now_ms(), control::next_sd_number(), "UN", &[o.stamp.as_str(), "emotibit-ble recording start"]);
                text(&mut o, l.as_str());
                set_file_name(o.name.as_str());
                open = Some(o);
                control::RECORDING.store(true, Relaxed);
                control::RESEND_CAL.store(true, Relaxed);
                let _ = control::EVENTS.try_send(control::mode_line());
            }
            Either::First(SdCmd::End) => {
                control::RECORDING.store(false, Relaxed);
                // Records already queued belong to the file.
                if let Some(mut o) = open.take() {
                    while let Ok((ts, rec)) = control::SD_DATA.try_receive() {
                        data(&mut o, &mut records, &mut derived, ts, &rec);
                    }
                    write_out(&mgr, &mut o);
                    // The session summary, now that it is complete.
                    write_info(&mgr, root, &o, "done");
                    close(&mgr, o);
                    log::info!("recording stopped");
                }
                set_file_name("");
                let _ = control::EVENTS.try_send(control::mode_line());
            }
            Either::First(SdCmd::Line(l)) => {
                if let Some(o) = open.as_mut() {
                    match stock::Line::parse(l.as_str()).map(|p| p.tag) {
                        Some("UN") => o.notes += 1,
                        Some("LM") => o.markers += 1,
                        _ => {}
                    }
                    text(o, l.as_str());
                }
            }
            Either::First(SdCmd::Sessions) => sessions(&mgr, root, open.as_ref()).await,
            Either::First(SdCmd::List) => list(&mgr, root).await,
            Either::First(SdCmd::Get { name, offset, len }) => {
                if open.is_some() {
                    // Reading would stall the recording.
                    control::emit("FE", &[name.as_str(), "busy recording"], true);
                } else {
                    send_file(&mgr, root, name.as_str(), offset, len).await;
                }
            }
            Either::First(SdCmd::PutOpen(name)) => {
                if let Some((_, f, _)) = put.take() {
                    let _ = mgr.close_file(f);
                }
                if open.is_some() {
                    control::emit("FC", &[name.as_str(), "busy recording"], true);
                } else {
                    match mgr.open_file_in_dir(root, name.as_str(), Mode::ReadWriteCreateOrTruncate) {
                        Ok(f) => put = Some((name, f, 0u32)),
                        Err(e) => {
                            log::warn!("SD card: cannot write {} ({:?})", name.as_str(), e);
                            control::emit("FC", &[name.as_str(), "cannot open"], true);
                        }
                    }
                }
            }
            Either::First(SdCmd::PutData(b64)) => {
                if let Some((_, f, n)) = put.as_mut() {
                    let mut bytes = [0u8; 384];
                    let len = unbase64(b64.as_str(), &mut bytes);
                    if mgr.write(*f, &bytes[..len]).is_ok() {
                        *n += len as u32;
                    }
                }
            }
            Either::First(SdCmd::PutClose) => {
                if let Some((name, f, n)) = put.take() {
                    let _ = mgr.close_file(f);
                    let mut num = Fixed::<12>::new();
                    let _ = write!(num, "{n}");
                    control::emit("FC", &[name.as_str(), num.as_str()], true);
                }
            }
            Either::First(SdCmd::Delete(name)) => {
                let n = name.as_str();
                let result = if open.is_some() {
                    "busy recording"
                } else if n.eq_ignore_ascii_case("CONFIG.TXT") {
                    // It holds the WiFi credentials of the stock firmware.
                    "protected"
                } else if mgr.delete_file_any_name(root, n).is_ok() {
                    "deleted"
                } else {
                    "not found"
                };
                control::emit("FX", &[n, result], true);
            }
            Either::First(SdCmd::Session { op, name, to }) => {
                let tag = op.tag();
                let result = if open.is_some() {
                    Err("busy recording")
                } else if put.is_some() {
                    Err("busy writing a file")
                } else {
                    session_op(&mgr, root, op, name.as_str(), to.as_str()).await
                };
                match result {
                    Ok(new) => control::emit(tag, &[name.as_str(), "ok", new.as_str()], true),
                    Err(why) => control::emit(tag, &[name.as_str(), "error", why], true),
                }
            }
            Either::First(SdCmd::SaveConfig(cfg)) => {
                let result = if open.is_some() { Err("busy recording") } else { save_config(&mgr, root, cfg.as_ref()) };
                match result {
                    Ok(what) => control::emit("CW", &["ok", what], true),
                    Err(why) => control::emit("CW", &["error", why], true),
                }
            }
            Either::First(SdCmd::Fill) => report_fill(&mgr, root, card_bytes, open.as_ref().map_or(0, |o| o.len as u64)),
            Either::Second((ts, rec)) => {
                if let Some(o) = open.as_mut() {
                    data(o, &mut records, &mut derived, ts, &rec);
                }
            }
        }
        if let Some(o) = open.as_mut() {
            if o.len >= CHUNK {
                write_out(&mgr, o);
            }
            if o.last_flush.elapsed() >= FLUSH_EVERY {
                write_out(&mgr, o);
                if let Err(e) = mgr.flush_file(o.file) {
                    log::warn!("SD card: flush failed ({:?})", e);
                }
                o.last_flush = Instant::now();
            }
        }
    }
}

/// Lists the root directory on the events characteristic.
async fn list<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, root: RawDirectory) {
    let mut entries: heapless::Vec<(Fixed<48>, u32), 64> = heapless::Vec::new();
    let mut storage = [0u8; 128];
    let mut lfn = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let _ = mgr.iterate_dir_lfn(root, &mut lfn, |e, long| {
        if !e.attributes.is_directory() && !e.attributes.is_volume() {
            let mut n = Fixed::<48>::new();
            // The long name when it fits, else the 8.3 one.
            if long.is_none_or(|l| n.write_str(l).is_err()) {
                n.clear();
                let _ = write!(n, "{}", e.name);
            }
            let _ = entries.push((n, e.size));
        }
        ControlFlow::Continue(())
    });
    for (name, size) in &entries {
        let mut sz = Fixed::<12>::new();
        let _ = write!(sz, "{size}");
        control::EVENTS.send(control::line("FL", &[name.as_str(), sz.as_str()])).await;
    }
    control::EVENTS.send(control::line("FL", &[])).await;
}

/// Bytes per `FD` line (base64 grows it by a third; the line must fit `control::Text`).
const FILE_CHUNK: usize = 330;
/// Pause between `FD` lines.
const FILE_PACE_MS: u64 = 60;

/// Sends `len` bytes (0: the rest) of a file from `from` on the events characteristic, base64
/// in `FD` lines.
async fn send_file<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, root: RawDirectory, name: &str, from: u32, len: u32) {
    let opened = mgr.open_file_in_dir(root, name, Mode::ReadOnly).or_else(|_| mgr.open_long_name_file_in_dir(root, name, Mode::ReadOnly));
    let file = match opened {
        Ok(f) => f,
        Err(e) => {
            log::warn!("SD card: cannot open {} ({:?})", name, e);
            control::emit("FE", &[name, "not found"], true);
            return;
        }
    };
    let size = mgr.file_length(file).unwrap_or(0);
    let mut num = Fixed::<12>::new();
    let _ = write!(num, "{size}");
    control::EVENTS.send(control::line("FG", &[name, num.as_str()])).await;
    let offset_ok = from <= size && mgr.file_seek_from_start(file, from).is_ok();
    let mut offset = if offset_ok { from } else { size };
    let end = if len == 0 { size } else { size.min(offset.saturating_add(len)) };
    let mut buf = [0u8; FILE_CHUNK];
    while offset < end {
        let want = FILE_CHUNK.min((end - offset) as usize);
        let n = match mgr.read(file, &mut buf[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut b64 = Fixed::<448>::new();
        base64(&buf[..n], &mut b64);
        num.clear();
        let _ = write!(num, "{offset}");
        control::EVENTS.send(control::line("FD", &[num.as_str(), b64.as_str()])).await;
        offset += n as u32;
        embassy_time::Timer::after(Duration::from_millis(FILE_PACE_MS)).await;
    }
    let _ = mgr.close_file(file);
    num.clear();
    let _ = write!(num, "{offset}");
    control::EVENTS.send(control::line("FE", &[name, num.as_str()])).await;
    log::info!("sent {} ({} bytes)", name, offset);
}

/// Decodes base64 into `out`, returning the length; stops at the first invalid character.
fn unbase64(s: &str, out: &mut [u8]) -> usize {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut n = 0;
    for q in s.trim().as_bytes().chunks(4) {
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        let mut v = 0u32;
        for &c in &q[..q.len() - pad] {
            let Some(x) = val(c) else { return n };
            v = v << 6 | x;
        }
        v <<= 6 * (4 - (q.len() - pad)) as u32;
        let bytes = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        for &b in &bytes[..(q.len() - pad).saturating_sub(1)] {
            if n == out.len() {
                return n;
            }
            out[n] = b;
            n += 1;
        }
    }
    n
}

fn base64<const N: usize>(data: &[u8], out: &mut Fixed<N>) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let v = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let mut q = [b'='; 4];
        for (i, slot) in q.iter_mut().enumerate().take(c.len() + 1) {
            *slot = T[(v >> (18 - 6 * i) & 63) as usize];
        }
        let _ = out.write_str(core::str::from_utf8(&q).unwrap_or(""));
    }
}

fn set_file_name(name: &str) {
    control::FILE_NAME.lock(|f| {
        let mut f = f.borrow_mut();
        f.clear();
        let _ = f.write_str(name);
    });
}

/// Next free 8.3 alias number (`EBnnnnnn.*`) in the root directory.
fn next_alias<D: embedded_sdmmc::BlockDevice, T: TimeSource>(
    mgr: &VolumeManager<D, T, 2, 2, 1>,
    root: RawDirectory,
) -> Result<u32, embedded_sdmmc::Error<D::Error>> {
    let mut highest = 0u32;
    mgr.iterate_dir(root, |e| {
        let base = e.name.base_name();
        if base.len() == 8 && base.starts_with(b"EB")
            && let Some(n) = core::str::from_utf8(&base[2..]).ok().and_then(|s| s.parse::<u32>().ok()) {
                highest = highest.max(n);
            }
        ControlFlow::Continue(())
    })?;
    Ok(highest + 1)
}

/// True for a stamp in the stock `TL` form, usable as a file name.
fn stamp_ok(stamp: &str) -> bool {
    parse_stamp(stamp).is_some() && stamp.bytes().all(|c| c.is_ascii_digit() || c == b'-' || c == b'_')
}

/// Creates `<stamp>.<ext>` (with an `EBnnnnnn.<EXT>` alias), or just the alias when the host
/// sent no usable stamp. Returns the name the file is known by.
fn create<D: embedded_sdmmc::BlockDevice, T: TimeSource>(
    mgr: &VolumeManager<D, T, 2, 2, 1>,
    root: RawDirectory,
    stamp: &str,
    ext: &str,
) -> Result<(Fixed<48>, RawFile), embedded_sdmmc::Error<D::Error>> {
    let mut alias = Fixed::<16>::new();
    let _ = write!(alias, "EB{:06}.", next_alias(mgr, root)?);
    for c in ext.chars() {
        let _ = alias.write_char(c.to_ascii_uppercase());
    }
    let mut name = Fixed::<48>::new();
    if stamp_ok(stamp) {
        let _ = write!(name, "{stamp}.{ext}");
        let file = mgr.create_file_with_long_name(root, alias.as_str(), name.as_str())?;
        Ok((name, file))
    } else {
        let _ = name.write_str(alias.as_str());
        let file = mgr.open_file_in_dir(root, alias.as_str(), Mode::ReadWriteCreate)?;
        Ok((name, file))
    }
}

/// Writes `<stamp>_info.json` beside the recording, as the stock firmware does, with
/// emotibit-ble's `session` summary (see `stock::SessionInfo`). Created at the start (state
/// `recording`) and rewritten through its 8.3 alias when the recording ends (`done`). Returns
/// the alias.
fn write_info<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, root: RawDirectory, o: &Open, state: &str) -> Option<Fixed<16>> {
    let stamp = o.stamp.as_str();
    if !stamp_ok(stamp) {
        return None;
    }
    let (alias, file) = match &o.info {
        Some(alias) => (*alias, mgr.open_file_in_dir(root, alias.as_str(), Mode::ReadWriteTruncate).ok()?),
        None => {
            let mut long = Fixed::<48>::new();
            let _ = write!(long, "{stamp}_info.json");
            let mut alias = Fixed::<16>::new();
            let _ = write!(alias, "EB{:06}.JSN", next_alias(mgr, root).ok()?);
            match mgr.create_file_with_long_name(root, alias.as_str(), long.as_str()) {
                Ok(f) => (alias, f),
                Err(e) => {
                    log::warn!("SD card: cannot write the info file ({:?})", e);
                    return None;
                }
            }
        }
    };
    let board = control::board();
    let si7013 = control::FOUND.load(Relaxed) & control::found::SI7013 != 0;
    let mut json = Fixed::<3072>::new();
    let _ = stock::write_info_json(&mut json, &stock::InfoJson {
        device_id: board.device_id.as_str(),
        hardware: board.hardware.as_str(),
        sku: board.sku.as_str(),
        firmware_version: env!("CARGO_PKG_VERSION"),
        created_at: stamp,
        ppg_hz: board.ppg_hz() as f64,
        temp_humidity: si7013,
        session: Some(stock::SessionInfo {
            data_file: o.name.as_str(),
            format: if o.binary { "binary" } else { "csv" },
            state,
            duration_s: control::now_ms().wrapping_sub(o.start_ms) as f64 / 1000.0,
            bytes: o.bytes,
            notes: o.notes,
            markers: o.markers,
        }),
    });
    if let Err(e) = mgr.write(file, json.as_bytes()) {
        log::warn!("SD card: info file write failed ({:?})", e);
    }
    let _ = mgr.close_file(file);
    Some(alias)
}

/// Answers `FS`: one `FS,<data file>,<format>,<state>,<duration s>,<bytes>,<notes>,<markers>,<created>`
/// line per recording (`created`: the host's local time at `RB`), from the `session` member of each `_info.json` (the file being written
/// is reported live), then data files without one (`state` `unknown`), then `FS` alone.
async fn sessions<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, root: RawDirectory, open: Option<&Open>) {
    // Every file once: long name (or 8.3), size.
    let mut entries: heapless::Vec<(Fixed<48>, u32), 96> = heapless::Vec::new();
    let mut storage = [0u8; 128];
    let mut lfn = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let _ = mgr.iterate_dir_lfn(root, &mut lfn, |e, long| {
        if !e.attributes.is_directory() && !e.attributes.is_volume() {
            let mut n = Fixed::<48>::new();
            if long.is_none_or(|l| n.write_str(l).is_err()) {
                n.clear();
                let _ = write!(n, "{}", e.name);
            }
            let _ = entries.push((n, e.size));
        }
        ControlFlow::Continue(())
    });
    let size_of = |name: &str| entries.iter().find(|(n, _)| n.as_str() == name).map_or(0, |(_, s)| *s);
    let mut described: heapless::Vec<Fixed<48>, 96> = heapless::Vec::new();
    for (info_name, _) in entries.iter().filter(|(n, _)| n.as_str().ends_with("_info.json")) {
        let Ok(f) = mgr.open_long_name_file_in_dir(root, info_name.as_str(), Mode::ReadOnly) else { continue };
        let mut buf = [0u8; 3072];
        let n = mgr.read(f, &mut buf).unwrap_or(0);
        let _ = mgr.close_file(f);
        let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
        let Some(sec) = stock::json_object(text, "session") else { continue };
        let data = stock::json_value(sec, "data_file").unwrap_or("");
        let field = |k: &str| stock::json_value(sec, k).unwrap_or("");
        let mut num = Fixed::<16>::new();
        let mut dur = Fixed::<16>::new();
        let mut notes = Fixed::<8>::new();
        let mut markers = Fixed::<8>::new();
        let mut state = field("state");
        match open.filter(|o| o.name.as_str() == data) {
            // The recording in progress: live numbers.
            Some(o) => {
                state = "recording";
                let _ = write!(num, "{}", o.bytes + o.len as u64);
                let _ = write!(dur, "{:.1}", control::now_ms().wrapping_sub(o.start_ms) as f64 / 1000.0);
                let _ = write!(notes, "{}", o.notes);
                let _ = write!(markers, "{}", o.markers);
            }
            None => {
                let _ = write!(num, "{}", size_of(data));
                let _ = dur.write_str(if state == "done" { field("duration_s") } else { "" });
                let _ = notes.write_str(field("notes"));
                let _ = markers.write_str(field("markers"));
                // Started but never closed (power lost, reset): the file is what is left.
                if state == "recording" {
                    state = "interrupted";
                }
            }
        }
        // The start time: names can change (`FR`), the info file keeps it.
        let created = stock::json_value(text, "created_at").unwrap_or("");
        control::EVENTS.send(control::line("FS", &[data, field("format"), state, dur.as_str(), num.as_str(), notes.as_str(), markers.as_str(), created])).await;
        let mut d = Fixed::<48>::new();
        let _ = d.write_str(data);
        let _ = described.push(d);
    }
    // Data files without a session summary: older recordings, or the stock firmware's.
    for (name, size) in &entries {
        let n = name.as_str();
        let lower_csv = n.len() > 4 && n[n.len() - 4..].eq_ignore_ascii_case(".csv");
        let data = lower_csv || n.to_ascii_uppercase().ends_with(".EBB");
        if !data || described.iter().any(|d| d.as_str() == n) || n.eq_ignore_ascii_case("CONFIG.TXT") {
            continue;
        }
        let mut num = Fixed::<16>::new();
        let _ = write!(num, "{size}");
        let format = if n.to_ascii_uppercase().ends_with(".EBB") { "binary" } else { "csv" };
        control::EVENTS.send(control::line("FS", &[n, format, "unknown", "", num.as_str(), "", "", ""])).await;
    }
    control::EVENTS.send(control::line("FS", &[])).await;
}

/// Longest new name (without the extension) for `FR`/`FY`: `<name>_info.json` must fit 48.
const STEM_MAX: usize = 36;

/// A new recording name: letters, digits, space, `-`, `_`, `.`, `(`, `)`; no leading or
/// trailing space or dot.
fn stem_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= STEM_MAX
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || b" -_.()".contains(&c))
        && !s.starts_with([' ', '.'])
        && !s.ends_with([' ', '.'])
}

/// `<stem>.csv|ebb` to (`<stem>`, extension); None for anything else.
fn split_data_name(name: &str) -> Option<(&str, &str)> {
    let (stem, ext) = name.rsplit_once('.')?;
    (ext.eq_ignore_ascii_case("csv") || ext.eq_ignore_ascii_case("ebb")).then_some((stem, ext))
}

fn exists<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, root: RawDirectory, name: &str) -> bool {
    match mgr.open_long_name_file_in_dir(root, name, Mode::ReadOnly).or_else(|_| mgr.open_file_in_dir(root, name, Mode::ReadOnly)) {
        Ok(f) => {
            let _ = mgr.close_file(f);
            true
        }
        Err(_) => false,
    }
}

fn open_any<D: embedded_sdmmc::BlockDevice, T: TimeSource>(
    mgr: &VolumeManager<D, T, 2, 2, 1>,
    root: RawDirectory,
    name: &str,
) -> Result<RawFile, embedded_sdmmc::Error<D::Error>> {
    mgr.open_long_name_file_in_dir(root, name, Mode::ReadOnly).or_else(|_| mgr.open_file_in_dir(root, name, Mode::ReadOnly))
}

/// A recording's `_info.json` with its `data_file` pointed at `new_data`.
fn read_info_for<D: embedded_sdmmc::BlockDevice, T: TimeSource>(
    mgr: &VolumeManager<D, T, 2, 2, 1>,
    root: RawDirectory,
    info: &str,
    old_data: &str,
    new_data: &str,
) -> Option<Fixed<3072>> {
    let f = mgr.open_long_name_file_in_dir(root, info, Mode::ReadOnly).ok()?;
    let mut buf = [0u8; 3072];
    let n = mgr.read(f, &mut buf).unwrap_or(0);
    let _ = mgr.close_file(f);
    let text = core::str::from_utf8(&buf[..n]).ok()?;
    let mut old = Fixed::<52>::new();
    let _ = write!(old, "\"{old_data}\"");
    let mut out = Fixed::<3072>::new();
    match text.split_once(old.as_str()) {
        Some((a, b)) => {
            let _ = write!(out, "{a}\"{new_data}\"{b}");
        }
        None => {
            let _ = out.write_str(text);
        }
    }
    Some(out)
}

/// A new 8.3 alias `EBnnnnnn.<ext>`.
fn alias<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, root: RawDirectory, ext: &str) -> Option<Fixed<16>> {
    let mut a = Fixed::<16>::new();
    let _ = write!(a, "EB{:06}.", next_alias(mgr, root).ok()?);
    for c in ext.chars() {
        let _ = a.write_char(c.to_ascii_uppercase());
    }
    Some(a)
}

/// Bytes copied between `FY,<name>,copying,<done>,<total>` progress lines.
const COPY_PROGRESS: u32 = 256 * 1024;

/// Copies `from` to a new file `to` (long name, new alias). On failure the partial copy is
/// removed.
async fn copy_file<D: embedded_sdmmc::BlockDevice, T: TimeSource>(
    mgr: &VolumeManager<D, T, 2, 2, 1>,
    root: RawDirectory,
    from: &str,
    to: &str,
    ext: &str,
    progress: Option<&str>,
) -> Result<(), &'static str> {
    let src = open_any(mgr, root, from).map_err(|_| "not found")?;
    let total = mgr.file_length(src).unwrap_or(0);
    let Some(a) = alias(mgr, root, ext) else {
        let _ = mgr.close_file(src);
        return Err("card error");
    };
    let dst = match mgr.create_file_with_long_name(root, a.as_str(), to) {
        Ok(f) => f,
        Err(_) => {
            let _ = mgr.close_file(src);
            return Err("cannot create the copy");
        }
    };
    let mut buf = [0u8; CHUNK];
    let mut done = 0u32;
    let mut shown = 0u32;
    let result = loop {
        match mgr.read(src, &mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if mgr.write(dst, &buf[..n]).is_err() {
                    break Err("write failed (card full?)");
                }
                done += n as u32;
            }
            Err(_) => break Err("read failed"),
        }
        if let Some(name) = progress.filter(|_| done - shown >= COPY_PROGRESS) {
            shown = done;
            let mut d = Fixed::<12>::new();
            let mut t = Fixed::<12>::new();
            let _ = write!(d, "{done}");
            let _ = write!(t, "{total}");
            control::EVENTS.send(control::line("FY", &[name, "copying", d.as_str(), t.as_str()])).await;
        }
    };
    let _ = mgr.close_file(src);
    let _ = mgr.close_file(dst);
    if result.is_err() {
        let _ = mgr.delete_file_any_name(root, to);
    }
    result
}

/// `FR`/`FY`/`FZ` on the recording whose data file is `name` (and its `_info.json`, when there
/// is one). Returns the new data file's name (`name` for a delete).
async fn session_op<D: embedded_sdmmc::BlockDevice, T: TimeSource>(
    mgr: &VolumeManager<D, T, 2, 2, 1>,
    root: RawDirectory,
    op: SessionOp,
    name: &str,
    to: &str,
) -> Result<Fixed<48>, &'static str> {
    let (stem, ext) = split_data_name(name).ok_or("not a recording")?;
    if !exists(mgr, root, name) {
        return Err("not found");
    }
    let mut old_info = Fixed::<48>::new();
    let has_info = write!(old_info, "{stem}_info.json").is_ok() && exists(mgr, root, old_info.as_str());
    let mut new_data = Fixed::<48>::new();
    let mut new_info = Fixed::<48>::new();
    if op == SessionOp::Delete {
        mgr.delete_file_any_name(root, name).map_err(|_| "cannot delete")?;
        if has_info {
            let _ = mgr.delete_file_any_name(root, old_info.as_str());
        }
        let _ = new_data.write_str(name);
        log::info!("deleted {}", name);
        return Ok(new_data);
    }
    if !stem_name_ok(to) {
        return Err("bad new name (letters digits space - _ . ( ) up to 36)");
    }
    // Lower case, like the stock names.
    let _ = write!(new_data, "{to}.{}", if ext.eq_ignore_ascii_case("csv") { "csv" } else { "ebb" });
    let _ = write!(new_info, "{to}_info.json");
    if exists(mgr, root, new_data.as_str()) || (has_info && exists(mgr, root, new_info.as_str())) {
        return Err("name taken");
    }
    let info_text = if has_info { read_info_for(mgr, root, old_info.as_str(), name, new_data.as_str()) } else { None };
    match op {
        SessionOp::Rename => {
            let a = alias(mgr, root, ext).ok_or("card error")?;
            mgr.rename_file_long(root, name, a.as_str(), new_data.as_str()).map_err(|_| "cannot rename")?;
            if has_info {
                let a = alias(mgr, root, "JSN").ok_or("card error")?;
                mgr.rename_file_long(root, old_info.as_str(), a.as_str(), new_info.as_str()).map_err(|_| "cannot rename the info file")?;
                if let Some(t) = &info_text
                    && let Ok(f) = mgr.open_file_in_dir(root, a.as_str(), Mode::ReadWriteTruncate) {
                        let _ = mgr.write(f, t.as_bytes());
                        let _ = mgr.close_file(f);
                    }
            }
            log::info!("renamed {} to {}", name, new_data.as_str());
        }
        SessionOp::Copy => {
            copy_file(mgr, root, name, new_data.as_str(), ext, Some(name)).await?;
            if let Some(t) = &info_text {
                let created = alias(mgr, root, "JSN").and_then(|a| mgr.create_file_with_long_name(root, a.as_str(), new_info.as_str()).ok());
                match created {
                    Some(f) => {
                        let _ = mgr.write(f, t.as_bytes());
                        let _ = mgr.close_file(f);
                    }
                    None => log::warn!("SD card: cannot write the copy's info file"),
                }
            }
            log::info!("copied {} to {}", name, new_data.as_str());
        }
        SessionOp::Delete => unreachable!(),
    }
    Ok(new_data)
}

/// Sends `SD,<percent used>`: the root directory's files against the card size.
fn report_fill<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, root: RawDirectory, card_bytes: u64, pending: u64) {
    let mut used = pending;
    let _ = mgr.iterate_dir(root, |e| {
        used += e.size as u64;
        ControlFlow::Continue(())
    });
    let pct = used as f64 * 100.0 / card_bytes.max(1) as f64;
    let mut v = Fixed::<16>::new();
    let _ = write!(v, "{pct:.2}");
    control::emit("SD", &[v.as_str()], true);
}

/// Largest `CONFIG.TXT` handled.
const CONFIG_MAX: usize = 4096;

/// Puts `cfg` into `CONFIG.TXT`'s `EmotiBitBle` section (`None` removes it), keeping the rest of
/// the file unchanged. The first time, the original is copied to `CONFIG.BAK`. The file's
/// contents are never logged.
fn save_config<D: embedded_sdmmc::BlockDevice, T: TimeSource>(
    mgr: &VolumeManager<D, T, 2, 2, 1>,
    root: RawDirectory,
    cfg: Option<&control::Config>,
) -> Result<&'static str, &'static str> {
    let mut old = [0u8; CONFIG_MAX];
    let mut n = 0;
    let existed = match mgr.open_file_in_dir(root, "CONFIG.TXT", Mode::ReadOnly) {
        Ok(f) => {
            let len = mgr.file_length(f).unwrap_or(0) as usize;
            if len >= CONFIG_MAX {
                let _ = mgr.close_file(f);
                return Err("CONFIG.TXT is too large to edit here");
            }
            while n < len {
                match mgr.read(f, &mut old[n..len]) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => n += k,
                }
            }
            let _ = mgr.close_file(f);
            true
        }
        Err(_) => false,
    };
    let text = core::str::from_utf8(&old[..n]).map_err(|_| "CONFIG.TXT is not text")?;
    let mut section = Fixed::<256>::new();
    if let Some(c) = cfg {
        let numbers: [(&str, Option<i16>); crate::tuning::N] = core::array::from_fn(|i| (crate::tuning::NUMBERS[i].key, c.values[i]));
        stock::write_settings_section(&mut section, c.name.as_ref().map(|n| n.as_str()), c.leds, &numbers).map_err(|_| "settings too long")?;
    }
    let mut new = Fixed::<{ CONFIG_MAX + 288 }>::new();
    stock::json_with_member(text, "EmotiBitBle", cfg.map(|_| section.as_str()), &mut new).map_err(|_| "CONFIG.TXT is not a JSON object; left unchanged")?;
    if !existed && cfg.is_none() {
        return Ok("no CONFIG.TXT");
    }
    // Keep the user's original once, before the first change.
    if existed && mgr.find_directory_entry(root, "CONFIG.BAK").is_err() {
        let bak = mgr.open_file_in_dir(root, "CONFIG.BAK", Mode::ReadWriteCreate).map_err(|_| "cannot write CONFIG.BAK")?;
        let ok = mgr.write(bak, &old[..n]).is_ok();
        let _ = mgr.close_file(bak);
        if !ok {
            return Err("cannot write CONFIG.BAK");
        }
    }
    let f = mgr.open_file_in_dir(root, "CONFIG.TXT", Mode::ReadWriteCreateOrTruncate).map_err(|_| "cannot write CONFIG.TXT")?;
    let ok = mgr.write(f, new.as_bytes()).is_ok();
    let _ = mgr.close_file(f);
    if !ok {
        return Err("CONFIG.TXT write failed");
    }
    log::info!("settings {} CONFIG.TXT", if cfg.is_some() { "written to" } else { "removed from" });
    Ok(if cfg.is_some() { "written to CONFIG.TXT" } else { "removed from CONFIG.TXT" })
}

fn set_config(c: Option<control::Config>) {
    control::CONFIG.lock(|cfg| *cfg.borrow_mut() = c);
    control::CONFIG_DONE.store(true, Relaxed);
}

/// The `"EmotiBitBle"` object of `CONFIG.TXT`, for example
/// `{"WifiCredentials": [...], "EmotiBitBle": {"name": "lab-3", "ppg_hz": 100, "leds": true}}`;
/// also `profile` and every key of `tuning::NUMBERS` (a key wins over the profile).
/// The rest of the file (WiFi credentials) is neither kept nor logged.
fn read_config<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, root: RawDirectory) -> Option<control::Config> {
    let mut cfg = control::Config::default();
    let Ok(file) = mgr.open_file_in_dir(root, "CONFIG.TXT", Mode::ReadOnly) else {
        log::info!("settings: no CONFIG.TXT");
        return None;
    };
    let mut buf = [0u8; 2048];
    let n = mgr.read(file, &mut buf).unwrap_or(0);
    let _ = mgr.close_file(file);
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    let Some(section) = stock::json_object(text, "EmotiBitBle") else {
        log::info!("settings: CONFIG.TXT has no EmotiBitBle section");
        return None;
    };
    if let Some(name) = stock::json_value(section, "name") {
        let mut f = Fixed::<24>::new();
        // Letters, digits, '-' and '_' only: it becomes part of the advertised name.
        for c in name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(9) {
            let _ = f.write_char(c);
        }
        if !f.as_str().is_empty() {
            cfg.name = Some(f);
        }
    }
    cfg.leds = stock::json_value(section, "leds").and_then(|v| match v {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    });
    // A profile first, then single keys over it.
    if let Some(p) = stock::json_value(section, "profile").and_then(crate::tuning::profile) {
        cfg.leds = cfg.leds.or(Some(p.leds));
        cfg.values = p.values;
    }
    for (i, setting) in crate::tuning::NUMBERS.iter().enumerate() {
        if let Some(v) = stock::json_value(section, setting.key).and_then(|v| v.parse::<i16>().ok()).filter(|v| setting.allowed.contains(*v)) {
            cfg.values[i] = Some(v);
        }
    }
    log::info!(
        "settings: name {} leds {} {:?}",
        cfg.name.as_ref().map_or("(default)", |n| n.as_str()),
        cfg.leds.unwrap_or(true),
        cfg.values
    );
    Some(cfg)
}

fn append(o: &mut Open, bytes: &[u8]) {
    let n = bytes.len().min(o.buf.len() - o.len);
    o.buf[o.len..o.len + n].copy_from_slice(&bytes[..n]);
    o.len += n;
}

fn text(o: &mut Open, line: &str) {
    if o.binary {
        append(o, &[stock::FRAME_TEXT]);
        append(o, &(line.len() as u16).to_le_bytes());
    }
    append(o, line.as_bytes());
}

fn data(o: &mut Open, records: &mut Records, derived: &mut crate::derive::Derived, ts: u32, rec: &[u8; 20]) {
    if o.binary {
        append(o, &[stock::FRAME_RECORD]);
        append(o, &ts.to_le_bytes());
        append(o, rec);
        return;
    }
    let count = (rec[1] as usize).min(4);
    let mut samples = [0i32; 4];
    for (i, s) in samples.iter_mut().take(count).enumerate() {
        *s = i32::from_le_bytes(rec[4 + 4 * i..8 + 4 * i].try_into().unwrap());
    }
    let mut lines = Fixed::<768>::new();
    records.convert(rec[0], &samples[..count], |tag, v| {
        let _ = stock::write_value(&mut lines, ts, control::next_sd_number(), tag, v);
        // Heart rate and SCR, as the stock firmware writes them.
        derived.push(tag, v, |dtag, dv| {
            let _ = stock::write_value(&mut lines, ts, control::next_sd_number(), dtag, dv);
        });
    });
    append(o, lines.as_bytes());
}

fn write_out<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, o: &mut Open) {
    if o.len == 0 {
        return;
    }
    if let Err(e) = mgr.write(o.file, &o.buf[..o.len]) {
        log::warn!("SD card: write failed ({:?})", e);
    } else {
        o.bytes += o.len as u64;
    }
    o.len = 0;
}

fn close<D: embedded_sdmmc::BlockDevice, T: TimeSource>(mgr: &VolumeManager<D, T, 2, 2, 1>, mut o: Open) {
    write_out(mgr, &mut o);
    if let Err(e) = mgr.close_file(o.file) {
        log::warn!("SD card: close failed ({:?})", e);
    }
}
