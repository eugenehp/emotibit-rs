# Browser version

The same library the command line and the TUI drive, running in a browser and talking to a
board with `firmware/emotibit-ble` over the browser's own Web Bluetooth. Connect from a button,
watch every signal, record on the board's SD card, change its settings, download its files.

Nothing is re-implemented for the browser. The BLE client (connect, wire formats, conversion to
stock units, heart rate and SCR, clock sync, the SD card protocol, settings) is `emotibit`
compiled to `wasm32-unknown-unknown`, on the same `webbluetooth` layer the native build uses
(as in `muse-rs`). The page is a canvas, a worker and some glue.

## Running it

```sh
rustup target add wasm32-unknown-unknown   # once
examples/web/build.sh
python3 -m http.server --directory examples/web/dist 8000
```

Then open <http://localhost:8000/> in Chrome or Edge and press **Connect a board…**.
Web Bluetooth needs Chrome or Edge (Safari and Firefox do not implement it) and a secure
context: `localhost` or HTTPS, not `file://`.

From a release archive, no build: `python3 -m http.server --directory web-ble 8000`.

## Hosting it

The page is five static files and talks only to the board, so any HTTPS host serves it: GitHub
Pages, Netlify, S3, an nginx `location`. Upload `dist/` (or the release's
`emotibit-web-ble-<version>.zip`, unpacked). Nothing needs configuring (a server that
sends `.wasm` without `application/wasm` only makes loading a little slower: the shim falls back). Everyone who opens the page
picks their own board in the browser's chooser; nothing passes through the host.

## What it does

| | |
|---|---|
| Plots | every signal, grouped (X/Y/Z of each motion sensor overlaid, the SCR metrics together), 2-60 s window, pause; heart rate and SCR drawn as held steps |
| Header | board, battery, samples per second, records lost, link (wire format, ATT MTU, records the firmware dropped), clipping/overflow (`DC`/`DO`), SD recording |
| Recording | on the board's SD card, CSV (stock format) or binary; notes into the file |
| Power | normal, low, max-low, wireless off, hibernate |
| Settings… | name, PPG rate, LEDs; kept in the board's flash, its SD card, or both |
| Self-test | each chip, the SD card, the EEPROM id, the LEDs, the battery |
| SD files… | the recordings on the card, newest first: start, duration, size, format, notes, LSL markers, and state (`recording` live, `done`, `interrupted`, `unknown` for older files); download a recording's data or its `_info.json` (about 5 kB/s, not while recording); rename, duplicate (on the card, about 300 kB/s) or delete a recording, its `_info.json` with it; the other files under a fold, each can be deleted (`CONFIG.*` are not downloaded, `CONFIG.TXT` not deleted) |

Tested in Chrome on macOS against a V7 board: 340-355 samples/s, 0 lost, self-test, settings,
an SD recording with a note, the file list.

## What `build.sh` produces

`dist/` (git-ignored):

| File | From |
|---|---|
| `web.wasm` | `cargo build --example web --target wasm32-unknown-unknown --release --no-default-features --features ble` |
| `index.html`, `app.js`, `render.js` | this directory |
| `webbluetooth.js` | copied out of the resolved `webbluetooth-wasm` crate |

The shim is not vendored: it is the other half of the module's ABI, so it is taken from the
`webbluetooth-wasm` the lock file resolved and cannot drift from the `.wasm` beside it.

## Two threads

| | Main thread (`app.js`) | Worker (`render.js`) |
|---|---|---|
| Web Bluetooth and the wasm module | ✓ | |
| Sample buffers, autoscaling, every canvas call | | ✓ |
| Status chips, panels, downloads | ✓ | |

`navigator.bluetooth` exists on `Window`, not in workers, and `requestDevice` needs a user
gesture, so the module stays on the main thread. The canvas is handed to the worker with
`transferControlToOffscreen()`, and samples cross once per frame as typed arrays.

## The boundary

No `wasm-bindgen`: plain numbers and `(pointer, length)` strings, like `webbluetooth-wasm`.

**Exports** (`lib.rs`): `eb_connect(compact)` (from a click: it opens the chooser),
`eb_disconnect()`, `eb_command(ptr, len)` (tab-separated tag and fields: `RB`, `RE`, `UN`,
`MN`…`MH`, `CG`, `CS`, `CR`, `ST`, `FS`, `FL`, `FR`, `FY`, `FZ`, `FX`; the host's local time is added where the protocol
wants it; the commands are listed in
[`firmware/emotibit-ble/README.md`](../../firmware/emotibit-ble/README.md#commands-and-replies)), `eb_sd_get(ptr, len)`, and `eb_alloc` / `eb_free` for the strings.

**Imports** (namespace `emotibit`, from `app.js`): `eb_series` and `eb_sample` (plots),
`eb_state` (status JSON), `eb_message` (other packet lines), `eb_file` and `eb_progress`
(downloads), `eb_status`, `eb_device`, `eb_active`, `eb_log`, and the clock for
`emotibit::platform`: `eb_now_ms` (`Date.now()`) and `eb_tz_offset_min`.

Sample times come from the board's clock (each notification carries it), mapped onto the page's
clock with the smallest offset seen, so jitter in delivery does not bunch the traces.
