# emotibit-ble

A Rust `no_std` firmware for the EmotiBit on an Adafruit Feather ESP32 (HUZZAH32) that streams
every sensor over Bluetooth Low Energy and records on the board's SD card. The host side is
`emotibit` (`--transport ble`, and the browser version in `examples/web`). The main
[README](../../README.md#ble-a-custom-firmware-that-streams-everything) covers using it; this
file is the reference: building, the architecture, the protocol, the file formats, and testing.

- [Building and flashing](#building-and-flashing)
- [Architecture](#architecture)
- [Bluetooth protocol](#bluetooth-protocol)
- [Commands and replies](#commands-and-replies)
- [SD card](#sd-card)
- [Settings](#settings)
- [Sensors](#sensors)
- [Patched dependencies](#patched-dependencies)
- [Testing](#testing)
- [Known issues](#known-issues)

## Building and flashing

**Installing** needs no toolchain: `emotibit firmware ble` writes the image built into the host
tools (`firmware/emotibit-ble.bin`); see the main README's
[Firmware](../../README.md#firmware-install-back-up-restore) section, which also covers backups
and going back to the stock firmware (`firmware stock` or `firmware restore`).

**Building** needs the Xtensa Rust toolchain, pinned in `rust-toolchain.toml` as `esp195`
(Xtensa Rust 1.97), and espflash:

```sh
cargo install espup espflash                                  # once
espup install --targets esp32 --toolchain-version 1.97.0.0 --name esp195   # once; writes ~/export-esp.sh
```

From the repository root:

```sh
scripts/ble-firmware.sh                 # build → firmware/emotibit-ble.bin (merged image)
scripts/ble-firmware.sh flash [PORT]    # build, then install it (emotibit firmware ble --image …)
cargo build --release                   # embed the new image in emotibit / emotibit-flash
```

The script sources `$ESP_EXPORT`, `~/export-esp.sh` or `~/export-esp195.sh`. Or from this
directory: `cargo build --release`, then `espflash flash --chip esp32 --port PORT
target/xtensa-esp32-none-elf/release/emotibit-ble`. `firmware/emotibit-ble.bin` is a merged
image (bootloader at 0x1000, partition table at 0x8000, app at 0x10000), so
`esptool.py write-flash 0 firmware/emotibit-ble.bin` also installs it, erasing the settings.
`emotibit firmware ble` and `espflash flash` keep the settings sector (see [Settings](#settings)).

Opening the Feather's serial port resets it (DTR/RTS). The log is at 115200 baud; it never
contains WiFi credentials, and the settings file is never logged.

## Architecture

```
core 0 (embassy, esp-rtos)                                  core 1 (embassy)
  sensors::run   I2C at 400 kHz, 2 ms tick; records ──┬─▶ QUEUE ─▶ BLE notifications
                 LEDs, self-test, battery            └─▶ SD_DATA ─▶ sdcard::run (SPI)
  BLE runner     trouble-host over esp-radio                         CSV or binary file,
  radio/session  advertise, connect, stream(), tune, status          _info.json, CONFIG.TXT,
  buttons        short/long press, hibernate                         FL/FS/FG/FO… commands
  settings       flash writes (CS), restart (CR)
```

| Module | What it does |
|---|---|
| `bin/main.rs` | Pins, peripherals, the GATT server, advertising, sessions, button, settings task |
| `sensors.rs` | Drivers (ADS1114 EDA, BMI160+BMM150, MAX30101, MLX90632, Si7013) and the scheduler |
| `control.rs` | Shared state, the command parser, `EM`/`RD`/`DC`/`DO` lines, settings commands |
| `sdcard.rs` | The SD writer on core 1: files, sessions, downloads, `CONFIG.TXT` |
| `board.rs` | The EEPROM identity and the LED drivers (NCP5623, KTD2026B) |
| `settings.rs` | Settings in flash |
| `adc_cal.rs` | The ESP32's eFuse ADC calibration (battery) |
| `../../src/shared/`: `codec.rs`, `units.rs`, `stock.rs`, `derive.rs` | Shared verbatim with the host, which owns them (so they ship with the published crate); `src/lib.rs` takes them in with `#[path]`: the compact codec, unit conversion, records to stock lines and file formats, heart rate and SCR |

Pins (as the stock firmware): I2C SDA 27 / SCL 13; regulator enable GPIO 32 (high = on); battery
divider GPIO 35 (ADC1, half the battery voltage); button GPIO 12 (high = pressed); SD card SPI
SCK 5, MOSI 18, MISO 19, CS 4.

## Bluetooth protocol

One service, `6e6f7469-6269-7400-0000-000000000001`, with four characteristics:

| UUID suffix | Name | Properties | Content |
|---|---|---|---|
| `…0002` | data | notify | sensor records, batched up to the ATT MTU (up to 508) |
| `…0003` | mode | read, write | wire format: 0 raw, 1 compact; every write restarts the encoder with key frames |
| `…0004` | control | write | host commands: stock packet lines, up to 500 bytes |
| `…0005` | events | notify | board status and replies: stock packet lines, up to 480 bytes |

The board advertises as `EmotiBit-BLE-<name>` (at most 22 bytes): the serial number, a
configured name, or MAC bytes. Its static random address mixes the chip MAC, the GATT layout
version (`GATT_LAYOUT` in `main.rs`) and the name, because macOS caches a device's
characteristics and name by address: bump `GATT_LAYOUT` when characteristics change. After
connecting the board asks for 251-byte link-layer packets and a 15-30 ms interval (400-500 ms in
max-low power).

**Records** are 20 bytes: `u8 tag, u8 count, u16 seq (per tag), up to 4 × i32` in raw device
units. The compact format (`codec.rs`) sends the same records as deltas in zigzag varints, 4-6
bytes each. Sequence numbers count streamed records only, so gaps mean records lost on the way.

| Tag | Record | Samples |
|---|---|---|
| 1 | EDA | mean ADS1114 count over 64 ms, × 16 |
| 2 | PPG | red, infrared, green (18-bit) |
| 3 / 4 | accelerometer / gyroscope | x, y, z (±8 g / ±1000 °/s) |
| 5 | magnetometer | BMM150 x, y, z, rhall |
| 6 | thermopile | MLX90632 RAM_6, RAM_9, object sum |
| 7 | TEMP1 | MAX30101 die temperature, m°C |
| 8 | battery | raw ADC, calibrated pin mV |
| 9 / 10 | MLX / BMM calibration | index, value (every 10 s, and at record start) |
| 11 | link | dropped, ATT MTU, records in batch, bytes sent |
| 12 | Si7013 | temperature code, humidity code |
| 13 | clock | device ms of the records that follow in the notification |
| 14 | info | PPG rate (Hz) |

The host turns records into stock tags and units with `stock::Records` and `units` (the same
code the board uses for CSV files).

## Commands and replies

Commands are stock EmotiBit packet lines (`timestamp,number,count,TAG,1,100,payload…`) written
to the control characteristic, the same packets the stock firmware takes over TCP. Replies come
as packet lines on the events characteristic.

| Command | Payload | Effect | Reply |
|---|---|---|---|
| `RB` | local time [, `FMT`, `BIN`] | start recording (CSV, or binary) | `EM` |
| `RE` | local time | stop recording | `EM` |
| `UN` | local time, text | note in the file | |
| `LM`, `TX` | as the Oscilloscope | LSL marker and cross-time lines in the file | |
| `TL`, `AK` | host time; acked number, `RD` | answer to the board's `RD` | echoed, restamped |
| `MN` `ML` `MM` `MO` `MH` | local time | power: normal, low, max-low, wireless off, hibernate | `EM` |
| `CG` | | read settings | `CG,name,…,ppg_hz,…,leds,…,source,…,restart_needed,…,device_id,…` |
| `CS` | `key,value,…` (`name`, `ppg_hz`, `leds`, `store`) | change settings | `CS,ok\|error,…`, `CG`, and `CW` if stored on the card |
| `CC` | `flash`, `sd` or `both` | forget stored settings | `CC`, `CW` |
| `CR` | | restart (closes a recording first) | |
| `ST` | | self-test | `ST,item,PASS\|FAIL\|ABSENT\|OFF,…` |
| `FL` | | list files | `FL,name,size` per file, then `FL` |
| `FS` | | list recordings | `FS,file,format,state,duration_s,bytes,notes,markers,created` each, then `FS` |
| `FG` | name [, offset, length] | send a file | `FG,name,size`, `FD,offset,base64` lines, `FE,name,end` |
| `FO`, `FA`, `FC` | 8.3 name; base64; | write a file: open, append, close | `FC,name,bytes` |
| `FX` | name (long or 8.3) | delete one file | `FX,name,deleted\|not found\|protected\|busy recording` |
| `FR` | data file, new name | rename a recording (data file and `_info.json`) | `FR,file,ok,new file` or `FR,file,error,why` |
| `FY` | data file, new name | copy a recording | `FY,file,copying,done,total` every 256 KB, then as `FR` |
| `FZ` | data file | delete a recording | `FZ,file,ok,file` or `FZ,file,error,why` |

Lines the board sends by itself:

| Line | When |
|---|---|
| `EM,RS,RB\|RE[,file],PS,<power>` | once a second while connected, and on changes |
| `RD,TL,TU` | every 5 s: the host answers `TL` and `AK` (clock sync) |
| `SD,<percent>` | every 10 s: card fill level |
| `DC,TAG,n,…` / `DO,TAG,n,…` | once a second when samples clipped / records were dropped |
| `BS`, `BL` | short / long button press |

Payload fields cannot contain commas; the host replaces them in notes.

## SD card

Recordings are written on core 1 in 4 KB chunks and flushed every 2 s, so card latency never
holds up the sensors. Each recording is:

- **`<time>.csv`** or **`<time>.ebb`**, `<time>` being the host's local time from `RB`
  (`2026-11-07_23-49-09-480343`), as the stock firmware names files. Each file also has an 8.3
  alias (`EB000014.CSV`). The first line is a `UN` note with the stock-style name.
  - CSV: stock packet lines in stock units, including `HR`, `BI`, `SA`, `SR`, `SF`, `DC`, `DO`,
    notes, markers and clock syncs, readable by the official DataParser and `emotibit parse`.
  - Binary: `EMOTIBB1`, then frames: `0xEB` + u32 device ms + a 20-byte record, or `0x54` + u16
    length + a stock line. About 3.4× smaller; `emotibit parse` converts it to the same lines.
- **`<time>_info.json`**: the stock firmware's info file (device, hardware, firmware, one entry
  per signal group with tags, rate and units), plus a `session` member in the device entry:

  ```json
  "session": {"data_file": "2026-11-07_23-49-09-480343.csv", "format": "csv", "state": "done",
              "duration_s": 20.5, "bytes": 209331, "notes": 0, "markers": 0}
  ```

  Written with `"state": "recording"` at the start and rewritten when the recording ends; a file
  still saying `recording` when nothing is recording was interrupted (power, reset). `FS` reads
  these to list sessions.

`FR`, `FY` and `FZ` take the data file's name (any case) and act on its `<stem>_info.json` too,
when there is one. The new name is the part before the extension: letters, digits, space, `-`,
`_`, `.`, `(`, `)`, up to 36, no leading or trailing space or dot; the extension (lower-cased)
stays. A rename moves the directory entries (no data is copied) and points the info file's
`data_file` at the new name; `created_at` keeps the start, which `FS` reports as `created`. A copy
reads and writes 4 KB at a time on core 1, about 300 kB/s; a failed copy is removed. Deletes
free the clusters. All three answer `busy recording` during a recording, `name taken` when the
new name (or its info file) exists. `FX` refuses `CONFIG.TXT`, which holds the stock firmware's
WiFi settings.

Long file names need FAT32 (SDHC/SDXC cards are). The card also holds `CONFIG.TXT` (settings,
below) and `CONFIG.BAK` (its original, saved before the firmware first changed it).

## Settings

| Key | Values | Applies |
|---|---|---|
| `name` | up to 9 letters, digits, `-`, `_`; empty: the serial number | after a restart |
| `ppg_hz` | 25 (stock), 100 (stock PPG-100: 800 sps averaged by 8, 118 µs) | at once |
| `leds` | on / off | at once |

Read at boot: the card's `CONFIG.TXT`, `EmotiBitBle` section, if present; otherwise the flash
sector; otherwise defaults. `CG` reports which (`source`).

- **Flash:** 4 KB at 0x9000 (the default partition table's `nvs`, unused by this firmware):
  `EBCF`, version 1, name length, name (9 bytes), PPG Hz, LEDs, a one-byte sum. Written through
  `esp-storage` with the second core parked for the write.
- **Card:** `{"WifiCredentials": [...], "EmotiBitBle": {"name": "lab3", "ppg_hz": 100, "leds": false}}`.
  The firmware replaces only the `EmotiBitBle` member (`stock::json_with_member`), copies the
  original to `CONFIG.BAK` once, refuses a file that is not a JSON object, and never logs it.

## Sensors

| Signal | Chip | Rate | Notes |
|---|---|---|---|
| EDA, EDL | ADS1114 (0x48) | 15.625 Hz | 250 SPS, read every 4 ms, averaged over timed 64 ms windows |
| PPG red/IR/green | MAX30101 (0x57) | 25 or 100 Hz | FIFO drained every 10 ms |
| TEMP1 | MAX30101 die | 7.5 Hz | reads outside -40..85 °C or 5 °C off the last are dropped |
| Accelerometer, gyroscope | BMI160 (0x68) | 25 Hz | on data-ready flags |
| Magnetometer | BMM150 behind the BMI160 | 25 Hz | Bosch compensation on the host / in CSV |
| Thermopile | MLX90632 (0x3A) | about 7.5 Hz | step mode; restarted after 8 identical readings |
| TEMP0, humidity | Si7013 (0x40) | 2 Hz | V2/V3 boards only; untested |
| Battery | ESP32 ADC1 | every 5 s | eFuse-calibrated, as esp-idf's `esp_adc_cal` |
| Identity | EEPROM (0x50) | at boot | stock memory map: SKU, hardware version, serial |
| LEDs | NCP5623 (0x38/0x39) or KTD2026B (0x31) | 20 Hz | by hardware version |

## Patched dependencies

`vendor/`, used through `[patch.crates-io]` in `Cargo.toml`. Keep the patches when upgrading
unless upstream has them.

- **esp-hal 1.2.2**, `src/i2c/master/low_level/v1.rs`, `read_fifo`: reads the ESP32 I2C RX FIFO
  as ESP-IDF does (interrupts masked, a dummy APB read first). Without it an interrupt during a
  read can skip a FIFO byte: about 19 corrupted samples in 10 minutes, across every sensor.
- **embedded-sdmmc 0.10.0**, `src/sdcard/spi.rs`: accepts a CSD register whose own CRC7 is zero
  (some cards; SdFat does not check it either). `src/fat/volume.rs` and `src/volume_mgr.rs`:
  `create_file_with_long_name`, which writes VFAT long-name entries (FAT32);
  `delete_file_any_name` (long-name entries too, and frees the clusters, which the stock
  `delete_entry_in_dir` leaves allocated); `rename_file_long`. Fixed upstream bugs: long-name
  lookup is case-insensitive, and a partial match (another name with the same ending) no longer
  hides the next file from lookup by long name; freeing a cluster chain counts its last cluster
  in the FSInfo free count. Tests in `vendor/embedded-sdmmc`, against a FAT32 image
  (`hdiutil create -size 64m -fs "MS-DOS FAT32" -layout MBRSPUD`, then
  `hdiutil convert -format UDTO`): `cargo +stable run --target aarch64-apple-darwin --example
  lfn_create -- disk.img 12` and `--example lfn_ops -- disk.img`, then `fsck_msdos -n` and
  mount it.

## Testing

- **Long runs:** `emotibit ble --seconds 600 --record --record-dir DIR`, then
  `scripts/ble-spikes.py DIR` (isolated one-sample spikes per channel; expect `total 0`) and the
  report's `lost` and `dropped in firmware` (expect 0). Short runs hide link problems: the link
  fell behind only after about 14 s before data length extension was requested. Last run
  (2026-11-08, V7 board, macOS, the `webbluetooth` client, compact format, SD recording on):
  79,182 records in 600.5 s (131.9/s), 0 lost, 0 dropped in firmware, 0 spikes, 120 clock syncs.
- **SD card file operations:** `examples/lfn_ops` in `vendor/embedded-sdmmc` against a FAT32
  image, then `fsck_msdos -n` (see [Patched dependencies](#patched-dependencies)); on the board,
  `emotibit sd rename|copy|delete` on recordings made for the test.
- **SD files:** `emotibit sd get <file> --parse`; check rates and gaps in the CSVs.
- **The board's log:** `espflash monitor` (resets the board).
- **What is installed:** `emotibit firmware info` (version from the app descriptor).
- **Host tests:** `cargo test` in the repository root also runs the tests of the shared modules
  (`codec`, `units`, `stock`, `derive`).

## Known issues

- About 1 in 900 MAX30101 die-temperature reads is wrong (mostly 110-111 °C) even after the chip
  reports the conversion ready; the stock board showed none. They are dropped and logged.
- An unworn PPG sensor yields about 3 false heart beats a minute.
- Notifications are paced at about 5 kB/s for downloads: faster, and macOS ended the link.
- The button (short press, long press, wake from hibernate) and the LEDs' light were not
  verified by someone at the board; the Si7013 driver has no hardware to run on here.
- No WiFi, and no factory-test mode.
