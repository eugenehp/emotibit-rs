//! emotibit-ble patch test: create files with long names, then read them back by long name.
//!
//! ```bash
//! cargo run --example lfn_create -- ./disk.img 12
//! ```

mod linux;
use linux::*;

use embedded_sdmmc::{Error, Mode, VolumeIdx};

type VolumeManager = embedded_sdmmc::VolumeManager<LinuxBlockDevice, Clock, 8, 4, 4>;

fn main() -> Result<(), Error<std::io::Error>> {
    let mut args = std::env::args().skip(1);
    let filename = args.next().expect("disk image");
    let count: usize = args.next().map(|n| n.parse().unwrap()).unwrap_or(12);
    let lbd = LinuxBlockDevice::new(filename, false).map_err(Error::DeviceError)?;
    let volume_mgr: VolumeManager = VolumeManager::new_with_limits(lbd, Clock, 0xAA00_0000);
    let volume = volume_mgr.open_raw_volume(VolumeIdx(0))?;
    let root = volume_mgr.open_root_dir(volume)?;
    for i in 0..count {
        let long = format!("2026-11-07_20-23-{i:02}-026091_info.json");
        let short = format!("EB{i:06}.JSN");
        let f = volume_mgr.create_file_with_long_name(root, short.as_str(), &long)?;
        volume_mgr.write(f, format!("{{\"file\":{i}}}\n").as_bytes())?;
        volume_mgr.close_file(f)?;
        println!("created {long} ({short})");
    }
    for i in 0..count {
        let long = format!("2026-11-07_20-23-{i:02}-026091_info.json");
        let f = volume_mgr.open_long_name_file_in_dir(root, &long, Mode::ReadOnly)?;
        let mut buf = [0u8; 64];
        let n = volume_mgr.read(f, &mut buf)?;
        volume_mgr.close_file(f)?;
        assert_eq!(&buf[..n], format!("{{\"file\":{i}}}\n").as_bytes());
    }
    println!("read back {count} files by long name");
    volume_mgr.close_dir(root)?;
    volume_mgr.close_volume(volume)?;
    Ok(())
}
