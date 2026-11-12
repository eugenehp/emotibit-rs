#!/usr/bin/env bash
# Build the browser UI.
#
#   scripts/build-web.sh                 compile the wasm core into web/pkg
#   scripts/build-web.sh --static DIR    also assemble a self-contained static site in DIR
#                                        (host it anywhere: GitHub Pages, S3, `python3 -m http.server`)
#
# The static site works on its own in demo and replay modes; to see a live device, point it
# at a running bridge (`emotibit serve`) with the "Bridge" field or `?bridge=ws://host:port`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

static_dir=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --static) static_dir="${2:?--static needs a directory}"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "error: unknown option $1" >&2; exit 1 ;;
  esac
done

command -v wasm-pack >/dev/null || { echo "error: wasm-pack is required (cargo install wasm-pack)" >&2; exit 1; }
wasm-pack build --target web --release --out-dir web/pkg --out-name emotibit -- --no-default-features --features webscope
rm -f web/pkg/.gitignore web/pkg/package.json web/pkg/README.md

if [[ -n "$static_dir" ]]; then
  rm -rf "$static_dir"
  mkdir -p "$static_dir/pkg"
  cp web/index.html web/main.js "$static_dir/"
  cp web/pkg/emotibit.js web/pkg/emotibit_bg.wasm "$static_dir/pkg/"
  touch "$static_dir/.nojekyll"
  echo "Static site written to $static_dir"
  echo "  try it:  (cd $static_dir && python3 -m http.server 8080)  then open http://127.0.0.1:8080/"
else
  echo "Built web/pkg. Serve it with:"
  echo "  emotibit serve --ws 127.0.0.1:8765 --web web     then open http://127.0.0.1:8765/"
fi
