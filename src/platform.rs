//! Spawning, timers and the clock, chosen per target (the same arrangement as `muse-rs`).
//!
//! Natively all of it is tokio's and `std::time`'s. In a browser none of it is: tokio's timer is
//! not implemented for `wasm32-unknown-unknown`, there are no threads, and `std::time` panics.
//! What a page has is an event loop, which `webbluetooth-wasm` already drives: its
//! `exec::spawn` polls tasks, and its `SET_TIMEOUT` operation is `setTimeout`. The clock comes
//! from the page through two imports in the `emotibit` namespace, `eb_now_ms` (`Date.now()`)
//! and `eb_tz_offset_min` (`new Date().getTimezoneOffset()`); `examples/web/app.js` supplies
//! them.

use std::future::Future;
use std::time::Duration;

/// A deadline passed before the future finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

// ── Native ─────────────────────────────────────────────────────────────────────

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn(task: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(task);
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    tokio::time::timeout(duration, future).await.map_err(|_| Elapsed)
}

/// Milliseconds since the Unix epoch.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

/// `2025-11-05_17-37-57-123456`: local time in the format EmotiBit uses in `TL`/`RB`/`UN`.
#[cfg(not(target_arch = "wasm32"))]
pub fn local_timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S-%6f").to_string()
}

// ── Browser ────────────────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "emotibit")]
unsafe extern "C" {
    fn eb_now_ms() -> f64;
    fn eb_tz_offset_min() -> f64;
}

/// Runs `task` on the page's event loop. No `Send` bound: a page has one thread, and the Web
/// Bluetooth objects these tasks hold are not `Send`.
#[cfg(target_arch = "wasm32")]
pub fn spawn(task: impl Future<Output = ()> + 'static) {
    webbluetooth_wasm::exec::spawn(task);
}

#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: Duration) {
    use webbluetooth_wasm::codec::Writer;
    use webbluetooth_wasm::host::{call, Op};
    let ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
    let mut message = Writer::new();
    message.u32(ms);
    let _ = call(Op::SetTimeout, &message.finish()).await;
}

#[cfg(target_arch = "wasm32")]
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    use webbluetooth::future::{select, Either};
    let future = std::pin::pin!(future);
    let deadline = std::pin::pin!(sleep(duration));
    match select(future, deadline).await {
        Either::Left((value, _)) => Ok(value),
        Either::Right(((), _)) => Err(Elapsed),
    }
}

#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> f64 {
    unsafe { eb_now_ms() }
}

#[cfg(target_arch = "wasm32")]
pub fn local_timestamp() -> String {
    // getTimezoneOffset is UTC minus local, in minutes.
    let local_ms = now_ms() - unsafe { eb_tz_offset_min() } * 60_000.0;
    format_local(local_ms)
}

/// Formats milliseconds since 1970 (already shifted to local time) as `YYYY-MM-DD_HH-MM-SS-uuuuuu`.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn format_local(local_ms: f64) -> String {
    let total_ms = local_ms.max(0.0) as i64;
    let (days, ms_of_day) = (total_ms.div_euclid(86_400_000), total_ms.rem_euclid(86_400_000));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s, ms) = (ms_of_day / 3_600_000, ms_of_day / 60_000 % 60, ms_of_day / 1000 % 60, ms_of_day % 1000);
    format!("{year:04}-{month:02}-{day:02}_{h:02}-{m:02}-{s:02}-{:06}", ms * 1000)
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_local_time_like_emotibit() {
        // 2026-11-07 20:23:15.026 (no time zone shift here).
        assert_eq!(super::format_local(1_794_082_995_026.0), "2026-11-07_20-23-15-026000");
        assert_eq!(super::format_local(0.0), "1970-01-01_00-00-00-000000");
        assert_eq!(super::format_local(951_782_400_000.0), "2000-02-29_00-00-00-000000");
    }
}
