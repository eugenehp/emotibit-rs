//! The browser version of `emotibit`: the same library the CLI and the TUI drive, compiled
//! to `wasm32-unknown-unknown`, talking to a board running `firmware/emotibit-ble` over the
//! browser's own Web Bluetooth (through `webbluetooth`, as in `muse-rs`).
//!
//! Build it with `examples/web/build.sh`; `examples/web/README.md` has the details.
//!
//! # The boundary
//!
//! No `wasm-bindgen`: plain numbers and (pointer, length) strings, like `webbluetooth-wasm`.
//!
//! - **Exports** (the page calls them): [`eb_connect`], [`eb_disconnect`], [`eb_command`]
//!   (record, note, power, settings, self-test, SD listing), [`eb_sd_get`] (download a file),
//!   and [`eb_alloc`] / [`eb_free`] for the strings the page passes in.
//! - **Imports** (the `emotibit` namespace, from `app.js`): series declarations and samples
//!   for the plots, the status as JSON, every other packet line, downloaded files, logs, and
//!   the clock (`eb_now_ms`, `eb_tz_offset_min`, used by `emotibit::platform`).
//!
//! Native builds of this file are empty, as in `muse-rs`.

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, HashMap};
    use std::time::Duration;

    use emotibit::ble;
    use emotibit::handle::EmotiBitHandle;
    use emotibit::platform;
    use emotibit::protocol::Packet;
    use emotibit::types::EmotiBitEvent;

    #[link(wasm_import_module = "emotibit")]
    unsafe extern "C" {
        /// Connection state and errors, in words for the page.
        fn eb_status(ptr: *const u8, len: usize);
        fn eb_log(level: u32, ptr: *const u8, len: usize);
        /// The board's name once connected (empty when the link ends).
        fn eb_device(ptr: *const u8, len: usize);
        /// A new plotted series: index, stock tag, display name, unit.
        fn eb_series(index: u32, tag: *const u8, tag_len: usize, name: *const u8, name_len: usize, unit: *const u8, unit_len: usize);
        /// One sample: series index, host time (ms), value.
        fn eb_sample(index: u32, t_ms: f64, value: f64);
        /// Status as JSON: battery, recording, file, power mode, counters, link statistics.
        fn eb_state(ptr: *const u8, len: usize);
        /// Any other packet from the board, as a stock line (settings, self-test, file lists…).
        fn eb_message(ptr: *const u8, len: usize);
        /// A downloaded SD file: name, bytes.
        fn eb_file(name: *const u8, name_len: usize, ptr: *const u8, len: usize);
        /// Download progress: bytes so far, total.
        fn eb_progress(have: f64, total: f64);
        /// 1 while connected or connecting, 0 after.
        fn eb_active(active: u32);
    }

    fn status(s: &str) {
        unsafe { eb_status(s.as_ptr(), s.len()) }
    }

    struct PageLog;
    impl log::Log for PageLog {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, r: &log::Record<'_>) {
            let line = format!("{}: {}", r.target(), r.args());
            unsafe { eb_log(r.level() as u32, line.as_ptr(), line.len()) }
        }
        fn flush(&self) {}
    }
    static PAGE_LOG: PageLog = PageLog;

    /// A file being downloaded: chunks by offset; missing ranges are asked for again.
    struct Download {
        name: String,
        size: Option<usize>,
        chunks: BTreeMap<usize, Vec<u8>>,
        rounds: u32,
    }

    thread_local! {
        static HANDLE: RefCell<Option<EmotiBitHandle>> = const { RefCell::new(None) };
        static DOWNLOAD: RefCell<Option<Download>> = const { RefCell::new(None) };
    }

    fn handle() -> Option<EmotiBitHandle> {
        HANDLE.with(|h| h.borrow().clone())
    }

    // ── Exports ───────────────────────────────────────────────────────────────

    /// Room for a string the page passes in.
    #[unsafe(no_mangle)]
    pub extern "C" fn eb_alloc(len: usize) -> *mut u8 {
        let mut v = Vec::<u8>::with_capacity(len.max(1));
        let p = v.as_mut_ptr();
        std::mem::forget(v);
        p
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn eb_free(ptr: *mut u8, len: usize) {
        drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1)) });
    }

    fn take_str(ptr: *mut u8, len: usize) -> String {
        let s = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }).into_owned();
        unsafe { eb_free(ptr, len) };
        s
    }

    /// Opens the browser's device chooser and streams from the board picked. Call it from a
    /// click: `requestDevice` needs a user gesture, and the chooser opens before this returns.
    /// `compact`: 1 for the compact wire format, 0 for raw records.
    #[unsafe(no_mangle)]
    pub extern "C" fn eb_connect(compact: u32) {
        let _ = log::set_logger(&PAGE_LOG);
        log::set_max_level(log::LevelFilter::Info);
        platform::spawn(async move {
            unsafe { eb_active(1) };
            if let Err(e) = stream(compact == 1).await {
                status(&format!("{e:#}"));
            }
            HANDLE.with(|h| *h.borrow_mut() = None);
            unsafe { eb_device(std::ptr::null(), 0) };
            unsafe { eb_active(0) };
        });
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn eb_disconnect() {
        if let Some(h) = handle() {
            platform::spawn(async move {
                let _ = h.disconnect().await;
            });
        }
    }

    /// A command, as tab-separated fields: a tag and its payload. For `RB` (record), `RE`, `UN`
    /// (note) and the power tags (`MN`, `ML`, `MM`, `MO`, `MH`) the host's local time is put
    /// first, as the stock protocol has it; `RB` with `bin` records in the binary format.
    /// Anything else goes out as given: `CG`, `CS`…, `CR`, `ST`, `FL`.
    #[unsafe(no_mangle)]
    pub extern "C" fn eb_command(ptr: *mut u8, len: usize) {
        let text = take_str(ptr, len);
        let mut fields = text.split('\t').map(str::to_string);
        let Some(tag) = fields.next() else { return };
        let rest: Vec<String> = fields.map(|f| f.replace(',', ";")).collect();
        let Some(h) = handle() else {
            status("Not connected.");
            return;
        };
        let payload = match tag.as_str() {
            "RB" if rest.first().map(String::as_str) == Some("bin") => vec![platform::local_timestamp(), "FMT".into(), "BIN".into()],
            "RB" | "RE" | "MN" | "ML" | "MM" | "MO" | "MH" => vec![platform::local_timestamp()],
            "UN" => [vec![platform::local_timestamp()], rest].concat(),
            _ => rest,
        };
        platform::spawn(async move {
            if let Err(e) = h.send_control(&tag, &payload).await {
                status(&format!("{tag}: {e:#}"));
            }
        });
    }

    /// Downloads a file from the board's SD card; the page gets it through `eb_file`.
    #[unsafe(no_mangle)]
    pub extern "C" fn eb_sd_get(ptr: *mut u8, len: usize) {
        let name = take_str(ptr, len);
        let Some(h) = handle() else { return };
        DOWNLOAD.with(|d| *d.borrow_mut() = Some(Download { name: name.clone(), size: None, chunks: BTreeMap::new(), rounds: 0 }));
        platform::spawn(async move {
            let _ = h.send_control("FG", &[name, "0".into(), "0".into()]).await;
        });
    }

    // ── The stream ────────────────────────────────────────────────────────────

    /// Plotted series by stock tag, and the device-to-host clock offset.
    struct Plots {
        index: HashMap<&'static str, u32>,
        offset: Option<f64>,
    }

    impl Plots {
        fn sample(&mut self, r: &emotibit::types::SensorReading) {
            let next = self.index.len() as u32;
            let i = *self.index.entry(r.channel.tag).or_insert_with(|| {
                let c = r.channel;
                unsafe { eb_series(next, c.tag.as_ptr(), c.tag.len(), c.name.as_ptr(), c.name.len(), c.unit.as_ptr(), c.unit.len()) };
                next
            });
            // The board's own clock spaces the samples; the smallest offset maps it to the
            // page's clock (the fastest delivery).
            let t = if r.device_timestamp_ms > 0 {
                let o = r.received_at_ms - r.device_timestamp_ms as f64;
                let off = *self.offset.get_or_insert(o);
                if o < off {
                    self.offset = Some(o);
                }
                r.device_timestamp_ms as f64 + self.offset.unwrap_or(o)
            } else {
                r.received_at_ms
            };
            for v in &r.samples {
                unsafe { eb_sample(i, t, *v) };
            }
        }
    }

    async fn stream(compact: bool) -> anyhow::Result<()> {
        status("Choose the board in the browser's dialog…");
        let device = ble::request(None, Duration::from_secs(30)).await?;
        status("Connecting…");
        let (mut events, h) = ble::connect_device(device, compact).await?;
        HANDLE.with(|c| *c.borrow_mut() = Some(h.clone()));
        let name = h.device().id.clone();
        unsafe { eb_device(name.as_ptr(), name.len()) };
        status("Streaming.");
        let _ = h.send_control("CG", &[]).await;

        let mut plots = Plots { index: HashMap::new(), offset: None };
        let mut link: Vec<f64> = Vec::new();
        let mut last_state = 0.0;
        while let Some(ev) = events.recv().await {
            match ev {
                EmotiBitEvent::Sensor(r) => plots.sample(&r),
                EmotiBitEvent::Message(p) if p.tag == ble::LINK_TAG => link = p.values(),
                EmotiBitEvent::Message(p) => message(&h, p).await,
                EmotiBitEvent::Disconnected => break,
                _ => {}
            }
            // The status, about four times a second.
            let now = platform::now_ms();
            if now - last_state > 250.0 {
                last_state = now;
                let mut json = serde_json::to_value(h.status()).unwrap_or_default();
                json["link"] = serde_json::json!(link);
                json["compact"] = serde_json::json!(compact);
                let s = json.to_string();
                unsafe { eb_state(s.as_ptr(), s.len()) };
            }
        }
        status("Disconnected.");
        Ok(())
    }

    /// Passes a packet to the page, and runs a download along.
    async fn message(h: &EmotiBitHandle, p: Packet) {
        let line = p.to_line();
        if !matches!(p.tag.as_str(), "FD" | "FG" | "FE") {
            unsafe { eb_message(line.as_ptr(), line.len()) };
            return;
        }
        let Some(mut d) = DOWNLOAD.with(|d| d.borrow_mut().take()) else { return };
        let field = |i: usize| p.payload.get(i).map(String::as_str).unwrap_or("");
        match p.tag.as_str() {
            "FG" => d.size = field(1).parse().ok().or(d.size),
            "FD" => {
                if let (Ok(offset), Some(bytes)) = (field(0).parse::<usize>(), base64(field(1))) {
                    d.chunks.entry(offset).or_insert(bytes);
                }
                let have: usize = d.chunks.values().map(Vec::len).sum();
                unsafe { eb_progress(have as f64, d.size.unwrap_or(0) as f64) };
            }
            _ => {
                // FE: this request is done; ask for what is missing, or hand the file over.
                if field(1).parse::<usize>().is_err() {
                    status(&format!("{}: {}", d.name, field(1)));
                    return;
                }
                let total = d.size.unwrap_or(0);
                let mut at = 0;
                let mut missing = None;
                for (&o, c) in &d.chunks {
                    if o > at {
                        missing = Some((at, o - at));
                        break;
                    }
                    at = at.max(o + c.len());
                }
                match missing.or((at < total).then_some((at, total - at))) {
                    Some((offset, len)) if d.rounds < 20 => {
                        d.rounds += 1;
                        let _ = h.send_control("FG", &[d.name.clone(), offset.to_string(), len.to_string()]).await;
                    }
                    Some(_) => {
                        status(&format!("{}: download incomplete", d.name));
                        return;
                    }
                    None => {
                        let bytes: Vec<u8> = d.chunks.into_values().flatten().collect();
                        unsafe { eb_file(d.name.as_ptr(), d.name.len(), bytes.as_ptr(), bytes.len()) };
                        return;
                    }
                }
            }
        }
        DOWNLOAD.with(|slot| *slot.borrow_mut() = Some(d));
    }

    fn base64(s: &str) -> Option<Vec<u8>> {
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
        let b = s.trim().as_bytes();
        if !b.len().is_multiple_of(4) {
            return None;
        }
        let mut out = Vec::with_capacity(b.len() / 4 * 3);
        for q in b.chunks(4) {
            let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
            let mut v = 0u32;
            for &c in &q[..4 - pad] {
                v = v << 6 | val(c)?;
            }
            v <<= 6 * pad as u32;
            out.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8][..3 - pad]);
        }
        Some(out)
    }
}
