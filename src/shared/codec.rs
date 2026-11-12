//! Compact record codec shared by the firmware (encoder) and `emotibit` (decoder).
//!
//! Only `core` is used so the same file builds for the ESP32 and for the host. It lives in the
//! host crate (`src/shared/`, published with it) and the firmware pulls it in with `#[path]`, so
//! the two ends cannot drift apart.
//!
//! A raw record is 20 bytes: `tag, count, seq (u16), up to 4 x i32`. The compact form is:
//!
//! ```text
//! byte 0   bits 0-3 tag, bit 4 key frame, bits 5-7 sample count (0..=4)
//! byte 1   low 8 bits of the per-tag sequence number
//! then     one zigzag LEB128 varint per sample:
//!          key frame -> the value itself, otherwise the change from the previous record of this tag
//! ```
//!
//! Neighbouring sensor samples change little, so most samples take one or two bytes instead of
//! four. Each tag sends a key frame first and then every `KEY_INTERVAL` records, which bounds how
//! far a decoder that joined late (or lost a notification) stays wrong.
//!
//! Sequence numbers are 16 bits in the raw form but only the low byte travels; the decoder
//! rebuilds the rest, so a run of up to 255 missing records per tag is still counted correctly.

pub const RECORD_LEN: usize = 20;
pub const TAGS: usize = 16;
pub const MAX_SAMPLES: usize = 4;
/// Largest compact record: 2 header bytes + 4 samples x 5 bytes.
pub const MAX_COMPACT_LEN: usize = 2 + MAX_SAMPLES * 5;
/// A key frame is forced at least this often per tag.
pub const KEY_INTERVAL: u8 = 32;

fn put_varint(mut v: u32, out: &mut [u8], at: &mut usize) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out[*at] = b;
            *at += 1;
            return;
        }
        out[*at] = b | 0x80;
        *at += 1;
    }
}

fn get_varint(bytes: &[u8], at: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    for shift in (0..35).step_by(7) {
        let b = *bytes.get(*at)?;
        *at += 1;
        v |= ((b & 0x7F) as u32) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

fn unzigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

#[derive(Clone, Debug)]
pub struct Encoder {
    prev: [[i32; MAX_SAMPLES]; TAGS],
    prev_count: [u8; TAGS],
    since_key: [u8; TAGS],
    started: [bool; TAGS],
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub const fn new() -> Self {
        Self { prev: [[0; MAX_SAMPLES]; TAGS], prev_count: [0; TAGS], since_key: [0; TAGS], started: [false; TAGS] }
    }

    /// Forget all history: every tag sends a key frame next.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Appends the compact form of one raw record at `out[*len..]`. Returns false (and leaves
    /// every bit of state untouched) when it does not fit or the record is malformed.
    pub fn encode(&mut self, rec: &[u8], out: &mut [u8], len: &mut usize) -> bool {
        if rec.len() < 4 {
            return false;
        }
        let tag = (rec[0] as usize) & 0x0F;
        let count = rec[1] as usize;
        if count > MAX_SAMPLES || rec.len() < 4 + 4 * count {
            return false;
        }
        let key = !self.started[tag] || self.since_key[tag] >= KEY_INTERVAL || self.prev_count[tag] as usize != count;
        let mut tmp = [0u8; MAX_COMPACT_LEN];
        let mut n = 2;
        tmp[0] = tag as u8 | (key as u8) << 4 | (count as u8) << 5;
        tmp[1] = rec[2];
        let mut vals = [0i32; MAX_SAMPLES];
        for i in 0..count {
            let v = i32::from_le_bytes([rec[4 + 4 * i], rec[5 + 4 * i], rec[6 + 4 * i], rec[7 + 4 * i]]);
            vals[i] = v;
            let coded = if key { v } else { v.wrapping_sub(self.prev[tag][i]) };
            put_varint(zigzag(coded), &mut tmp, &mut n);
        }
        if *len + n > out.len() {
            return false;
        }
        out[*len..*len + n].copy_from_slice(&tmp[..n]);
        *len += n;
        self.prev[tag] = vals;
        self.prev_count[tag] = count as u8;
        self.started[tag] = true;
        self.since_key[tag] = if key { 0 } else { self.since_key[tag] + 1 };
        true
    }
}

#[derive(Clone, Debug)]
pub struct Decoder {
    prev: [[i32; MAX_SAMPLES]; TAGS],
    seq: [u16; TAGS],
    started: [bool; TAGS],
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Self {
        Self { prev: [[0; MAX_SAMPLES]; TAGS], seq: [0; TAGS], started: [false; TAGS] }
    }

    /// Decodes every record in `bytes`, calling `f(tag, seq, samples)` for each. A delta record
    /// whose tag has had no key frame yet (the decoder joined mid-stream) is skipped, not
    /// reported; records are self-delimiting so the rest of the notification still decodes.
    /// Returns false at the first structurally malformed record.
    pub fn decode(&mut self, bytes: &[u8], mut f: impl FnMut(u8, u16, &[i32])) -> bool {
        let mut at = 0;
        while at < bytes.len() {
            let b0 = bytes[at];
            let (tag, key, count) = ((b0 & 0x0F) as usize, b0 & 0x10 != 0, (b0 >> 5) as usize);
            let Some(&low) = bytes.get(at + 1) else { return false };
            if count > MAX_SAMPLES {
                return false;
            }
            let skip = !key && !self.started[tag];
            at += 2;
            let mut vals = [0i32; MAX_SAMPLES];
            for (i, v) in vals.iter_mut().enumerate().take(count) {
                let Some(raw) = get_varint(bytes, &mut at) else { return false };
                let d = unzigzag(raw);
                *v = if key { d } else { self.prev[tag][i].wrapping_add(d) };
            }
            if skip {
                continue;
            }
            let seq = if self.started[tag] {
                self.seq[tag].wrapping_add(low.wrapping_sub(self.seq[tag] as u8) as u16)
            } else {
                low as u16
            };
            self.prev[tag] = vals;
            self.seq[tag] = seq;
            self.started[tag] = true;
            f(tag as u8, seq, &vals[..count]);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(tag: u8, seq: u16, vals: &[i32]) -> [u8; RECORD_LEN] {
        let mut r = [0u8; RECORD_LEN];
        r[0] = tag;
        r[1] = vals.len() as u8;
        r[2..4].copy_from_slice(&seq.to_le_bytes());
        for (i, v) in vals.iter().enumerate() {
            r[4 + 4 * i..8 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        r
    }

    #[test]
    fn round_trips_and_shrinks_slowly_changing_signals() {
        let mut e = Encoder::new();
        let mut d = Decoder::new();
        let mut wire = [0u8; 4096];
        let mut len = 0;
        let mut sent = Vec::new();
        for i in 0..300u16 {
            let a = (1000.0 + 300.0 * (i as f64 / 20.0).sin()) as i32;
            for r in [rec(3, i, &[a, -a / 2, 4000 - a / 7]), rec(2, i, &[1300 + (i % 5) as i32, 1100, 200]), rec(1, i, &[880 + (i % 3) as i32])] {
                assert!(e.encode(&r, &mut wire, &mut len));
                sent.push(r);
            }
        }
        let mut got = Vec::new();
        assert!(d.decode(&wire[..len], |t, s, v| got.push(rec(t, s, v))));
        assert_eq!(got, sent);
        // 900 raw records are 18000 bytes.
        assert!(len < 18000 / 3, "compact size {len}");
    }

    #[test]
    fn extreme_values_and_sequence_wrap() {
        let mut e = Encoder::new();
        let mut d = Decoder::new();
        let mut wire = [0u8; 256];
        let mut len = 0;
        let rs = [rec(9, 65534, &[i32::MIN, i32::MAX]), rec(9, 65535, &[i32::MAX, i32::MIN]), rec(9, 0, &[0, 0]), rec(9, 3, &[-1, 1])];
        for r in &rs {
            assert!(e.encode(r, &mut wire, &mut len));
        }
        let mut got = Vec::new();
        assert!(d.decode(&wire[..len], |t, s, v| got.push(rec(t, s, v))));
        // Only the low sequence byte travels, so the decoder's numbers can differ from the
        // sender's in the high byte; what matters (loss counting) is that differences are exact.
        let seq = |r: &[u8; RECORD_LEN]| u16::from_le_bytes([r[2], r[3]]);
        for i in 1..rs.len() {
            assert_eq!(&got[i][4..], &rs[i][4..], "samples");
            assert_eq!(seq(&got[i]).wrapping_sub(seq(&got[i - 1])), seq(&rs[i]).wrapping_sub(seq(&rs[i - 1])));
        }
        assert_eq!(got[0][2], 254);
    }

    #[test]
    fn a_full_buffer_leaves_encoder_state_unchanged() {
        let mut e = Encoder::new();
        let mut tiny = [0u8; 3];
        let mut len = 0;
        assert!(!e.encode(&rec(3, 0, &[1, 2, 3]), &mut tiny, &mut len));
        assert_eq!(len, 0);
        let mut wire = [0u8; 64];
        assert!(e.encode(&rec(3, 0, &[1, 2, 3]), &mut wire, &mut len));
        assert_ne!(wire[0] & 0x10, 0, "still a key frame");
    }

    #[test]
    fn four_sample_diagnostics_record_round_trips() {
        let mut e = Encoder::new();
        let mut d = Decoder::new();
        let (mut wire, mut len) = ([0u8; 64], 0);
        let r = rec(11, 0, &[0, 508, 24, 51540]);
        assert!(e.encode(&r, &mut wire, &mut len));
        let mut got = Vec::new();
        assert!(d.decode(&wire[..len], |t, s, v| got.push(rec(t, s, v))));
        assert_eq!(got, vec![r]);
    }

    #[test]
    fn delta_without_key_frame_is_skipped_but_later_records_decode() {
        let mut e = Encoder::new();
        let (mut wire, mut len) = ([0u8; 64], 0);
        e.encode(&rec(3, 0, &[10]), &mut wire, &mut len); // key frame, never delivered
        let first = len;
        e.encode(&rec(3, 1, &[12]), &mut wire, &mut len); // delta for tag 3
        e.encode(&rec(5, 0, &[7, 8]), &mut wire, &mut len); // key frame for tag 5
        let mut d = Decoder::new();
        let mut got = Vec::new();
        assert!(d.decode(&wire[first..len], |t, s, v| got.push(rec(t, s, v))));
        assert_eq!(got, vec![rec(5, 0, &[7, 8])]);
    }

    #[test]
    fn periodic_key_frames_let_a_late_decoder_resync() {
        let mut e = Encoder::new();
        let mut wire = [0u8; 4096];
        let mut len = 0;
        let mut marks = Vec::new();
        for i in 0..100u16 {
            marks.push(len);
            assert!(e.encode(&rec(4, i, &[i as i32 * 3]), &mut wire, &mut len));
        }
        // Join at record 40: skip to the first key frame at or after it.
        let start = (40..100).find(|&i| wire[marks[i]] & 0x10 != 0).unwrap();
        let mut d = Decoder::new();
        let mut last = None;
        assert!(d.decode(&wire[marks[start]..len], |_, _, v| last = Some(v[0])));
        assert_eq!(last, Some(99 * 3));
    }
}
