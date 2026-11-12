//! Diagnose the USB-serial link: send bytes to the board and print how it answers.
//!
//! ```sh
//! cargo run --example serial_probe -- /dev/cu.usbserial-XXXX 2000000 ":" 0 3   # send ':' alone, listen 3 s
//! cargo run --example serial_probe -- PORT 2000000 ":E" 50 3                   # 50 ms between bytes
//! ```
//! Board output is passed through the same credential redaction as `emotibit serial`.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        eprintln!("usage: serial_probe PORT BAUD BYTES GAP_MS LISTEN_SECS");
        std::process::exit(2);
    }
    let (port, baud, bytes, gap, listen) = (&a[1], a[2].parse::<u32>().unwrap(), a[3].as_bytes(), a[4].parse::<u64>().unwrap(), a[5].parse::<u64>().unwrap());
    let mut p = serialport::new(port, baud).timeout(Duration::from_millis(50)).open().expect("open port");
    for &b in bytes {
        p.write_all(&[b]).unwrap();
        p.flush().unwrap();
        println!("sent {:?}", b as char);
        std::thread::sleep(Duration::from_millis(gap));
    }
    let t0 = Instant::now();
    let mut buf = [0u8; 4096];
    let mut text = String::new();
    while t0.elapsed() < Duration::from_secs(listen) {
        if let Ok(n) = p.read(&mut buf) {
            text.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }
    let mut shown = 0;
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // Skip the repeating JSON info blocks and numeric data lines for readability.
        if l.starts_with('"') || l.starts_with("[{") || l.starts_with("}}") || l.parse::<f64>().is_ok() {
            continue;
        }
        println!("  board: {}", emotibit::serial::redact(l));
        shown += 1;
        if shown > 25 {
            break;
        }
    }
    let nums = text.lines().filter(|l| l.trim().parse::<f64>().is_ok()).count();
    println!("({} numeric lines received)", nums);
}
