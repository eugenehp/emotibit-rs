#!/usr/bin/env bash
# Battery drain comparison: for each profile and SD format, set the profile (restart for its CPU
# clock), stream for MINUTES while the board records to its card and this computer keeps a raw log
# with per-signal CSVs. Run the board on its battery (USB unplugged: on USB it charges).
#
#   scripts/power-run.sh EmotiBit-BLE-0001010 [MINUTES] [OUT_DIR]
#
# Then: scripts/power-report.py OUT_DIR
#
# To compare modes fairly, alternate them over many rounds from a full battery, so each sees
# every part of the discharge curve:
#
#   PROFILES="default balanced low_energy" FORMATS=csv ROUNDS=99 MIN_BATTERY=20 \
#     scripts/power-run.sh EmotiBit-BLE-0001010 10
set -u
DEVICE=${1:?device name, e.g. EmotiBit-BLE-0001010}
MINUTES=${2:-15}
OUT=${3:-power-run-$(date +%Y%m%d-%H%M%S)}
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
BIN="$ROOT/target/release/emotibit"
PROFILES=${PROFILES:-"default high_frequency low_noise balanced energy_saving low_energy"}
FORMATS=${FORMATS:-"csv bin"}
MIN_BATTERY=${MIN_BATTERY:-15}
ROUNDS=${ROUNDS:-1}
mkdir -p "$OUT"

e() { "$BIN" --device "$DEVICE" --timeout 15 "$@"; }

# Last battery percent the board reported in a run's output.
battery() { grep -o 'battery [0-9]*%' "$1" | tail -1 | tr -dc '0-9'; }

round=0
while [ $round -lt "$ROUNDS" ]; do
round=$((round + 1))
for profile in $PROFILES; do
  for format in $FORMATS; do
    if [ "$ROUNDS" -gt 1 ]; then run="$OUT/r$(printf %02d $round)-$profile-$format"; else run="$OUT/$profile-$format"; fi
    mkdir -p "$run"
    echo "== $(date '+%H:%M:%S') round $round, $profile, SD $format, $MINUTES min"
    tries=0
    until e config set "profile=$profile" --restart > "$run/config.txt" 2>&1 && grep -q "Restarting" "$run/config.txt"; do
      tries=$((tries + 1))
      [ $tries -ge 5 ] && { echo "   could not set the profile; stopping"; exit 1; }
      sleep 5
    done
    sleep 8
    e config show > "$run/settings.txt" 2>&1
    date +%s > "$run/start"
    # After a restart the board is sometimes not found in time; try again rather than lose the run.
    for try in 1 2 3; do
      e ble --seconds $((MINUTES * 60)) --record --sd-format "$format" --record-dir "$run/host" > "$run/stream.txt" 2>&1
      grep -q "records in" "$run/stream.txt" && break
      echo "   no stream (try $try): $(tail -1 "$run/stream.txt")"
      date +%s > "$run/start"
      sleep 10
    done
    date +%s > "$run/end"
    grep -E "records in|dropped|SD card file" "$run/stream.txt" | sed 's/^/   /'
    pct=$(battery "$run/stream.txt")
    echo "   battery ${pct:-?}%"
    if [ -n "$pct" ] && [ "$pct" -lt "$MIN_BATTERY" ]; then
      echo "   battery below $MIN_BATTERY%; stopping"
      e config set profile=default --restart > /dev/null 2>&1
      exit 0
    fi
  done
done
done
e config set profile=default --restart > /dev/null 2>&1
echo "done: $OUT"
