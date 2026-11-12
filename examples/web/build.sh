#!/bin/sh
# Build the browser version and lay out a directory to serve (as muse-rs does).
#
# dist/ gets five files: the module, the page, its glue, the drawing worker and the shim. The
# shim is not vendored: it is copied out of whichever `webbluetooth-wasm` the lock file resolved
# to, so it cannot drift from the ABI the module was just built against.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "$here/../.." && pwd)
dist="$here/dist"

# python3, or python where that is its name (Windows).
py=$(command -v python3 || command -v python)

cargo build --manifest-path "$root/Cargo.toml" --example web \
  --target wasm32-unknown-unknown --release --no-default-features --features ble

shim=$(
  cargo metadata --manifest-path "$root/Cargo.toml" --format-version 1 \
    --filter-platform wasm32-unknown-unknown |
    "$py" -c 'import json, os, sys
packages = json.load(sys.stdin)["packages"]
backend = next(p for p in packages if p["name"] == "webbluetooth-wasm")
print(os.path.join(os.path.dirname(backend["manifest_path"]), "js", "webbluetooth.js"))'
)

mkdir -p "$dist"
cp "$root/target/wasm32-unknown-unknown/release/examples/web.wasm" "$dist/web.wasm"
cp "$shim" "$dist/webbluetooth.js"
cp "$here/index.html" "$here/app.js" "$here/render.js" "$dist/"

cat <<MSG

Built $dist:
$(cd "$dist" && ls -1 | sed 's/^/  /')

Web Bluetooth needs a secure context, so serve it over localhost:

  python3 -m http.server --directory $dist 8000

then open http://localhost:8000/ in Chrome or Edge.
MSG
