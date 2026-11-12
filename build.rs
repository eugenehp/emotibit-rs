//! Embeds into the binary what has been built: the browser UI and the BLE firmware image.
//!
//! `scripts/build-web.sh` writes `web/pkg/`; if that exists at compile time the page, script
//! and wasm are compiled in (cfg `embedded_web`) so `emotibit web` works with no files on
//! disk. Without it the binary still works and looks for a web directory at run time.
//!
//! With the `flash` feature, `firmware/emotibit-ble.bin` (the merged image `scripts/ble-firmware.sh`
//! writes) is compiled in without its trailing erased bytes (cfg `embedded_ble_firmware`), so
//! `emotibit firmware ble` needs no files either.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(embedded_web)");
    for f in ["web/index.html", "web/main.js", "web/pkg/emotibit.js", "web/pkg/emotibit_bg.wasm"] {
        println!("cargo:rerun-if-changed={f}");
    }
    let ready = ["web/index.html", "web/main.js", "web/pkg/emotibit.js", "web/pkg/emotibit_bg.wasm"]
        .iter()
        .all(|f| std::path::Path::new(f).exists());
    if ready {
        println!("cargo:rustc-cfg=embedded_web");
    }

    println!("cargo:rustc-check-cfg=cfg(embedded_ble_firmware)");
    println!("cargo:rerun-if-changed=firmware/emotibit-ble.bin");
    if std::env::var_os("CARGO_FEATURE_FLASH").is_some()
        && let Ok(image) = std::fs::read("firmware/emotibit-ble.bin")
    {
        // A merged image: bootloader at 0x1000, partition table at 0x8000.
        if image.len() > 0x9000 && image[0x1000] == 0xE9 && image[0x8000..0x8002] == [0xAA, 0x50] {
            let end = image.iter().rposition(|&b| b != 0xFF).map_or(0, |i| i + 1);
            let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("emotibit-ble.bin");
            std::fs::write(out, &image[..end]).unwrap();
            println!("cargo:rustc-cfg=embedded_ble_firmware");
        } else {
            println!("cargo:warning=firmware/emotibit-ble.bin is not a merged image; not embedded");
        }
    }
}
