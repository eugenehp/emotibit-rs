# emotibit

Rust tools for [EmotiBit](https://www.emotibit.com/) boards, with either firmware:

- the **stock firmware**, over **WiFi**: a library and CLI that do what the official
  Oscilloscope and DataParser do;
- **[`emotibit-ble`](firmware/emotibit-ble/README.md)**, this project's firmware, over
  **Bluetooth**: every sensor, the SD card, settings and self-test, no WiFi network needed.

Around both: a terminal oscilloscope, two browser versions, LSL/OSC/UDP outputs, remote control,
a recording parser, and a USB flasher that installs either firmware (or any ESP32 image) and
backs a board up. Structured like [`muse-rs`](https://github.com/eugenehp/muse-rs).

**Contents:** [Install](#install) · [Quick start](#quick-start) · [Commands](#commands) ·
[TUI](#terminal-oscilloscope-tui) · [Recording and parsing](#recording-and-parsing) ·
[Outputs](#outputs) · [Browser versions](#browser-version-examplesweb) ·
[USB serial](#usb-serial-no-wifi-no-bridge) · [BLE firmware](#ble-a-custom-firmware-that-streams-everything) ·
[Remote control](#remote-control) · [Library](#library) · [Firmware](#firmware-install-back-up-restore) ·
[Development and releases](#development-and-releases) · [Troubleshooting](#troubleshooting)

| Document | What is in it |
|---|---|
| this file | using the tools with either firmware |
| [`firmware/emotibit-ble/README.md`](firmware/emotibit-ble/README.md) | the BLE firmware: architecture, GATT protocol, commands, SD formats, settings, testing |
| [`examples/web/README.md`](examples/web/README.md) | the Web Bluetooth page: running, hosting, how it is built |
| [`CHANGELOG.md`](CHANGELOG.md) | what changed, per release |
| `src/protocol.rs`, `src/ble.rs` | the stock wire protocol; the BLE record formats (`cargo doc --open`) |

## Install

**A release archive** from [Releases](https://github.com/eugenehp/emotibit-rs/releases) (macOS
arm64 and x86_64, Linux x86_64, Windows x86_64) has everything, nothing else to install. Check it
against `SHA256SUMS` (`shasum -a 256 -c SHA256SUMS --ignore-missing`):

| File | |
|---|---|
| `emotibit` | the command line, TUI, browser bridge, and `emotibit firmware …` |
| `emotibit-flash` | the firmware commands on their own |
| `firmware/emotibit-ble.bin` | the BLE firmware, for other flashers (it is also built into both binaries) |
| `web-ble/` | the Web Bluetooth page: serve it from `localhost` or any HTTPS host |
| `docs/` | the firmware and web page references (`firmware/emotibit-ble/README.md` and `examples/web/README.md` here) |

macOS may block a downloaded binary: `xattr -d com.apple.quarantine emotibit emotibit-flash`.
On Linux, Bluetooth needs BlueZ, and the serial port needs your user in the `dialout` group.

**From source** (Rust 1.94 or newer):

```sh
cargo install emotibit              # from crates.io, once published
cargo install --git https://github.com/eugenehp/emotibit-rs
cargo install --path .              # from a checkout: emotibit and emotibit-flash, into ~/.cargo/bin
# or: cargo build --release          → target/release/
```

On Linux first: `sudo apt-get install libdbus-1-dev libudev-dev pkg-config`.
`scripts/release.sh` builds the same archive as a release, for this machine.

## Quick start

| | Stock firmware | emotibit-ble |
|---|---|---|
| Link | WiFi (2.4 GHz; the computer on the same network) | Bluetooth LE |
| Set up | `config.txt` with WiFi credentials on the SD card (`scripts/cli.sh configure`) | nothing |
| Works with | this project, the official Oscilloscope and DataParser | this project (CLI, TUI, both web pages) |
| Install | `emotibit firmware stock` (needs the official installer's files) | `emotibit firmware ble` (built in) |

Plug the Feather into USB (HIB switch off), then:

```sh
emotibit firmware info                       # what is on the board now
emotibit firmware backup                     # keep a copy of it (restore with `firmware restore FILE`)
emotibit firmware ble                        # install emotibit-ble
emotibit --transport ble tui                 # and watch it
```

With the stock firmware, no `--transport`: `emotibit scan`, `emotibit tui`. Only one host
can hold a board at a time, so **close the EmotiBit Oscilloscope** before connecting from here.

## Commands

```sh
emotibit scan                             # devices on the network: id, IP, available / in use
emotibit info                             # connect 5 s: channels, measured rates, status, gaps, clock sync
emotibit stream                           # once-a-second summary (default command)
emotibit stream --raw                     # every sample: recv_ms<TAB>tag<TAB>device_ms<TAB>values
emotibit stream --seconds 60 --record     # also record to the device's SD card
emotibit tui                              # terminal oscilloscope
emotibit parse FILE                       # raw recording -> one CSV per signal
emotibit web                              # browser oscilloscope (bridge + page + opens browser)
emotibit serve --ws 127.0.0.1:8765        # headless daemon: bridge, remote control, outputs
emotibit power low                        # set power mode
emotibit --transport ble tui              # the same over Bluetooth (emotibit-ble firmware)
emotibit sd sessions                      # SD card of a BLE board: recordings, get, rename, copy, delete
emotibit firmware info                    # over USB: what is installed; backup, restore, stock, ble, write
```

Global options: `--device <ID|IP>`, `--ip <IP>` (ask one address, skip the network walk),
`--timeout <SECS>` (scan time, default 5), `--all` (use **every** available device at once),
`--transport wifi|ble|both` (where to look; default wifi), `--ble-format compact|raw`.
`RUST_LOG=debug` shows protocol detail.

Measured on a real device (v1.16.24 firmware): ACC / GYRO / MAG / PPG 25 Hz, EDA 15 Hz,
TEMP1 / THERM 7.5 Hz, plus derived SCR metrics, heart rate and battery. These match the nominal
rates in the official `lslOutputSettings.json`.

## Terminal oscilloscope (`tui`)

One chart per signal with the Oscilloscope's header: device, battery, recording, power mode,
clipping / overflow counters, sequence gaps. With `--all`, `Tab` switches between devices.

| Key | Action |
|---|---|
| `←` / `→` | shorter / longer time window (1 s … 60 s) |
| `Space` | pause the display (data keeps being collected) |
| `r` | start / stop recording on the device's SD card |
| `n` | add a note to the recording (Enter sends, Esc cancels) |
| `m` | cycle power mode normal → low → max-low → wireless-off (never hibernate) |
| `Tab` | next device |
| `q` / `Esc` / `Ctrl-C` | quit |

Heart rate and the SCR metrics only arrive when an event is detected; like the Oscilloscope they are
*periodized* (held at the sample rate of a source signal) so they can be plotted.

## Recording and parsing

```sh
emotibit stream --record-dir rec --seconds 60
```

writes `rec/<device>_<time>.csv`, a **raw log** (one packet per line, the same format the device
writes to its SD card), then converts it. `emotibit parse` does the same conversion for any raw
file, including one copied off the SD card. With `--gzip` the log and the CSVs are gzipped
(`.csv.gz`, about a sixth of the size: 3 MB instead of 17 MB for 10 minutes of a BLE board);
`parse` reads gzipped files either way, including a log cut short by a crash:

```sh
emotibit parse /Volumes/FLASHDEVICE/2026-11-05_18-25-09.csv --out parsed
```

Output, like the official DataParser: `<base>_<TAG>.csv` per signal with columns
`LocalTimestamp, EmotiBitTimestamp, PacketNumber, DataLength, TypeTag, ProtocolVersion,
DataReliability, <TAG>`. Sensor packets get **one row per sample**; sample times are interpolated
between packets. `LocalTimestamp` is epoch seconds on the *computer's* clock, derived from the
device's clock-sync exchanges (a straight line through the fastest-round-trip sync points in the
first and last quarter of the recording). Also written: `_timeSyncMap.csv` (the mapping) and
`_timeSyncs.csv` (each exchange). Checked on a live recording: reconstructed timestamps land on the
actual wall-clock time, with round trips of 8-12 ms.

`parse` also reads the binary SD files of the BLE firmware (`.EBB`, see below).

Not converted: the DataParser's optional LSL-clock columns (`LC`/`LM` cross-time packets).
Without any clock sync in a file, `LocalTimestamp` is relative to the start and the tool says so.

## Outputs

Available on `stream`, `tui` and `serve`:

| Flag | Output |
|---|---|
| `--udp HOST:PORT` | forward every raw packet line as a UDP datagram |
| `--osc HOST:PORT` | OSC messages, address `/<device>/<channel>` (e.g. `/MD-V6-0000356/ACC:X`), one float per sample. `--osc-map FILE`: JSON `{"AX": ["/a/x"]}` to override addresses |
| `--lsl` | publish 19 single-channel [LSL](https://labstreaminglayer.org/) streams named as in `lslOutputSettings.json` (`PPG_RED`, `EDA`, `ACC_X`, …), source id = device id |
| `--lsl-marker NAME[:SOURCE_ID]` | read an LSL marker stream and forward it to the device (`LM` + two `TX` cross-time packets), like the Oscilloscope; the official default is `DataSyncMarker:12345` |

LSL uses the pure-Rust [`rlsl`](https://crates.io/crates/rlsl) crate. Check published streams with
`cargo run --example lsl_probe`, and test marker forwarding with `cargo run --example lsl_marker_send`.
The device relays each marker back into its data stream, so markers end up in recordings. LSL does not
replay markers sent before the receiver connected, so start the receiver first.

## Browser version (`examples/web`)

The library itself in a browser, over the browser's own Web Bluetooth, like `muse-rs`: the BLE
client, conversion, heart rate, clock sync, SD card protocol and settings are the same Rust
code, compiled to `wasm32-unknown-unknown` on the same `webbluetooth` layer the native build
uses. The page is a canvas drawn by a worker plus some glue; no bridge, no wasm-bindgen.

```sh
rustup target add wasm32-unknown-unknown   # once
examples/web/build.sh
python3 -m http.server --directory examples/web/dist 8000   # then http://localhost:8000/ in Chrome or Edge
```

It does what the command line does with a BLE board: every signal plotted, SD recording (CSV
or binary) with notes, power modes, settings (flash or SD card), self-test, and the recordings on
the card (download, rename, duplicate, delete). A release archive has it built in `web-ble/`
(`python3 -m http.server --directory web-ble 8000`); it is a static site, so any HTTPS host can
serve it to everyone with a board. See [`examples/web/README.md`](examples/web/README.md).

The two browser front ends:

| | `examples/web` (above) | `web/` (below) |
|---|---|---|
| Boards | emotibit-ble, over Web Bluetooth | stock over WiFi (through `emotibit web`), emotibit-ble, USB serial |
| Needs | Chrome or Edge, any static host | the bridge for WiFi; nothing for demo and replay |
| SD card | recordings, settings, self-test | recording on and off, notes |
| Started by | serving `dist/` | `emotibit web` (built into the binary) |

## Browser oscilloscope

A web page cannot open UDP or raw TCP sockets, so it cannot talk to a stock EmotiBit directly. For
the stock firmware the design is therefore a **native bridge** plus a page whose decoding is the
same Rust code compiled to WebAssembly (a board running the custom BLE firmware connects to the page
directly, see below):

```
EmotiBit --WiFi--> emotibit serve --WebSocket--> browser (wasm: decode, scope, status; JS: draw)
```

```sh
emotibit web             # finds the device, serves the UI, opens your browser
```

`web` picks a free port (8765, or the next free one), connects to the device, then opens the page.
Options: `--port N`, `--no-open`, `--bind IP`, `--remote`, `--record-dir DIR`, `--lsl`/`--osc`/`--udp`
(outputs run alongside), `--allow-origin ORIGIN`. If the device drops it reconnects by itself.

The UI is **built into the binary** when `web/pkg` exists at compile time, so a release binary needs
no other files. After changing the Rust core, rebuild the wasm first:

```sh
scripts/build-web.sh && cargo build --release     # wasm core -> web/pkg, then embed it
```

Without a built UI, `web` looks for `./web`, `./dist` or `--web DIR`, and tells you to run
`scripts/build-web.sh` if none has a build. `serve --ws ADDR [--web DIR]` is the lower-level form.

The page opens on a choice of source, each a card with its own options:

| Source | Needs | Notes |
|---|---|---|
| **Bluetooth** | a board with `firmware/emotibit-ble`; Chrome or Edge | Web Bluetooth, no bridge. SD recording (CSV or binary), notes, power mode; compact or raw wire format; reconnects by itself (5 tries) if the link drops. |
| **WiFi** | `emotibit serve` / `web` | Stock firmware. Records to the SD card, notes, power mode. |
| **USB serial** | Chrome or Edge | One signal from the stock firmware's debug mode (below). |
| **Demo / recording** | nothing | Synthetic packets, or a raw recording (open or drop it) replayed on its own clock at 0.5× to 8×. |

While streaming, the header shows the device, a connection dot, battery, packet rate, lost packets
and, for Bluetooth, the link (format, ATT MTU, kB/s, records the firmware dropped). Signals are
plotted in a fixed order with time window, pause and autoscale; the X/Y/Z axes of the accelerometer,
gyroscope and magnetometer and the three SCR metrics share one plot each (untick *group axes* to
split them). **Record** works for every source: with WiFi it records on the SD card, otherwise the
page collects a raw recording in the stock format and saves it as a `.csv` when you stop. Bluetooth
and USB recordings start with a time-sync exchange, so `emotibit parse` gives wall-clock times;
any of them can be opened in the page again.

### Static site

```sh
scripts/build-web.sh --static dist     # self-contained: index.html, main.js, pkg/
(cd dist && python3 -m http.server 8080)
```

Handy links: `?demo` starts the synthetic demo, `?replay=rec.csv` loads a recording served next to
the page, `?bridge=ws://host:8765/` connects to a bridge. `dist/` can be hosted anywhere (GitHub Pages, S3, …). On a static host the page starts in a "choose
a source" state: use demo data, load a recording, or connect to a bridge with the **Bridge** field or
`?bridge=ws://host:8765/`. To let a *hosted* page control a device through your bridge, opt in to its
origin: `emotibit serve --ws … --allow-origin https://me.github.io`.

**Security.** Any web page open in your browser can attempt `ws://127.0.0.1:<port>`, and the bridge's
commands include hibernating the device. The bridge therefore binds to localhost by default and only
accepts WebSocket connections whose `Origin` is its own page (or that send no `Origin`, as non-browser
clients do) plus origins you list with `--allow-origin`. Binding to a non-loopback address prints a
warning. The same applies to the remote-control port below.

## USB serial: no WiFi, no bridge

A plain static web page **cannot** receive the WiFi stream: it needs UDP broadcast, a UDP data port and
a TCP server, and browsers allow none of those. USB is different, because browsers can open a serial
port directly (**Web Serial**, Chrome/Edge over https or `http://localhost`). So the static page has a
**USB serial…** button, and the CLI has the same thing:

```sh
emotibit serial --signal eda --seconds 10           # summary; --raw prints every value
emotibit serial --signal temp1 --verbose            # also show the board's own output (credentials hidden)
```

Signals: `eda`, `thermopile`, `temp0`, `temp1`, `accel-x`, `ppg-red`, `battery`. Plug the Feather in with a
data cable; the tool reboots it, enters the firmware's debug mode, and reads one signal.

What to expect, from testing on a Feather ESP32 (stock firmware 1.14.0):

- **It is the firmware's debug output, not a data stream.** One signal at a time, bare numbers, no
  timestamps. The firmware re-prints its recent buffer on every loop pass (about 1,300 lines/s for EDA, which
  samples at 15 Hz), so values repeat; the CLI summary rates are not sample rates and the web page
  averages values into 50 ms bins. Use WiFi (`stream`, `web`, `tui`) for real sample streams.
- **Opening the port reboots the board** (DTR/RTS reset, ESP32 Feather only); it rejoins WiFi afterwards. The
  firmware only accepts debug mode in the 2 s after boot, so a reset is how the tool gets in. The M0 Feather
  is not supported; for other boards use `--no-reset` and press Reset yourself.
- **It is not 100% reliable.** The firmware sometimes ignores debug keys for a whole boot (I could not find
  why: no crash, no late answer). The tool therefore restarts the whole handshake with another reboot when the
  stream has not started within about 45 s, up to 4 attempts. In my runs it usually started on the first
  boot and once failed all four attempts; just run it again.
- Baud rate is 2,000,000 (the firmware's `SERIAL_BAUD`); the ROM boot log is 115200 and shows as noise.
- **WiFi credentials:** the firmware prints your SSID and password from `config.txt` during boot. These tools
  replace any line mentioning an SSID or password with `[hidden: may contain WiFi credentials]` before anything
  can show or store it. If you use a serial monitor yourself, expect to see them.

The handshake is a portable state machine (`src/serial.rs`) used unchanged by the CLI and the page (wasm).
Diagnostics: `cargo run --example serial_diag -- PORT 2000000 5` reboots the board repeatedly and records
which debug keys it answers; `serial_probe` sends raw bytes.

## BLE: a custom firmware that streams everything

The stock firmware only speaks WiFi. `firmware/emotibit-ble` is a from-scratch Rust `no_std`
firmware for the EmotiBit's ESP32 Feather that streams every sensor over Bluetooth instead and
does what the stock firmware does around it: SD card recording, clock sync, notes, LSL markers,
power modes, status LEDs. Every `emotibit` command works with it (`--transport ble`), and so
does the [browser version](#browser-version-examplesweb). It replaces the stock firmware on the
board; `emotibit firmware stock` (or `restore` of a backup) puts stock back.

Reference (protocol, file formats, internals, testing):
[`firmware/emotibit-ble/README.md`](firmware/emotibit-ble/README.md).

### Getting started

```sh
emotibit firmware ble                        # install it over USB (the image is built in)
emotibit --device EmotiBit-BLE-0001010 firmware ota   # later updates: over Bluetooth
emotibit --transport ble scan                # EmotiBit-BLE-0001010  Bluetooth  available
emotibit ble --seconds 20                    # stream, then per-channel stats and the link report
emotibit selftest                            # each chip, SD card, EEPROM id, LEDs, battery: PASS/FAIL
```

To build the firmware yourself, see [Development and releases](#development-and-releases).
Updating keeps the board's settings; the SD card is never touched.

A board advertises as `EmotiBit-BLE-<serial>`, the serial number from its EEPROM
(`MD-V7-0001010` → `EmotiBit-BLE-0001010`), or a name you give it. That name is its id for
`--device`, LSL and OSC. With `--all`, several boards at once; `--transport both` mixes BLE and
WiFi boards.

### Streaming

```sh
emotibit --transport ble tui                 # terminal oscilloscope (r: record, n: note, m: power)
emotibit --transport ble stream --lsl        # LSL streams; --osc, --record-dir as over WiFi
emotibit --transport ble web                 # the bridge page in web/; or examples/web, no bridge
emotibit --transport ble serve --remote      # remote control (the Oscilloscope's UDP commands)
```

All signals arrive at the stock firmware's rates and in its units: EDA and EDL 15.6 Hz,
PPG, accelerometer, gyroscope and magnetometer 25 Hz (PPG 100 Hz if set), thermopile and TEMP1
7.5 Hz, battery, plus heart rate, inter-beat interval and SCR metrics. TEMP0/humidity only on
V2/V3 boards (with an Si7013; untested). Data carry the board's own clock.

### Recording on the SD card

```sh
emotibit --transport ble stream --record     # record while streaming (--sd-format bin: 3x smaller)
emotibit sd start                            # or record without staying connected …
emotibit sd stop                             # … and stop later
emotibit sd sessions                         # what is on the card (below)
emotibit sd get 2026-11-07_23-49-09-480343.csv --parse      # download over Bluetooth, convert
emotibit sd rename 2026-11-07_23-49-09-480343.csv "baseline (P07)"      # → baseline (P07).csv
emotibit sd copy "baseline (P07).csv" "baseline (P07) backup"
emotibit sd delete "baseline (P07) backup.csv"
```

A recording is named like the stock firmware's, after the host's local time when it started
(`2026-11-07_23-49-09-480343.csv`), with a stock-style `_info.json` beside it. CSV files hold
stock packet lines (the official DataParser reads them); binary files (`.ebb`) are smaller and
`emotibit parse` converts them. Notes (`n` in the TUI, the web page, remote control) and LSL
markers go into the file, and clock syncs every 5 s let `parse` put it on wall-clock time.
Recording continues when the host disconnects.

`sd sessions` lists the recordings with the summary the board keeps in each `_info.json`
(oldest first; the web page's **SD files…** shows the same, newest first, with buttons to
download, rename, duplicate and delete):

```
start                 duration      size  format  notes markers  state        file
–                            –    211 kB  csv         –       –  unknown      EB000001.CSV
2026-11-07 23:49:09       0:20    209 kB  csv         0       0  done         2026-11-07_23-49-09-480343.csv
2026-11-07 23:52:02       0:28    311 kB  csv         2       0  done         2026-11-07_23-52-02-104000.csv
```

`state` is `recording` for the file being written (live numbers), `done`, `interrupted` (power
lost or reset before the end; the file holds what was written), or `unknown` for files without
a session summary (older firmware). The start comes from the `_info.json`, so it survives a
rename. Downloads run at about 5 kB/s and not during a recording.

`rename`, `copy` and `delete` act on a whole recording, named by its data file as `sd sessions`
shows it: the data file and its `_info.json` together (the copy's and the renamed info file's
`data_file` point at the new name). New names take letters, digits, space, `-`, `_`, `.`, `(`
and `)`, up to 36 characters; the extension stays. Copies run on the card at about 300 kB/s
(6.7 MB in 20 s), with progress. None of them runs during a recording, and names are not
case-sensitive (FAT). `sd list` shows every file; `sd put` writes an 8.3-named file and `sd rm`
deletes any single file (not `CONFIG.TXT`).

### Board settings

```sh
emotibit config                                 # every setting, the matching profile, where they come from
emotibit config profiles                        # the settings and profiles (no board needed)
emotibit config set profile=balanced --restart  # a profile (its CPU clock applies after the restart)
emotibit config set profile=low_energy leds=1   # a profile, with the LEDs on anyway
emotibit config set ppg_hz=100 leds=0           # apply at once
emotibit config set tx_dbm=0 conn_ms=60         # radio: transmit power, connection interval
emotibit config set name=lab3 --restart         # advertise as EmotiBit-BLE-lab3 after a restart
emotibit config set ppg_hz=100 --store sd       # keep them in the card's CONFIG.TXT (or both)
emotibit config clear --store both              # back to defaults at the next restart
```

Besides the name, LEDs and PPG rate, the firmware has settings that trade battery life against
rate, noise and range: LED brightness, CPU clock (80, 160 or 240 MHz), how often the sensor task
wakes, the Bluetooth connection and advertising intervals, transmit power and the PPG LED
current. Profiles set them together:

| Profile | For |
|---|---|
| `default` | as shipped: everything as fast as it goes (240 MHz, sensors every 2 ms, 15 ms link, the radio's +9 dBm) |
| `high_frequency` | PPG at 100 Hz, fastest CPU and link |
| `low_noise` | every EDA conversion averaged, a brighter PPG LED, status LEDs off |
| `balanced` | the stock firmware's 80 MHz; EDA still at every conversion; a 30 ms link at 0 dBm |
| `energy_saving` | 80 MHz, sensors every 10 ms, a 60 ms link, slow advertising, dim LEDs |
| `low_energy` | sensors every 20 ms, a 120 ms link at −6 dBm, a dimmer PPG LED, LEDs off |

The values are in [firmware/emotibit-ble/README.md](firmware/emotibit-ble/README.md#settings).
Power use has not been measured yet. To compare, put a meter between the battery and the board;
a USB meter won't do, since on USB the board charges its battery.

Settings are kept in the ESP32's flash (no card needed), in the card's `CONFIG.TXT`, or both;
the card wins at boot. The firmware edits only its own `EmotiBitBle` section of `CONFIG.TXT`,
keeps the rest (the stock firmware's WiFi credentials) as it was, and saves the original as
`CONFIG.BAK` first. The web page has the same in **Settings…**.

### Power, button and LEDs

`emotibit power low|max-low|wireless-off|hibernate|normal` (or the TUI's `m`, the web page):
*low* stops streaming but keeps recording, status and clock sync; *max-low* also slows the link;
*wireless-off* drops the link and stops advertising; *hibernate* closes the recording and
deep-sleeps. As on the stock firmware, a short button press toggles wireless off/normal, a 3 s
press hibernates, and a press wakes it. The LEDs follow the stock firmware: red blinks while
recording, blue is on while connected and blinks while advertising, yellow means battery below
20%, all three light while the button is held.

### What was tested

On a V7 and a V6 board against a Mac (macOS, `webbluetooth`/CoreBluetooth):

- 125-133 records/s with 0 lost over 10-minute runs, SD recording on, no corrupted samples;
- two BLE boards at once, and a BLE and a WiFi board at once, with SD recordings whose
  wall-clock times agree within 35 ms after `parse`;
- a 10-minute SD recording downloaded and parsed complete (600 s, no gaps, 120 clock syncs);
- `sd rename` / `copy` / `delete` on CSV, binary and info-less recordings (copies byte-identical,
  info files updated, nothing left behind); the same operations on a FAT32 image pass `fsck_msdos`;
- notes, LSL markers, power modes, remote control, settings (flash and card), self-test, the TUI,
  both web pages in Chrome;
- `firmware`: info, backup, `ble` over itself (settings kept), BLE → `stock` (settings area
  erased) → `restore` of the backup (the board back as it was, card untouched);
- the host tools build and pass their tests on Linux (x86_64), and pass clippy for Windows.

Not tested: the button (nobody at the board), the LEDs' light itself, the Si7013, a stock
`CONFIG.TXT` edited on a real card (the merge is unit-tested).

### Limits

- Heart rate and SCR are this project's detectors, similar to the stock firmware's, not identical;
  an unworn sensor yields about 3 false beats a minute.
- About 1 in 900 TEMP1 reads is wrong on the test boards; the firmware drops them.
- No WiFi in this firmware, and no factory-test mode (the self-test covers the board checks).
- Downloads over Bluetooth are slow (5 kB/s): for big files, read the card in a computer.
- Don't open the board's serial port while it runs: that resets the Feather.

## Remote control

```sh
emotibit serve --remote                # UDP 3130, the Oscilloscope's control port
echo '{"version":0,"target":"WIFI_HOST","action":["RECORD_BEGIN"]}' | nc -u -w1 127.0.0.1 3130
```

Actions, as in the Oscilloscope: `EMOTIBIT_CONNECT <id>`, `EMOTIBIT_DISCONNECT`, `RECORD_BEGIN`,
`RECORD_END`, `USER_NOTE <text>`. Extension: `SET_POWER_MODE <normal|low|max-low|wireless-off|hibernate>`.
`serve` reconnects automatically if the device drops. Works the same with a BLE board
(`emotibit --transport ble serve --remote`).

## Library

```rust
use emotibit::prelude::*;

let client = EmotiBitClient::new(EmotiBitClientConfig::default());
let (mut rx, handle) = client.connect().await?;      // or client.connect_all() for every device
while let Some(ev) = rx.recv().await {
    match ev {
        EmotiBitEvent::Sensor(r) => println!("{}: {:?}", r.channel.name, r.samples),
        EmotiBitEvent::Status(s) => println!("battery {:?}%", s.battery_percent),
        _ => {}
    }
}
handle.record_begin().await?;   // also: record_end, user_note, set_power_mode, subscribe_raw, status
```

The **portable core** (`protocol`, `types`, `parse`, `status`, `health`, `periodizer`, `timesync`,
`scope`) builds for `wasm32-unknown-unknown`; networking, files, outputs, the TUI and the bridge are
native-only. Features: `serial` (USB serial; included by `flash`), `flash`, `tui`, `lsl`, `osc`, `ws` (all default); `default-features = false`
gives the core and client only. The wire protocol is documented in `src/protocol.rs`.

## Firmware: install, back up, restore

`emotibit firmware …` (or `emotibit-flash …`, the same commands) works over USB with the
Feather ESP32 Huzzah under the EmotiBit (CP210x USB bridge). The Feather M0 is not supported.

```sh
emotibit firmware ports                      # USB serial ports; the Feather shows as CP210x
emotibit firmware info                       # read-only: chip, MAC, flash, partitions, installed firmware
emotibit firmware backup                     # whole flash → emotibit-backup-<MAC>-<date>.bin (about 1 min)
emotibit firmware ble                        # install emotibit-ble (built in; --image FILE for another build)
emotibit --device EmotiBit-BLE-0001010 firmware ota   # update emotibit-ble over Bluetooth, no USB
emotibit firmware stock                      # install the stock firmware from the official installer's files
emotibit firmware write FILE[@ADDRESS]... # any ESP32 image
emotibit firmware restore BACKUP.bin         # put a backup back, all of it
```

Every writing command prints its plan first and asks before writing; `--dry-run` only checks the
files and prints the plan, `-y` skips the question. Options: `-p/--port` (default: the one
CP210x port), `-b/--baud` (default 921600; try 460800 if it fails). Writes are verified. The
board restarts into its firmware afterwards.

| Command | Writes | Settings (`nvs`) |
|---|---|---|
| `ble` | bootloader, partition table, app, from a merged image; resets which app slot starts | kept on an update; erased when coming from another firmware |
| `ota` | over Bluetooth: the app, into the board's other app slot | kept |
| `stock` | the official installer's layout: bootloader@0x1000, partitions@0x8000, boot_app0@0xe000, app@0x10000 | same rule |
| `write` | each file where it belongs: a merged image at 0, an app at 0x10000, a partition table at 0x8000; or `FILE@0x…` | untouched unless an image covers them |
| `restore` | the whole file from address 0 | as in the backup |

`info` names the installed firmware from its app descriptor: `emotibit-ble 0.1.1 (Bluetooth)`, or
"an Arduino sketch" for the stock firmware (an Arduino build, which does not name itself), and
with update slots, which one starts.

**Updates over Bluetooth.** `firmware ota` needs no USB: the board writes the new app into its
other app slot, checks it, and restarts into it; an interrupted or failed update changes
nothing. It takes about a minute (two from firmware before the faster transfer). Pick the board with `--device` (without it, the only board in
range); `--dry-run` checks the image and finds the board; `-y` skips the question. Boards
running the first release need one `firmware ble` over USB first, for the slots (their
settings are kept). Details in the
[firmware README](firmware/emotibit-ble/README.md#updates-over-bluetooth).

**Stock firmware files.** The bootloader, partition table and `boot_app0` come only with the
official EmotiBit software ([ofxEmotiBit releases](https://github.com/EmotiBit/ofxEmotiBit/releases)).
`scripts/cli.sh download` fetches the macOS version into `.emotibit/software/` (the default);
on another OS, extract the zip and pass its folder: `firmware stock -f ~/Downloads/EmotiBitSoftware`
(or set `EMOTIBIT_FIRMWARE_DIR`). The folder is searched for `esp32/boot_app0.bin`. Other stock
builds, such as the 100 Hz PPG variant from
[EmotiBit_FeatherWing releases](https://github.com/EmotiBit/EmotiBit_FeatherWing/releases), go in
with `--app EmotiBit_stock_firmware_PPG_100Hz.feather_esp32.bin`.

**Backups** are the full 4 MB flash, checked against the board's MD5, saved with owner-only
permissions. They can hold WiFi settings saved by the stock firmware: keep them private.

**Other images.** Make an ELF into an image with `espflash save-image --chip esp32 --merge app.elf
image.bin`. `firmware/emotibit-ble.bin` is such a merged image, so `esptool.py write-flash 0
firmware/emotibit-ble.bin` installs it on a machine without this tool (erasing the settings).

Before writing: HIB switch **not** on HIB, boards stacked, battery connected, the installer and
serial monitors closed, the EmotiBit not worn while on USB. For the stock firmware, the SD card
with `config.txt` in the EmotiBit (`scripts/cli.sh all`): after flashing, a blinking blue LED
means WiFi is connected, solid blue means check the credentials, red means check `config.txt`.

## Helper script

`scripts/cli.sh` prepares the SD card (`configure` writes `config.txt`; WiFi secrets are never
logged), downloads the official software, and opens the Oscilloscope. See `scripts/cli.sh --help`.

## Notes and limits

- **Sequence gaps** (`info`, TUI, web): the device numbers packets in one sequence across all
  signals. A stock device still skips a few numbers per clock-sync exchange (roughly every 5 s) —
  numbered packets the host never receives — so a small steady count is normal; extra, irregular
  gaps indicate real loss.
- The device sends the `EM` mode packet over both UDP and TCP; the client drops the duplicate.
- **Power modes:** low-power modes can stop the data stream and hibernate powers the device down
  until its button is pressed; `power hibernate` needs `--yes`, and the TUI never offers it.
- Units for EDL, EDR, MAG, PPG and the SCR metrics are not documented in the protocol sources, so
  they are left blank rather than guessed.

## Development and releases

```sh
cargo test && cargo clippy --all-targets      # host: library, CLI, flasher, shared firmware modules
examples/web/build.sh                         # the Web Bluetooth page → examples/web/dist
scripts/build-web.sh                          # the bridge page → web/pkg (embedded by the next cargo build)
```

**The firmware** needs the Xtensa Rust toolchain, pinned in
`firmware/emotibit-ble/rust-toolchain.toml` as `esp195` (Xtensa Rust 1.97), and espflash:

```sh
cargo install espup espflash
espup install --toolchain-version 1.97.0.0 --name esp195
scripts/ble-firmware.sh                       # build → firmware/emotibit-ble.bin (merged image)
scripts/ble-firmware.sh flash                 # build and install (keeps the board's settings)
cargo build --release                         # re-embed the new image in emotibit
```

`scripts/ble-firmware.sh` sources `~/export-esp.sh` (or `$ESP_EXPORT`). The firmware's
reference, including its patched dependencies and how it was tested, is in
[`firmware/emotibit-ble/README.md`](firmware/emotibit-ble/README.md).

**A release:**

1. Rebuild the firmware image if the firmware changed (`scripts/ble-firmware.sh`) and test it.
2. Set the version in `Cargo.toml` (and `firmware/emotibit-ble/Cargo.toml`) and give it a
   section in `CHANGELOG.md`. `cargo publish --dry-run` checks that the crate packages and
   builds on its own (it carries `src/shared/` and `firmware/emotibit-ble.bin`).
3. Tag it: `git tag v0.1.1 && git push origin v0.1.1`. `.github/workflows/release.yml` builds the archives
   for macOS (arm64, x86_64), Linux (x86_64) and Windows (x86_64) and attaches them, with
   `SHA256SUMS`, to a GitHub release. `ci.yml` checks every push: clippy, tests, the wasm page,
   and the firmware build.

`scripts/release.sh [--firmware] [--target TRIPLE]` builds the same archives locally, into
`release/`, with the `dist` profile (fat LTO, one codegen unit, stripped: about 40% smaller
binaries than `--release`, for several times the build time). The web pages build with the `web`
profile (`opt-level = "z"`) and `wasm-opt -Oz` if binaryen is installed: 346 KB instead of 785 KB. Each holds the binaries (the BLE image and, with wasm-pack installed, the bridge
page built in), `firmware/emotibit-ble.bin`, the Web Bluetooth page (`web-ble/`), the docs and
the licence; `emotibit-web-ble-<version>.zip` is the page alone, ready for any HTTPS host.

## Troubleshooting

- `scan` finds nothing: same 2.4 GHz network? Some networks isolate clients; try `--ip <device IP>`.
- Device shown as *in use*: the Oscilloscope or another host is connected; close it.
- `serve --remote` fails to bind: the Oscilloscope is already using UDP 3130.
- Flash fails: press Reset, replug USB, check HIB, avoid USB hubs, lower `--baud`; close
  anything holding the port (the installer, `espflash monitor`, a serial monitor).
- A BLE board is not found right after flashing: give it a few seconds to boot; macOS keeps a
  board's Bluetooth details cached by address, which `emotibit-ble` changes with its GATT layout.
- `firmware stock` cannot find the files: pass the extracted EmotiBit software with `-f DIR`.
- `emotibit web` says the UI is not built: run `scripts/build-web.sh`, then rebuild.
