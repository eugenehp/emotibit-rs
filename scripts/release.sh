#!/usr/bin/env bash
# Builds the release archives for this machine into release/.
#
#   scripts/release.sh                     # host tools for this machine + the BLE web page
#   scripts/release.sh --firmware          # rebuild firmware/emotibit-ble.bin first (needs espup)
#   scripts/release.sh --target TRIPLE     # cross-compile the host tools (rustup target add TRIPLE)
#
# Produces, with <v> the crate version and <t> the target triple:
#   release/emotibit-<v>-<t>.tar.gz (.zip on Windows)
#       emotibit, emotibit-flash     the command line, TUI, bridge, and the flasher; both have the
#                                       emotibit-ble image (and the bridge page, if built) inside
#       firmware/emotibit-ble.bin       the BLE firmware, for other flashers (esptool write-flash 0)
#       web-ble/                        the Web Bluetooth page, a static site
#       README.md, CHANGELOG.md, LICENSE, docs/
#   release/emotibit-web-ble-<v>.zip    the Web Bluetooth page alone, to host on any HTTPS server
#   release/SHA256SUMS
#
# Needs: Rust, the wasm32-unknown-unknown target, python3; wasm-pack for the bridge page (optional:
# without it `emotibit web` needs --web DIR).
set -euo pipefail
cd "$(dirname "$0")/.."

firmware=0
target=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --firmware) firmware=1; shift ;;
    --target) target="${2:?--target needs a triple}"; shift 2 ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "error: unknown option $1" >&2; exit 1 ;;
  esac
done

version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
host=$(rustc -vV | sed -n 's/^host: //p')
triple=${target:-$host}
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) windows=1 ;; *) windows=0 ;; esac
[[ "$triple" == *windows* ]] && exe=.exe || exe=""

py=$(command -v python3 || command -v python)
step() { printf '\n== %s\n' "$*"; }

if [[ $firmware == 1 ]]; then
  step "BLE firmware"
  scripts/ble-firmware.sh
fi
[[ -f firmware/emotibit-ble.bin ]] || { echo "error: firmware/emotibit-ble.bin is missing (run with --firmware)" >&2; exit 1; }

step "Bridge page (embedded in emotibit)"
if command -v wasm-pack >/dev/null; then
  scripts/build-web.sh
else
  echo "wasm-pack not found: skipped; \`emotibit web\` will need --web DIR"
fi

step "Web Bluetooth page"
rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown || rustup target add wasm32-unknown-unknown
examples/web/build.sh >/dev/null
echo "examples/web/dist"

step "Host tools ($triple)"
cargo build --release --locked --bins ${target:+--target "$target"}
bin_dir=target/${target:+$target/}release

step "Archives"
name="emotibit-$version-$triple"
stage="release/$name"
rm -rf "$stage" "release/emotibit-web-ble-$version.zip"
mkdir -p "$stage/firmware" "$stage/web-ble" "$stage/docs"
cp "$bin_dir/emotibit$exe" "$bin_dir/emotibit-flash$exe" "$stage/"
cp firmware/emotibit-ble.bin "$stage/firmware/"
cp firmware/emotibit-ble/README.md "$stage/docs/firmware-emotibit-ble.md"
cp examples/web/README.md "$stage/docs/web-ble.md"
cp examples/web/dist/* "$stage/web-ble/"
cp README.md CHANGELOG.md LICENSE "$stage/"

(
  cd release
  if [[ $windows == 1 ]]; then
    powershell -NoProfile -Command "Compress-Archive -Force -Path '$name' -DestinationPath '$name.zip'"
    archive="$name.zip"
  else
    tar -czf "$name.tar.gz" "$name"
    archive="$name.tar.gz"
  fi
  (cd "$name/web-ble" && "$py" -I -c 'import sys, zipfile, os
with zipfile.ZipFile(sys.argv[1], "w", zipfile.ZIP_DEFLATED) as z:
    for f in sorted(os.listdir(".")):
        z.write(f)' "../../emotibit-web-ble-$version.zip")
  if command -v sha256sum >/dev/null; then sum="sha256sum"; else sum="shasum -a 256"; fi
  $sum "$archive" > "SHA256SUMS-$triple"
  $sum "emotibit-web-ble-$version.zip" > SHA256SUMS-web
  cat SHA256SUMS-* > SHA256SUMS
  echo "release/$archive"
  echo "release/emotibit-web-ble-$version.zip"
  echo "release/SHA256SUMS"
)
