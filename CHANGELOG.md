# 0.1.1 (2026-11-17)

## emotibit-ble firmware

- **Settings for battery life**, over Bluetooth (`CS`), in flash or `CONFIG.TXT`: LED brightness,
  CPU clock (80/160/240 MHz), sensor wake interval, connection and advertising intervals,
  transmit power, PPG LED current.
- **Profiles** that set them together: `default`, `high_frequency`, `low_noise`, `balanced`,
  `energy_saving`, `low_energy`; `CG` reports the matching one.
- The settings record in flash is version 2; version 1 records are still read.
- Fixed: on a slow connection interval, replies and status lines could be lost behind the
  sensor records (they now go first). A slow interval starts 5 s into a connection, so the
  central's setup and short commands run at 30 ms.
- `emotibit config set … --restart` waits for the board to drop the link, so the restart is
  not lost on a slow connection.
- **Updates over Bluetooth** (`emotibit firmware ota`): a new partition table with two app slots
  (`partitions.csv`); the board writes the update into the other slot, checks its SHA-256 and
  image hash, and restarts into it; an interrupted update changes nothing. Boards with the
  0.1.0 image need one install over USB for the slots (settings are kept). Data goes without
  response, six writes per acknowledged window, and compressed per 4 KB sector (35% less): about
  45 s for the app on macOS.
- The record queue holds 256 records (was 96): `low_energy`'s 120 ms interval dropped a few.

## Host

- `emotibit config profiles`; `config` shows every setting and the profile; the web pages'
  Settings panels have the profile and every setting.
- `emotibit firmware ota`; `firmware ble` resets `otadata` (so the board starts the app it
  wrote) and keeps emotibit-ble's settings across its partition table change; `firmware info`
  shows which app slot starts.
- `scripts/power-run.sh` retries a run whose stream did not start.
- `--gzip`: recordings (the raw log and the per-signal CSVs) as `.csv.gz`, about a sixth of the
  size; `parse` reads gzipped files, including a log cut short.

## Packaging

- Release binaries build with a `dist` profile (fat LTO, one codegen unit, stripped): about 40%
  smaller. The web pages' wasm builds with `opt-level = "z"` and `wasm-opt -Oz`: 56% smaller.
- `firmware/emotibit-ble.bin` is no longer padded to the 4 MB flash: 725 KB.
- Release archives are `.tar.xz` (Windows: `.zip`): 2.4 MB, from 6.1 MB.
- webbluetooth 0.0.5: on macOS a write without response could wait for ever, which stalled bulk
  transfers such as firmware updates.

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
