//! Per-boot reliability test of the firmware's debug-mode serial input.
//!
//! Resets the board, enters debug mode, waits for setup to finish, then sends `?`, `:` and `E`
//! one at a time (with a pause after each) and records which ones the firmware answered.
//! Board text is only matched, never printed, so WiFi credentials in the boot log stay hidden.
//!
//! ```sh
//! cargo run --example serial_diag -- /dev/cu.usbserial-XXXX 2000000 5 1500 1.0   # boots, answer wait ms, pre-wait s
//! ```

use std::io::{Read, Write};
use std::time::{Duration, Instant};

struct Port(Box<dyn serialport::SerialPort>);

impl Port {
    /// Read for `secs`, appending to `acc`.
    fn read_for(&mut self, secs: f64, acc: &mut String) {
        let t0 = Instant::now();
        let mut buf = [0u8; 4096];
        while t0.elapsed().as_secs_f64() < secs {
            if let Ok(n) = self.0.read(&mut buf) {
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }
    }
    fn wait_for(&mut self, needle: &str, secs: f64, acc: &mut String) -> bool {
        let t0 = Instant::now();
        let mut buf = [0u8; 4096];
        while t0.elapsed().as_secs_f64() < secs {
            if acc.contains(needle) {
                return true;
            }
            if let Ok(n) = self.0.read(&mut buf) {
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }
        acc.contains(needle)
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (name, baud, boots) = (&a[1], a[2].parse::<u32>().unwrap(), a[3].parse::<u32>().unwrap());
    let gap_ms: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(1500);
    let pre_wait: f64 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let mut p = Port(serialport::new(name, baud).timeout(Duration::from_millis(50)).open().expect("open"));
    for boot in 1..=boots {
        p.0.write_data_terminal_ready(false).unwrap();
        p.0.write_request_to_send(true).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        p.0.write_request_to_send(false).unwrap();

        let mut acc = String::new();
        if !p.wait_for("Enter C to enter WiFi config edit mode", 15.0, &mut acc) {
            println!("boot {boot}: no boot prompt (reset did not take)");
            continue;
        }
        p.0.write_all(b"D").unwrap();
        acc.clear();
        let entered = p.wait_for("ENTERING DEBUG MODE", 5.0, &mut acc);
        let banner = p.wait_for("Enter ? to know more", 60.0, &mut acc);
        // Optionally give the firmware a moment to leave setup before the first key.
        if pre_wait > 0.0 {
            p.read_for(pre_wait, &mut acc);
        }

        let mut res = Vec::new();
        for (key, answer) in [('?', "Debug tool options"), (':', "_serialData"), ('E', "acquireData.eda")] {
            acc.clear();
            p.0.write_all(&[key as u8]).unwrap();
            p.0.flush().unwrap();
            let t0 = Instant::now();
            let ok = p.wait_for(answer, gap_ms as f64 / 1000.0, &mut acc);
            res.push(if ok {
                format!("{key}=ok after {:.1}s", t0.elapsed().as_secs_f64())
            } else {
                format!("{key}=no answer in {:.0}s", gap_ms as f64 / 1000.0)
            });
        }
        println!("boot {boot}: debug={entered} banner={banner}  {}", res.join("  "));
    }
}
