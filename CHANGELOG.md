# 0.1.0 (2026-11-11)

The first release.

## Stock firmware (WiFi)

- **`emotibit`**: `scan`, `info`, `stream`, `tui`, `parse`, `serve`, `web`, `power`, `serial`;
  `--all` for every board at once.
- **Recording and parsing**: raw logs while streaming, and `parse` into one CSV per signal with
  local timestamps from the clock-sync exchanges, as the DataParser does.
- **Outputs**: UDP, OSC (with an address map), LSL streams, and LSL markers forwarded to the board.
- **Remote control**: the Oscilloscope's UDP commands (`serve --remote`).
- **Browser oscilloscope** (`web/`): through a local bridge, or demo, replay and USB serial without
  one; built into the binary.
- **USB serial**: one signal from the stock firmware's debug mode, credentials hidden.

## emotibit-ble firmware (Bluetooth)

- A Rust `no_std` firmware for the Feather ESP32 that streams every sensor at the stock rates and
  units, with heart rate and SCR, over BLE (compact or raw records, data length extension).
- **SD card**: recording in the stock CSV format or a binary one, stock-style `_info.json` with a
  session summary, notes and LSL markers, clock syncs; list, download, rename, copy and delete
  recordings over Bluetooth.
- **Settings** (name, PPG rate, LEDs) in flash, on the card's `CONFIG.TXT`, or both.
- Power modes, the button and LEDs as on the stock firmware; a self-test.
- Every host command works with it (`--transport ble`), and so does a **Web Bluetooth page**
  (`examples/web`) built from the same library compiled to wasm.

## Firmware tools

- **`emotibit firmware`** (and `emotibit-flash`): `info` (installed firmware from the app
  descriptor, partition table), `backup` and `restore` of the whole flash, `ble` (the image is
  built in; the board's settings are kept on updates), `stock` (the official installer's files,
  from any OS's download, or another stock build with `--app`), and `write` for any ESP32 image.

## Fixed in dependencies (vendored for the firmware)

- **esp-hal 1.2.2**: the ESP32 I2C RX FIFO is read as ESP-IDF does; an interrupt during a read
  could skip a byte (about 19 corrupted samples in 10 minutes).
- **embedded-sdmmc 0.10.0**: long file names (create, delete, rename); deleting frees clusters;
  long-name lookup is case-insensitive and no longer misses a file after a partial match; the
  FSInfo free count is right after a delete; CSD registers with a zero CRC are accepted.

## Packaging

- The modules the host and the firmware share (`codec`, `units`, `stock`, `derive`) live in
  `src/shared/`, so they ship with the crate; the firmware takes them in with `#[path]`.
- `scripts/release.sh`: release archives (binaries, firmware image, Web Bluetooth page, docs) and
  `SHA256SUMS`; GitHub Actions for CI and tagged releases (macOS, Linux, Windows).
