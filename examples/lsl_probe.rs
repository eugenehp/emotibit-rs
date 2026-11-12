//! Check what LSL streams are visible and read a few samples from one.
//!
//! ```sh
//! emotibit stream --lsl &               # publish the signals
//! cargo run --example lsl_probe         # list streams, sample ACC_X
//! cargo run --example lsl_probe -- EDA  # sample another stream
//! ```

use rlsl::prelude::*;

fn main() {
    let want = Some(std::env::args().nth(1).unwrap_or_else(|| "ACC_X".into()));
    let streams = rlsl::resolver::resolve_all(3.0);
    if streams.is_empty() {
        println!("no LSL streams found");
        std::process::exit(1);
    }
    for s in &streams {
        println!("{:<10} type={:<16} rate={:<5} source={}", s.name(), s.type_(), s.nominal_srate(), s.source_id());
    }
    let Some(info) = streams.iter().find(|s| match &want {
        Some(w) => s.name() == *w,
        None => s.source_id().starts_with("MD-"),
    }) else {
        println!("no matching stream");
        std::process::exit(1);
    };
    let inlet = StreamInlet::new(info, 360, 0, true);
    inlet.open_stream(10.0).expect("open stream");
    println!("\nsamples from {}:", info.name());
    let mut buf = [0.0f32; 1];
    for _ in 0..5 {
        match inlet.pull_sample_f(&mut buf, 5.0) {
            // rlsl reports a timeout as timestamp 0.
            Ok(0.0) => {
                println!("  no sample within 5 s (is the stream producing data?)");
                std::process::exit(2);
            }
            Ok(ts) => println!("  t={ts:.3}  value={}", buf[0]),
            Err(e) => {
                println!("  timeout: {e}");
                std::process::exit(2);
            }
        }
    }
}
