//! emotibit-ble patch test: rename and delete files with long names.
//!
//! ```bash
//! cargo run --example lfn_ops -- ./disk.img
//! ```

mod linux;
use linux::*;

use embedded_sdmmc::{Error, Mode, VolumeIdx};

type VolumeManager = embedded_sdmmc::VolumeManager<LinuxBlockDevice, Clock, 8, 4, 4>;

fn main() -> Result<(), Error<std::io::Error>> {
    let filename = std::env::args().nth(1).expect("disk image");
    let lbd = LinuxBlockDevice::new(filename, false).map_err(Error::DeviceError)?;
    let mgr: VolumeManager = VolumeManager::new_with_limits(lbd, Clock, 0xAA00_0000);
    let volume = mgr.open_raw_volume(VolumeIdx(0))?;
    let root = mgr.open_root_dir(volume)?;
    // Files of a few clusters each, so deleting one frees space.
    let body = vec![b'x'; 40_000];
    for i in 0..6 {
        let f = mgr.create_file_with_long_name(root, format!("OP{i:06}.CSV").as_str(), &format!("2026-11-08_10-00-0{i}-000000.csv"))?;
        mgr.write(f, &body)?;
        mgr.close_file(f)?;
    }
    mgr.rename_file_long(root, "2026-11-08_10-00-02-000000.csv", "OPREN001.CSV", "renamed session with a much longer name.csv")?;
    mgr.delete_file_any_name(root, "2026-11-08_10-00-04-000000.csv")?;
    // The renamed file keeps its data; the old names are gone.
    let f = mgr.open_long_name_file_in_dir(root, "renamed session with a much longer name.csv", Mode::ReadOnly)?;
    assert_eq!(mgr.file_length(f)?, 40_000);
    mgr.close_file(f)?;
    assert!(mgr.open_long_name_file_in_dir(root, "2026-11-08_10-00-02-000000.csv", Mode::ReadOnly).is_err());
    assert!(mgr.open_long_name_file_in_dir(root, "2026-11-08_10-00-04-000000.csv", Mode::ReadOnly).is_err());
    println!("renamed and deleted");
    // The board's sequence: an 8.3-only file renamed twice, copied, then deleted by long name
    // in another case.
    let f = mgr.open_file_in_dir(root, "EBTEST.CSV", Mode::ReadWriteCreate)?;
    mgr.write(f, b"1,1,1,UN,1,100,test\n")?;
    mgr.close_file(f)?;
    mgr.rename_file_long(root, "ebtest.csv", "OPREN002.CSV", "no info test.CSV")?;
    mgr.rename_file_long(root, "no info test.csv", "OPREN003.CSV", "no info test2.csv")?;
    let src = mgr.open_long_name_file_in_dir(root, "NO INFO TEST2.CSV", Mode::ReadOnly)?;
    let dst = mgr.create_file_with_long_name(root, "OPREN004.CSV", "no info copy.csv")?;
    let mut buf = [0u8; 64];
    let n = mgr.read(src, &mut buf)?;
    mgr.write(dst, &buf[..n])?;
    mgr.close_file(src)?;
    mgr.close_file(dst)?;
    mgr.iterate_dir_lfn(root, &mut embedded_sdmmc::LfnBuffer::new(&mut [0u8; 128]), |e, l| {
        println!("  {} {:?} {}", e.name, l, e.size);
        core::ops::ControlFlow::Continue(())
    })?;
    mgr.delete_file_any_name(root, "no info test2.csv")?;
    mgr.delete_file_any_name(root, "no info copy.csv")?;
    println!("board sequence ok");
    mgr.close_dir(root)?;
    mgr.close_volume(volume)?;
    Ok(())
}
