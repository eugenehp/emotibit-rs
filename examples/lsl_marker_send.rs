//! Publish an LSL marker stream, for testing `emotibit stream --lsl-marker`.
//!
//! ```sh
//! cargo run --example lsl_marker_send                 # DataSyncMarker / 12345, the official default
//! emotibit stream --lsl-marker DataSyncMarker:12345
//! ```

use rlsl::prelude::*;

fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "DataSyncMarker".into());
    let source = std::env::args().nth(2).unwrap_or_else(|| "12345".into());
    let info = StreamInfo::new(&name, "Markers", 1, 0.0, ChannelFormat::String, &source);
    let outlet = StreamOutlet::new(&info, 0, 360);
    println!("publishing '{name}' (source {source}); a marker every second, Ctrl-C to stop");
    let mut i = 0u64;
    loop {
        i += 1;
        std::thread::sleep(std::time::Duration::from_secs(1));
        let marker = format!("marker-{i}");
        outlet.push_sample_str(std::slice::from_ref(&marker), 0.0, true);
        println!("sent {marker}");
    }
}
