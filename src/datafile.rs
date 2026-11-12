//! Raw recordings and the DataParser-compatible converter.
//!
//! A *raw file* is one packet per line in exactly the format the device writes to its SD
//! card (see [`crate::protocol`]). [`spawn_raw_logger`] writes one from a live connection;
//! [`parse_file`] turns any raw file — a live log or a file copied off the SD card — into
//! one CSV per signal with a `LocalTimestamp` column, like the official EmotiBit DataParser:
//!
//! * pass 1 collects `RD`/`TL`/`AK` time-sync exchanges and derives a device→computer clock
//!   mapping ([`crate::timesync`]);
//! * pass 2 writes `<base>_<TAG>.csv` for each tag. Sensor packets get one row per sample,
//!   timestamps interpolated between packets; everything else gets one row per packet.
//!
//! Only the local-time domain is produced. The DataParser can also emit LSL-clock columns when
//! `LC`/`LM` cross-time packets are present; those are not converted here.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDateTime, TimeZone};
use log::warn;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::task::JoinHandle;

use crate::emotibit_client::EmotiBitHandle;
use crate::protocol::{Packet, tag};
use crate::timesync::{Anchors, TimeSync};
use crate::types::channel;

/// Convert the host's `TL` string (`2026-11-05_17-54-49-677532`, local time, last field the
/// fractional second) to epoch seconds.
pub fn parse_local_timestamp(s: &str) -> Option<f64> {
    let (head, frac) = s.trim().rsplit_once('-')?;
    let naive = NaiveDateTime::parse_from_str(head, "%Y-%m-%d_%H-%M-%S").ok()?;
    let secs = Local.from_local_datetime(&naive).single()?.timestamp() as f64;
    let digits = frac.len() as i32;
    let fraction = frac.parse::<f64>().ok()? / 10f64.powi(digits);
    Some(secs + fraction)
}

// ── Live raw logging ──────────────────────────────────────────────────────────

/// Write every packet from `handle` to `path`, one per line, until the connection ends.
/// Await the returned handle after disconnecting to be sure the file is flushed.
///
/// The handle keeps the packet channel alive itself, so the logger stops on the connection's
/// shutdown signal (after writing whatever is already queued), not on the channel closing.
pub async fn spawn_raw_logger(handle: &EmotiBitHandle, path: PathBuf) -> Result<JoinHandle<Result<u64>>> {
    let mut rx = handle.subscribe_raw();
    let handle = handle.clone();
    let file = tokio::fs::File::create(&path)
        .await
        .with_context(|| format!("cannot create {}", path.display()))?;
    Ok(tokio::spawn(async move {
        let mut out = tokio::io::BufWriter::new(file);
        let mut lines = 0u64;
        let mut closed = false;
        loop {
            let next = if closed {
                // Connection over: drain what is queued, then stop.
                match rx.try_recv() {
                    Ok(l) => Ok(l),
                    Err(TryRecvError::Lagged(n)) => Err(RecvError::Lagged(n)),
                    Err(_) => break,
                }
            } else {
                tokio::select! {
                    r = rx.recv() => r,
                    _ = handle.closed() => { closed = true; continue }
                }
            };
            match next {
                Ok(line) => {
                    out.write_all(line.as_bytes()).await?;
                    out.write_all(b"\n").await?;
                    lines += 1;
                }
                Err(RecvError::Lagged(n)) => warn!("raw logger fell behind, {n} packets not recorded"),
                Err(RecvError::Closed) => break,
            }
        }
        out.flush().await?;
        Ok(lines)
    }))
}

// ── Converter ─────────────────────────────────────────────────────────────────

/// What [`parse_file`] did.
#[derive(Debug, Default)]
pub struct ParseReport {
    pub packets: u64,
    pub malformed: u64,
    pub sync_points: usize,
    /// False if there were no sync points, so `LocalTimestamp` is relative, not wall-clock.
    pub anchored: bool,
    /// Rows written per tag.
    pub rows: BTreeMap<String, u64>,
    pub files: Vec<PathBuf>,
}

const COMMON_HEADERS: &str = "EmotiBitTimestamp,PacketNumber,DataLength,TypeTag,ProtocolVersion,DataReliability";

struct TagOut {
    w: BufWriter<File>,
    prev_ts: Option<u32>,
}

/// Convert a raw file into per-tag CSVs next to it (or in `out_dir`).
pub fn parse_file(input: &Path, out_dir: Option<&Path>) -> Result<ParseReport> {
    let base = input
        .file_stem()
        .and_then(|s| s.to_str())
        .context("input has no file name")?
        .to_string();
    let dir = match out_dir {
        Some(d) => d.to_path_buf(),
        None => input.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    std::fs::create_dir_all(&dir)?;

    // Pass 1: read everything, collect sync points.
    let mut report = ParseReport::default();
    let mut packets: Vec<Packet> = Vec::new();
    let mut sync = TimeSync::new();
    let bytes = std::fs::read(input).with_context(|| format!("cannot open {}", input.display()))?;
    // A binary file from the BLE firmware's SD card (`.EBB`) becomes the same stock lines.
    let lines: Vec<String> = match crate::ble::binary_to_lines(&bytes) {
        Some(lines) => lines,
        None => String::from_utf8_lossy(&bytes).lines().map(str::to_string).collect(),
    };
    for line in &lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match Packet::parse(line) {
            Ok(p) => {
                sync.observe(&p, parse_local_timestamp);
                packets.push(p);
            }
            Err(_) => report.malformed += 1,
        }
    }
    report.packets = packets.len() as u64;
    if packets.is_empty() {
        bail!("no packets found in {}", input.display());
    }
    report.sync_points = sync.points().len();
    let anchors = sync.anchors().expect("packets were observed");
    report.anchored = anchors.anchored;
    if !anchors.anchored {
        warn!("no time syncs in the file: LocalTimestamp is relative to the start, not wall-clock");
    }

    // Pass 2: one CSV per tag.
    let mut outs: BTreeMap<String, TagOut> = BTreeMap::new();
    for p in &packets {
        let sensor = channel(&p.tag).is_some();
        let out = match outs.get_mut(&p.tag) {
            Some(o) => o,
            None => {
                let path = dir.join(format!("{base}_{}.csv", p.tag));
                let mut w = BufWriter::new(File::create(&path)?);
                writeln!(w, "LocalTimestamp,{COMMON_HEADERS},{}", p.tag)?;
                report.files.push(path);
                outs.entry(p.tag.clone()).or_insert(TagOut { w, prev_ts: None })
            }
        };
        let prev = out.prev_ts.unwrap_or(p.timestamp);
        out.prev_ts = Some(p.timestamp);
        let prefix = format!("{},{},{},{},{}", p.number, p.data_length, p.tag, p.protocol_version, p.reliability);
        let rows = report.rows.entry(p.tag.clone()).or_default();

        if sensor {
            let n = p.payload.len().max(1);
            for (i, v) in p.payload.iter().enumerate() {
                let ts = prev as f64 + (i + 1) as f64 / n as f64 * (p.timestamp as f64 - prev as f64);
                writeln!(out.w, "{:.6},{:.3},{prefix},{v}", anchors.to_local(ts), ts)?;
                *rows += 1;
            }
        } else {
            // A user note carries the host's own local time; everything else is mapped from the device clock.
            let local = if p.tag == tag::USER_NOTE {
                p.payload.first().and_then(|s| parse_local_timestamp(s)).unwrap_or_else(|| anchors.to_local(p.timestamp as f64))
            } else {
                anchors.to_local(p.timestamp as f64)
            };
            // Payload may contain free text; quote it so commas cannot shift columns.
            let payload = p.payload.join(";");
            writeln!(out.w, "{:.6},{}.000,{prefix},\"{}\"", local, p.timestamp, payload.replace('"', "'"))?;
            *rows += 1;
        }
    }
    for o in outs.values_mut() {
        o.w.flush()?;
    }

    // Time-sync map and the individual exchanges.
    let map_path = dir.join(format!("{base}_timeSyncMap.csv"));
    write_sync_map(&map_path, &anchors, &sync, &packets)?;
    report.files.push(map_path);
    let sync_path = dir.join(format!("{base}_timeSyncs.csv"));
    let mut w = BufWriter::new(File::create(&sync_path)?);
    writeln!(w, "RD,TS_received,TS_sent,AK,RoundTrip")?;
    for p in sync.points() {
        writeln!(w, "{},{},{:.6},{},{}", p.rd, p.received, p.sent_local, p.ack, p.round_trip)?;
    }
    w.flush()?;
    report.files.push(sync_path);
    Ok(report)
}

fn write_sync_map(path: &Path, a: &Anchors, sync: &TimeSync, packets: &[Packet]) -> Result<()> {
    let (lo, hi) = packets
        .iter()
        .fold((u32::MAX, 0u32), |(lo, hi), p| (lo.min(p.timestamp), hi.max(p.timestamp)));
    let mut w = BufWriter::new(File::create(path)?);
    writeln!(w, "TE0,TE1,TL0,TL1,TimeSyncsReceived,EmotiBitStartTime,EmotiBitEndTime,DataParserVersion")?;
    writeln!(
        w,
        "{:.6},{:.6},{:.6},{:.6},{},{lo},{hi},emotibit-{}",
        a.e0,
        a.e1,
        a.c0,
        a.c1,
        sync.points().len(),
        env!("CARGO_PKG_VERSION")
    )?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_timestamp_string() {
        let a = parse_local_timestamp("2026-11-05_17-54-49-500000").unwrap();
        let b = parse_local_timestamp("2026-11-05_17-54-49-000000").unwrap();
        assert!((a - b - 0.5).abs() < 1e-9);
        assert!(parse_local_timestamp("garbage").is_none());
    }

    #[test]
    fn converts_raw_file_to_per_tag_csvs() {
        let dir = std::env::temp_dir().join(format!("emotibit-parse-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("rec.csv");

        // Device clock t -> host epoch (1.7e9 + t/1000). Two sync exchanges bound the data.
        let host = |dev_ms: f64| {
            let secs = 1.7e9 + dev_ms / 1000.0;
            let local = Local.timestamp_opt(secs.floor() as i64, 0).single().unwrap();
            format!("{}-{:06}", local.format("%Y-%m-%d_%H-%M-%S"), ((secs.fract()) * 1e6).round() as i64)
        };
        let mut raw = String::new();
        for (n, rd, rtt) in [(1u32, 1_000u32, 20u32), (50, 9_000, 30)] {
            let recv = rd + rtt;
            raw += &format!("{rd},{n},2,RD,1,100,TL,TU\n");
            raw += &format!("{recv},{},1,TL,1,100,{}\n", n + 1, host(rd as f64 + rtt as f64 / 2.0));
            raw += &format!("{},{},2,AK,1,100,{n},RD\n", recv + 1, n + 2);
        }
        raw += "4000,10,3,AX,1,100,1.0,2.0,3.0\n4100,11,3,AX,1,100,4.0,5.0,6.0\nbroken line\n";
        raw += "4200,12,4,EM,1,100,RS,RE,PS,MN\n";
        std::fs::write(&input, raw).unwrap();

        let report = parse_file(&input, None).unwrap();
        assert_eq!(report.malformed, 1);
        assert_eq!(report.sync_points, 2);
        assert!(report.anchored);
        assert_eq!(report.rows["AX"], 6);

        let ax = std::fs::read_to_string(dir.join("rec_AX.csv")).unwrap();
        let rows: Vec<&str> = ax.lines().collect();
        assert!(rows[0].starts_with("LocalTimestamp,EmotiBitTimestamp"));
        // Last sample of the second packet is at device time 4100 ms -> host 1.7e9 + 4.1 s.
        let last: Vec<&str> = rows.last().unwrap().split(',').collect();
        let local: f64 = last[0].parse().unwrap();
        assert!((local - (1.7e9 + 4.1)).abs() < 0.05, "local {local}");
        assert!(dir.join("rec_timeSyncMap.csv").exists());
        assert!(dir.join("rec_EM.csv").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
