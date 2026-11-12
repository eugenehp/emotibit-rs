#!/usr/bin/env bash
# Builds the emotibit-ble firmware into firmware/emotibit-ble.bin (a merged image: bootloader,
# partition table, app), and optionally installs it.
#
#   scripts/ble-firmware.sh              # build the image
#   scripts/ble-firmware.sh flash [PORT] # build, then install it (keeps the board's settings)
#
# Needs the Xtensa Rust toolchain (espup install) and espflash (cargo install espflash).
# The host tools embed firmware/emotibit-ble.bin when they are built, so rebuild them after this
# (cargo build --release) for `emotibit firmware ble` to install the new image.
set -euo pipefail
cd "$(dirname "$0")/.."

for f in "${ESP_EXPORT:-}" "$HOME/export-esp.sh" "$HOME/export-esp195.sh"; do
  if [[ -n "$f" && -f "$f" ]]; then
    # shellcheck disable=SC1090
    source "$f" >/dev/null
    break
  fi
done
command -v espflash >/dev/null || { echo "espflash not found: cargo install espflash" >&2; exit 1; }

(cd firmware/emotibit-ble && cargo build --release)
ELF=firmware/emotibit-ble/target/xtensa-esp32-none-elf/release/emotibit-ble
espflash save-image --chip esp32 --merge "$ELF" firmware/emotibit-ble.bin >/dev/null
echo "image: firmware/emotibit-ble.bin"

if [[ "${1:-}" == flash ]]; then
  PORT=${2:-}
  cargo run -q --release --bin emotibit -- firmware ble --image firmware/emotibit-ble.bin ${PORT:+--port "$PORT"} --yes
fi
