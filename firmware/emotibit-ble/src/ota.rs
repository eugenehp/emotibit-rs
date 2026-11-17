//! Firmware updates over Bluetooth (`emotibit firmware ota`).
//!
//! The host writes to the update characteristic. Data may go without response, a window at a
//! time: the window's last write is with response, and as writes are handled in order, its
//! response comes once the board has taken the whole window. While the flash task is busy the
//! window waits in trouble-host's queue of 8 incoming ATT PDUs, which drops what does not fit,
//! so a window is at most 6 writes. A dropped write would fail the update (`data out of order`),
//! not corrupt it.
//!
//! | write | meaning |
//! |---|---|
//! | `B` size (u32 LE) SHA-256 (32 bytes) | begin an image of `size` bytes |
//! | `Z` size (u32 LE) stream size (u32 LE) SHA-256 (32 bytes) | begin a compressed image |
//! | `D` offset (u32 LE) bytes | the next bytes (of the image, or of the compressed stream), in order |
//! | `E` | the image is complete |
//! | `A` | abort |
//!
//! A compressed image is one frame per 4 KB sector: a u16 (LE) length, then that many bytes of
//! raw deflate (`miniz_oxide`), or of the sector as it is if the top bit of the length is set.
//! Sectors compressed on their own need no 32 KB history: each one decompresses straight into
//! the sector buffer. That sends about a third less than the image itself.
//!
//! The image goes into the app slot the board is not running (`partitions.csv`), a 4 KB sector at
//! a time. The SHA-256 is the uncompressed image's. At the end the board checks the SHA-256 of what is in flash, that the image is an
//! emotibit-ble one, and the image's own hash, then has the bootloader start that slot and
//! restarts. Until then the running firmware is untouched, so a failed or interrupted update
//! changes nothing. Answers are `OT` lines on the events characteristic: `OT,ready,<size>`,
//! `OT,progress,<bytes>` (every 64 KB), `OT,done`, or `OT,error,<reason>`.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as M;
use embassy_sync::channel::Channel;
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{self, AppPartitionSubType, PartitionType, PARTITION_TABLE_MAX_LEN};
use esp_storage::FlashStorage;
use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;
use sha2::{Digest, Sha256};

use crate::control::emit;
use crate::stock::Fixed;

/// Largest write to the update characteristic: the ATT maximum.
pub const MAX_WRITE: usize = 512;
const SECTOR: usize = 4096;
/// A frame's length has this bit set when the sector is stored as it is.
const STORED: u16 = 0x8000;
/// The project name in emotibit-ble's app descriptor.
const PROJECT: &[u8] = b"emotibit-ble";

enum Msg<'a> {
    /// `wire`: the bytes that follow, `size` without compression.
    Begin { size: u32, wire: Option<u32>, sha: [u8; 32] },
    Data { offset: u32, bytes: &'a [u8] },
    End,
    Abort,
}

/// Writes to the update characteristic, for the task that owns the flash. On the heap: a write
/// copied through the stack a few times overflowed it.
pub static IN: Channel<M, Vec<u8>, 2> = Channel::new();
/// An update is under way: the sensor stream pauses and the link runs at its fastest.
pub static ACTIVE: AtomicBool = AtomicBool::new(false);

fn parse(b: &[u8]) -> Option<Msg<'_>> {
    let u32_at = |i: usize| b.get(i..i + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    match b.first()? {
        b'B' if b.len() == 37 => Some(Msg::Begin { size: u32_at(1)?, wire: None, sha: b[5..37].try_into().ok()? }),
        b'Z' if b.len() == 41 => Some(Msg::Begin { size: u32_at(1)?, wire: Some(u32_at(5)?), sha: b[9..41].try_into().ok()? }),
        b'D' if b.len() > 5 && b.len() <= MAX_WRITE => Some(Msg::Data { offset: u32_at(1)?, bytes: &b[5..] }),
        b'E' => Some(Msg::End),
        b'A' => Some(Msg::Abort),
        _ => None,
    }
}

/// An update in progress.
struct Update {
    slot: AppPartitionSubType,
    /// Flash address of the slot.
    base: u32,
    /// The image's size.
    size: u32,
    /// The bytes to receive: the image, or its compressed stream.
    wire: u32,
    sha: [u8; 32],
    /// Bytes received so far.
    received: u32,
    /// A compressed image's frame being received.
    frames: Option<Frames>,
    /// Bytes already in flash (whole sectors).
    flushed: u32,
    started: embassy_time::Instant,
    /// Time spent erasing and writing flash, for the summary.
    flash_time: embassy_time::Duration,
    /// The sector being filled (on the heap, only while an update runs).
    sector: Vec<u8>,
    fill: usize,
}

/// The frame being received, and the decompressor (on the heap, about 11 KB).
struct Frames {
    decomp: Box<DecompressorOxide>,
    /// The length field, as its bytes arrive.
    head: [u8; 2],
    head_len: usize,
    buf: Vec<u8>,
}

impl Frames {
    fn new() -> Option<Frames> {
        let layout = core::alloc::Layout::new::<DecompressorOxide>();
        // Allocated zeroed, not built and moved: it would not fit on the stack.
        // SAFETY: the layout is the type's; its fields are integers, arrays of them, and `State`,
        // whose `Start` is 0, so all zeroes is a valid initial decompressor.
        let p = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut DecompressorOxide;
        if p.is_null() {
            return None;
        }
        let decomp = unsafe { Box::from_raw(p) };
        let mut buf = Vec::new();
        buf.try_reserve_exact(SECTOR).ok()?;
        Some(Frames { decomp, head: [0; 2], head_len: 0, buf })
    }

    /// The frame's length and whether it is stored, once its length field is in.
    fn len(&self) -> Option<(usize, bool)> {
        (self.head_len == 2).then(|| {
            let v = u16::from_le_bytes(self.head);
            ((v & !STORED) as usize, v & STORED != 0)
        })
    }
}

#[derive(Default)]
pub struct Updater {
    update: Option<Update>,
}

/// A buffer for the partition table, on the heap: static RAM and the stack are short.
fn table_buf() -> Box<[u8; PARTITION_TABLE_MAX_LEN]> {
    vec![0u8; PARTITION_TABLE_MAX_LEN].into_boxed_slice().try_into().unwrap()
}

fn num(n: u32) -> Fixed<12> {
    let mut f = Fixed::new();
    let _ = write!(f, "{n}");
    f
}

fn fail(reason: &str) {
    log::warn!("update: {reason}");
    emit("OT", &["error", reason], true);
}

impl Updater {
    /// Handles one write; true when the new firmware is ready and the board should restart.
    pub fn handle(&mut self, flash: &mut FlashStorage<'_>, write: &[u8]) -> bool {
        let Some(msg) = parse(write) else {
            fail("malformed write");
            return false;
        };
        match msg {
            Msg::Begin { size, wire, sha } => {
                self.update = None;
                let frames = match wire {
                    Some(_) => match Frames::new() {
                        Some(f) => Some(f),
                        None => {
                            fail("not enough memory to decompress");
                            return false;
                        }
                    },
                    None => None,
                };
                match begin(flash, size) {
                    Ok((slot, base)) => {
                        log::info!(
                            "update: {} bytes ({} sent) into {:?} at 0x{:x}, {} bytes of heap free",
                            size,
                            wire.unwrap_or(size),
                            slot,
                            base,
                            esp_alloc::HEAP.free()
                        );
                        self.update = Some(Update {
                            slot,
                            base,
                            size,
                            wire: wire.unwrap_or(size),
                            sha,
                            received: 0,
                            frames,
                            flushed: 0,
                            started: embassy_time::Instant::now(),
                            flash_time: embassy_time::Duration::from_ticks(0),
                            sector: vec![0xFF; SECTOR],
                            fill: 0,
                        });
                        ACTIVE.store(true, Relaxed);
                        emit("OT", &["ready", num(size).as_str()], true);
                    }
                    Err(e) => fail(e),
                }
                false
            }
            Msg::Data { offset, bytes } => {
                let Some(u) = self.update.as_mut() else {
                    fail("no update begun");
                    return false;
                };
                if let Err(e) = u.data(flash, offset, bytes) {
                    self.abort();
                    fail(e);
                }
                false
            }
            Msg::End => {
                let Some(u) = self.update.as_mut() else {
                    fail("no update begun");
                    return false;
                };
                let result = u.finish(flash);
                let slot = u.slot;
                self.abort();
                match result {
                    Ok(()) => {
                        log::info!("update: {:?} starts next", slot);
                        emit("OT", &["done"], true);
                        true
                    }
                    Err(e) => {
                        fail(e);
                        false
                    }
                }
            }
            Msg::Abort => {
                if self.update.is_some() {
                    log::info!("update: aborted");
                }
                self.abort();
                false
            }
        }
    }

    fn abort(&mut self) {
        self.update = None;
        ACTIVE.store(false, Relaxed);
    }
}

/// The slot to write (the one not running) and its flash address.
fn begin(flash: &mut FlashStorage<'_>, size: u32) -> Result<(AppPartitionSubType, u32), &'static str> {
    let mut buf = table_buf();
    let pt = partitions::read_partition_table(flash, &mut buf[..]).map_err(|_| "cannot read the partition table")?;
    let slot = |s| pt.find_partition(PartitionType::App(s)).ok().flatten();
    let (Some(ota0), Some(ota1)) = (slot(AppPartitionSubType::Ota0), slot(AppPartitionSubType::Ota1)) else {
        return Err("no update slots: install this firmware over USB once (emotibit firmware ble)");
    };
    if pt.find_partition(PartitionType::Data(partitions::DataPartitionSubType::Ota)).ok().flatten().is_none() {
        return Err("no otadata partition: install this firmware over USB once (emotibit firmware ble)");
    }
    let running = pt.booted_partition().ok().flatten().map(|p| p.partition_type());
    let target = if running == Some(PartitionType::App(AppPartitionSubType::Ota0)) { ota1 } else { ota0 };
    if running == Some(target.partition_type()) {
        return Err("cannot tell which slot is running");
    }
    if size == 0 || size > target.len() {
        return Err("the image does not fit the update slot");
    }
    let PartitionType::App(sub) = target.partition_type() else { unreachable!() };
    Ok((sub, target.offset()))
}

impl Update {
    fn data(&mut self, flash: &mut FlashStorage<'_>, offset: u32, bytes: &[u8]) -> Result<(), &'static str> {
        if offset != self.received {
            return Err("data out of order");
        }
        if self.received + bytes.len() as u32 > self.wire {
            return Err("more data than announced");
        }
        if let Some(mut f) = self.frames.take() {
            let result = self.frame_data(flash, &mut f, bytes);
            self.frames = Some(f);
            result?;
        } else {
            if offset == 0 && bytes.first() != Some(&0xE9) {
                return Err("not an ESP32 app image");
            }
            let mut rest = bytes;
            while !rest.is_empty() {
                let n = rest.len().min(SECTOR - self.fill);
                self.sector[self.fill..self.fill + n].copy_from_slice(&rest[..n]);
                self.fill += n;
                rest = &rest[n..];
                if self.fill == SECTOR {
                    self.flush(flash)?;
                }
            }
        }
        let before = self.received;
        self.received += bytes.len() as u32;
        if self.received / 0x10000 != before / 0x10000 {
            emit("OT", &["progress", num(self.received).as_str()], true);
        }
        Ok(())
    }

    /// Compressed data: frames as they complete, each decompressed into a sector and written.
    fn frame_data(&mut self, flash: &mut FlashStorage<'_>, f: &mut Frames, mut rest: &[u8]) -> Result<(), &'static str> {
        while !rest.is_empty() {
            let Some((len, stored)) = f.len() else {
                f.head[f.head_len] = rest[0];
                f.head_len += 1;
                rest = &rest[1..];
                continue;
            };
            let n = rest.len().min(len - f.buf.len());
            f.buf.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if f.buf.len() == len {
                // The sector this frame holds: whole, or the image's last bytes.
                let expected = self.size.saturating_sub(self.flushed).min(SECTOR as u32) as usize;
                if expected == 0 || len > SECTOR {
                    return Err("a malformed frame");
                }
                if stored {
                    if len != expected {
                        return Err("a malformed frame");
                    }
                    self.sector[..len].copy_from_slice(&f.buf);
                } else {
                    f.decomp.init();
                    let flags = inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
                    let (status, _, out) = decompress(&mut f.decomp, &f.buf, &mut self.sector[..expected], 0, flags);
                    if status != TINFLStatus::Done || out != expected {
                        return Err("a frame did not decompress");
                    }
                }
                if self.flushed == 0 && self.sector[0] != 0xE9 {
                    return Err("not an ESP32 app image");
                }
                self.fill = expected;
                self.flush(flash)?;
                f.buf.clear();
                f.head_len = 0;
            }
        }
        Ok(())
    }

    /// Writes the sector being filled (erased first; the unfilled rest stays 0xFF).
    fn flush(&mut self, flash: &mut FlashStorage<'_>) -> Result<(), &'static str> {
        if self.fill == 0 {
            return Ok(());
        }
        let at = self.base + self.flushed;
        let t0 = embassy_time::Instant::now();
        flash.erase(at, at + SECTOR as u32).map_err(|_| "flash erase failed")?;
        let len = self.fill.next_multiple_of(4);
        flash.write_nor(at, &self.sector[..len]).map_err(|_| "flash write failed")?;
        self.flash_time += t0.elapsed();
        self.sector.fill(0xFF);
        self.fill = 0;
        self.flushed += SECTOR as u32;
        Ok(())
    }

    fn finish(&mut self, flash: &mut FlashStorage<'_>) -> Result<(), &'static str> {
        if self.received != self.wire {
            return Err("the image is incomplete");
        }
        self.flush(flash)?;
        if self.flushed < self.size || self.frames.as_ref().is_some_and(|f| f.head_len != 0) {
            return Err("the compressed image is incomplete");
        }
        log::info!(
            "update: {} bytes in {} ms, {} ms of it erasing and writing flash",
            self.size,
            self.started.elapsed().as_millis(),
            self.flash_time.as_millis()
        );
        // What is in flash is what the host sent.
        let mut hash = Sha256::new();
        let mut chunk = [0u8; 512];
        let mut done = 0u32;
        while done < self.size {
            let n = (self.size - done).min(chunk.len() as u32) as usize;
            flash.read(self.base + done, &mut chunk[..n]).map_err(|_| "flash read failed")?;
            if done == 0 && !is_emotibit_ble(&chunk[..n]) {
                return Err("not an emotibit-ble image");
            }
            hash.update(&chunk[..n]);
            done += n as u32;
        }
        if hash.finalize().as_slice() != self.sha {
            return Err("SHA-256 mismatch: the image in flash differs from the one sent");
        }
        // The image's own structure and appended hash, as the bootloader will check them.
        let mut buf = table_buf();
        let pt = partitions::read_partition_table(flash, &mut buf[..]).map_err(|_| "cannot read the partition table")?;
        let entry = pt.find_partition(PartitionType::App(self.slot)).ok().flatten().ok_or("update slot gone")?;
        entry.sha256(flash).map_err(|_| "the image's own hash does not match")?;
        // Start it next.
        let mut buf = table_buf();
        let mut updater = OtaUpdater::new(flash, &mut buf).map_err(|_| "no update slots")?;
        let mut ota = updater.ota_data().map_err(|_| "cannot read otadata")?;
        ota.set_current_app_partition(self.slot).map_err(|_| "cannot select the new slot")?;
        ota.set_current_ota_state(OtaImageState::New).map_err(|_| "cannot mark the new slot")?;
        Ok(())
    }
}

/// An app image whose descriptor (after the image and first segment headers) names emotibit-ble.
fn is_emotibit_ble(image: &[u8]) -> bool {
    let Some(d) = image.get(32..32 + 80) else { return false };
    d[..4] == 0xABCD_5432u32.to_le_bytes() && d[48..48 + PROJECT.len()] == *PROJECT && d[48 + PROJECT.len()] == 0
}

/// At boot: a slot started for the first time after an update is marked as working (with a
/// bootloader that rolls back, an unmarked one would be abandoned on the next start).
pub fn confirm(flash: &mut FlashStorage<'_>) {
    let mut buf = table_buf();
    let Ok(mut updater) = OtaUpdater::new(flash, &mut buf) else {
        log::info!("firmware: no update slots (an older partition table; install over USB once for updates over Bluetooth)");
        return;
    };
    let slot = updater.selected_partition();
    match updater.current_ota_state() {
        Ok(OtaImageState::New | OtaImageState::PendingVerify) => match updater.set_current_ota_state(OtaImageState::Valid) {
            Ok(()) => log::info!("firmware: updated, running {:?}", slot),
            Err(e) => log::warn!("firmware: cannot mark the update as working ({:?})", e),
        },
        _ => log::info!("firmware: running {:?}", slot),
    }
}
