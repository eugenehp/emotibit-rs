#!/usr/bin/env bash
# EmotiBit SD card helper: writes config.txt (WiFi credentials) to the card.
#
# Privacy: SSIDs and passwords are never written to logs, temp files, argv or
# command output. They are entered/selected interactively, held in memory, and
# written only to config.txt on the SD card. The one exception is the network
# picker, which must show saved network names on your terminal so you can choose.
set -euo pipefail
set +x

VOLUME_NAME="${EMOTIBIT_VOLUME:-FLASHDEVICE}"
CONFIG_NAME="config.txt"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EMOTIBIT_HOME="${EMOTIBIT_HOME:-$SCRIPT_DIR/../.emotibit}"
RELEASE_REPO="EmotiBit/ofxEmotiBit"

usage() {
  cat <<EOF
Usage: $(basename "$0") <command> [options]

SD card:
  status       Show the SD card volume and the shape of config.txt (no secrets)
  configure    Interactively write config.txt
  verify       Validate config.txt on the card
  clean        Remove macOS metadata folders (.fseventsd, .Spotlight-V100, ...)
  eject        Unmount the card safely
  all          configure + verify + clean + eject

Software and device:
  download     Download + extract the official EmotiBit macOS software (latest release)
  tools        Install flashing tools: esptool (ESP32, local venv) + bossac (M0, Homebrew)
  chip         Read the connected board's chip info with esptool (read-only)
  ports        List USB serial ports and guess the Feather type
  flash        Pre-flight checklist, then open the Firmware Installer
  oscilloscope Open the EmotiBit Oscilloscope (live data + recording)
  parser       Open the EmotiBit DataParser
  doctor       Check everything: card, config, software, USB, WiFi security

Options:
  -v, --volume NAME   Volume name (default: $VOLUME_NAME, env EMOTIBIT_VOLUME)
  -y, --yes           Overwrite an existing config.txt without asking (configure/all)
  -h, --help          Show this help

For each network, configure lets you choose:
  1) this Mac's current WiFi   2) one of this Mac's saved networks   3) type it in
Mac passwords are read from Keychain (macOS asks you to approve).
Software is stored in \$EMOTIBIT_HOME (default: <repo>/.emotibit).
EOF
}

die() { echo "error: $*" >&2; exit 1; }

mount_point() { echo "/Volumes/$VOLUME_NAME"; }

require_volume() {
  [[ -d "$(mount_point)" ]] || die "volume '$VOLUME_NAME' is not mounted"
  local fs
  fs="$(diskutil info "$(mount_point)" 2>/dev/null | awk -F: '/File System Personality/ {gsub(/^ +/,"",$2); print $2}')"
  [[ "$fs" == *FAT* ]] || die "volume is '$fs', EmotiBit needs FAT32"
}

# ---------- WiFi sources (all output of secrets goes to stdout for capture only) ----------

wifi_device() {
  networksetup -listallhardwareports | awk '/Hardware Port: Wi-Fi/ {getline; print $2; exit}'
}

current_ssid() {
  local dev out
  dev="$(wifi_device)"; [[ -n "$dev" ]] || return 0
  out="$(networksetup -getairportnetwork "$dev" 2>/dev/null || true)"
  if [[ "$out" == "Current Wi-Fi Network: "* ]]; then
    echo "${out#Current Wi-Fi Network: }"; return 0
  fi
  # macOS 26 reports "<redacted>" unless the app has Location access: treat as unknown.
  local v; v="$(ipconfig getsummary "$dev" 2>/dev/null | awk -F' : ' '/^ +SSID : / {print $2; exit}')"
  [[ "$v" == "<redacted>" ]] || echo "$v"
}

# Interactive picker; UI goes to /dev/tty so it never lands in captured output.
pick_saved_ssid() {
  local dev list=() line i n
  dev="$(wifi_device)"; [[ -n "$dev" ]] || die "no Wi-Fi interface found"
  while IFS= read -r line; do list+=("$line"); done < <(
    networksetup -listpreferredwirelessnetworks "$dev" | sed '1d; s/^[[:space:]]*//')
  (( ${#list[@]} )) || die "no saved WiFi networks on this Mac"
  {
    echo "Saved WiFi networks:"
    for i in "${!list[@]}"; do echo "  $((i+1))) ${list[$i]}"; done
  } > /dev/tty
  read -r -p "Choose [1-${#list[@]}]: " n < /dev/tty > /dev/tty
  [[ "$n" =~ ^[0-9]+$ ]] && (( n >= 1 && n <= ${#list[@]} )) || die "invalid choice"
  printf '%s' "${list[$((n-1))]}"
}

# Reads a saved WiFi password from Keychain; on failure explains why (no secrets)
# and falls back to typing it in.
keychain_password() {
  local pw rc=0
  pw="$(security find-generic-password -D "AirPort network password" -wa "$1" 2>/dev/null)" || rc=$?
  if (( rc == 0 )) && [[ -n "$pw" ]]; then printf '%s' "$pw"; return; fi
  {
    echo "Could not read the password from Keychain (exit $rc)."
    case $rc in
      128) echo "  macOS did not show/complete the approval prompt. It needs the Mac's own"
           echo "  screen (not SSH/remote shell) and an unlocked login keychain. You can run:"
           echo "    security unlock-keychain ~/Library/Keychains/login.keychain-db"
           echo "  or approve it in Keychain Access > the network item > Access Control." ;;
      44)  echo "  No saved password found for that network." ;;
    esac
    echo "Falling back to typing the password."
  } >&2
  prompt_secret "Password"
}

prompt_secret() {  # prompt_secret "Label" -> stdout; asks twice to catch typos
  local a b
  while true; do
    read -r -s -p "$1: " a < /dev/tty > /dev/tty; echo > /dev/tty
    read -r -s -p "Confirm: " b < /dev/tty > /dev/tty; echo > /dev/tty
    [[ "$a" == "$b" && -n "$a" ]] && { printf '%s' "$a"; return; }
    echo "Empty or mismatched, try again." > /dev/tty
  done
}

warn_if_5ghz() {
  [[ "$1" == *5G* || "$1" == *5g* ]] && \
    echo "warning: that network's name suggests 5 GHz; EmotiBit supports 2.4 GHz only" >&2 || true
}

# Adds one network to SSIDS/PASSWORDS/USERIDS, chosen interactively.
add_network() {
  local choice ssid pw uid=""
  echo "Add a WiFi network:"
  echo "  1) This Mac's current WiFi"
  echo "  2) Pick from this Mac's saved networks"
  echo "  3) Enter manually"
  read -r -p "Choose [1-3]: " choice
  case "$choice" in
    1) ssid="$(current_ssid)"
       if [[ -z "$ssid" ]]; then
         echo "macOS hides the current network's name from scripts (or WiFi is off)."
         echo "Pick it from the saved networks instead:"
         ssid="$(pick_saved_ssid)"
       fi
       warn_if_5ghz "$ssid"
       echo "Requesting password from Keychain (approve the macOS prompt)..."
       pw="$(keychain_password "$ssid")" ;;
    2) ssid="$(pick_saved_ssid)"
       warn_if_5ghz "$ssid"
       echo "Requesting password from Keychain (approve the macOS prompt)..."
       pw="$(keychain_password "$ssid")" ;;
    3) read -r -p "SSID (2.4 GHz): " ssid
       [[ -n "$ssid" ]] || die "SSID cannot be empty"
       warn_if_5ghz "$ssid"
       pw="$(prompt_secret "Password")" ;;
    *) die "invalid choice" ;;
  esac
  local ent
  read -r -p "Is it a WPA-Enterprise network (ESP32 only)? [y/N] " ent
  if [[ "$ent" =~ ^[Yy]$ ]]; then read -r -p "Enterprise userid: " uid; fi
  SSIDS+=("$ssid"); PASSWORDS+=("$pw"); USERIDS+=("$uid")
}

# Builds the config JSON. Data is passed on stdin (NUL-separated), not argv/env.
build_json() {
  local i
  for i in "${!SSIDS[@]}"; do
    printf '%s\0%s\0%s\0' "${SSIDS[$i]}" "${PASSWORDS[$i]}" "${USERIDS[$i]}"
  done | python3 -I -c '
import json, sys
f = sys.stdin.buffer.read().decode().split("\0")[:-1]
creds = []
for i in range(0, len(f), 3):
    c = {"ssid": f[i], "password": f[i + 1]}
    if f[i + 2]:
        c["userid"] = f[i + 2]
    creds.append(c)
print(json.dumps({"WifiCredentials": creds}, indent=2))
'
}

# ---------- commands ----------

cmd_status() {
  require_volume
  diskutil info "$(mount_point)" | grep -E "Volume Name|File System Personality|Total Size|Device Node"
  echo "--- contents ---"
  ls -la "$(mount_point)"
  if [[ -f "$(mount_point)/$CONFIG_NAME" ]]; then
    echo "--- $CONFIG_NAME (values hidden) ---"
    sed -E 's/("(ssid|password|userid)": *")[^"]*"/\1****"/' "$(mount_point)/$CONFIG_NAME"
  fi
}

cmd_configure() {
  SSIDS=(); PASSWORDS=(); USERIDS=()
  local assume_yes=$1
  require_volume

  local more=y
  while [[ "$more" == y ]]; do
    add_network
    read -r -p "Add another network (e.g. a phone hotspot)? [y/N] " more
    [[ "$more" =~ ^[Yy]$ ]] && more=y || more=n
  done

  local target json
  target="$(mount_point)/$CONFIG_NAME"
  if [[ -f "$target" && "$assume_yes" -eq 0 ]]; then
    local ans
    read -r -p "$CONFIG_NAME already exists on the card. Overwrite? [y/N] " ans
    [[ "$ans" =~ ^[Yy]$ ]] || die "aborted"
  fi

  json="$(build_json)"
  printf '%s\n' "$json" | python3 -I -m json.tool >/dev/null || die "generated JSON is invalid"
  printf '%s\n' "$json" > "$target"
  sync
  unset json SSIDS PASSWORDS USERIDS
  echo "wrote $target"
}

cmd_verify() {
  require_volume
  local target; target="$(mount_point)/$CONFIG_NAME"
  [[ -f "$target" ]] || die "$CONFIG_NAME not found on the card"
  python3 -I -c '
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except Exception as e:
    sys.exit("error: invalid JSON (%s)" % type(e).__name__)
creds = data.get("WifiCredentials")
if not isinstance(creds, list) or not creds:
    sys.exit("error: WifiCredentials must be a non-empty array")
for i, c in enumerate(creds):
    for k in ("ssid", "password"):
        v = c.get(k, "")
        if not v or "YOUR_WIFI" in v:
            sys.exit("error: entry %d: %s missing or still a placeholder" % (i, k))
print("ok: %d network(s) configured" % len(creds))
' "$target"
}

cmd_clean() {
  require_volume
  local mp; mp="$(mount_point)"
  rm -rf "$mp/.fseventsd" "$mp/.Spotlight-V100" "$mp/.Trashes" 2>/dev/null || true
  dot_clean -m "$mp" 2>/dev/null || true
  touch "$mp/.metadata_never_index" 2>/dev/null || true
  echo "cleaned metadata on the card"
}

cmd_eject() {
  require_volume
  diskutil eject "$(mount_point)"
}

# ---------- software + device ----------

app_path() { echo "$EMOTIBIT_HOME/software/$1.app"; }

ESPTOOL="$EMOTIBIT_HOME/tools/venv/bin/esptool"

cmd_tools() {
  command -v python3 >/dev/null || die "python3 is required"
  if [[ ! -x "$ESPTOOL" ]]; then
    echo "Installing esptool into $EMOTIBIT_HOME/tools/venv ..."
    mkdir -p "$EMOTIBIT_HOME/tools"
    python3 -m venv "$EMOTIBIT_HOME/tools/venv"
    "$EMOTIBIT_HOME/tools/venv/bin/pip" install -q esptool
  fi
  echo "esptool: $("$ESPTOOL" version 2>&1 | tail -1)"
  if command -v bossac >/dev/null; then
    echo "bossac: installed ($(command -v bossac))"
  elif command -v brew >/dev/null; then
    echo "Installing bossac (Homebrew formula 'bossa') ..."
    brew install bossa
  else
    echo "bossac: not installed (Homebrew missing); only needed for the Feather M0"
  fi
}

cmd_chip() {
  [[ -x "$ESPTOOL" ]] || die "esptool missing; run: $(basename "$0") tools"
  local port; port="$(list_ports | awk -F'\t' '/ESP32/ {print $1; exit}')"
  [[ -n "$port" ]] || die "no ESP32 serial port found (run ports)"
  "$ESPTOOL" --port "$port" chip-id
}

cmd_download() {
  command -v curl >/dev/null || die "curl is required"
  mkdir -p "$EMOTIBIT_HOME/dl" "$EMOTIBIT_HOME/software"
  echo "Looking up the latest release of $RELEASE_REPO..."
  local info tag url
  info="$(curl -fsSL "https://api.github.com/repos/$RELEASE_REPO/releases/latest")" \
    || die "could not query GitHub releases"
  read -r tag url < <(printf '%s' "$info" | python3 -I -c '
import json, sys
r = json.load(sys.stdin)
a = [x for x in r["assets"] if x["name"].endswith("macOS.zip")]
print(r["tag_name"], a[0]["browser_download_url"] if a else "")
')
  [[ -n "$url" ]] || die "no macOS asset in the latest release"
  local zip="$EMOTIBIT_HOME/dl/EmotiBitSoftware_${tag}-macOS.zip"
  if [[ ! -f "$zip" ]]; then
    echo "Downloading $tag (~50 MB)..."
    curl -fL --progress-bar -o "$zip.part" "$url" && mv "$zip.part" "$zip"
  else
    echo "Already downloaded: $tag"
  fi
  # Extract into its own fresh directory; downloaded content is untrusted.
  local out="$EMOTIBIT_HOME/software"
  rm -rf "$out"; mkdir -p "$out"
  unzip -q "$zip" -d "$out"
  echo "$tag" > "$EMOTIBIT_HOME/VERSION"
  echo "Extracted $tag to $out"
  ls "$out"
}

# Prints "port<TAB>guess" lines for connected USB serial devices.
list_ports() {
  local d
  for d in /dev/cu.usbserial-* /dev/cu.SLAB_USBtoUART* /dev/cu.usbmodem*; do
    [[ -e "$d" ]] || continue
    case "$d" in
      *usbserial*|*SLAB*) printf '%s\tESP32 Feather (CP210x)\n' "$d" ;;
      *usbmodem*)         printf '%s\tFeather M0 / native USB\n' "$d" ;;
    esac
  done
}

cmd_ports() {
  local out; out="$(list_ports)"
  if [[ -z "$out" ]]; then
    echo "No Feather found. Check: data-capable USB cable, board powered, CP210x driver"
    echo "(installer disk: $EMOTIBIT_HOME/software/SiLabsUSBDriverDisk.dmg)."
    return 1
  fi
  echo "$out"
}

open_app() {
  local app; app="$(app_path "$1")"
  [[ -d "$app" ]] || die "$1 not found; run: $(basename "$0") download"
  open "$app"
}

cmd_flash() {
  cmd_ports || die "plug the Feather into USB first"
  cat <<EOF

Pre-flight checklist:
  [ ] SD card with config.txt is in the EmotiBit
  [ ] HIB slider is in the ACTIVE position (HIB blocks flashing)
  [ ] Boards are stacked firmly and the battery is connected
  [ ] The EmotiBit is NOT being worn while plugged into USB
EOF
  local ans; read -r -p "All done? Open the Firmware Installer? [y/N] " ans
  [[ "$ans" =~ ^[Yy]$ ]] || die "aborted"
  open_app EmotiBitFirmwareInstaller
  cat <<EOF
In the installer pick the serial port and board type, then install.
macOS may block the app: right-click > Open, or approve in System Settings > Privacy & Security.
Afterwards watch the EmotiBit LED: blinking blue = connected to WiFi; solid blue = check WiFi.
EOF
}

cmd_doctor() {
  local ok=0 bad=0
  chk() { if [[ "$1" == ok ]]; then echo "  [ok]   $2"; ok=$((ok+1)); else echo "  [fail] $2"; bad=$((bad+1)); fi; }
  echo "SD card"
  if [[ -d "$(mount_point)" ]]; then
    chk ok "volume mounted"
    cmd_verify >/dev/null 2>&1 && chk ok "config.txt valid" || chk bad "config.txt missing or invalid (run configure)"
  else chk bad "volume '$VOLUME_NAME' not mounted (fine if the card is already in the EmotiBit)"; fi
  echo "Software"
  if [[ -d "$(app_path EmotiBitOscilloscope)" ]]; then chk ok "installed ($(cat "$EMOTIBIT_HOME/VERSION" 2>/dev/null))"
  else chk bad "not downloaded (run download)"; fi
  echo "Device"
  list_ports >/dev/null 2>&1; [[ -n "$(list_ports)" ]] && chk ok "USB serial port found" || chk bad "no USB serial port"
  echo "Network"
  local dev sec; dev="$(wifi_device)"
  sec="$(ipconfig getsummary "$dev" 2>/dev/null | awk -F' : ' '/^ +Security : / {print $2; exit}')"
  if [[ -n "$sec" ]]; then
    echo "  [info] this Mac's WiFi security: $sec"
    [[ "$sec" == *WPA3* ]] && echo "  [warn] WPA3: Feather M0 can't join; ESP32 may not. Use WPA2/mixed or a 2.4 GHz hotspot."
  else echo "  [info] this Mac is not on WiFi (it must share the EmotiBit's 2.4 GHz network for the Oscilloscope)"; fi
  echo "$ok ok, $bad to fix"
}

main() {
  local cmd="${1:-}"; shift || true
  local yes=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -v|--volume) VOLUME_NAME="${2:?}"; shift 2 ;;
      -y|--yes)    yes=1; shift ;;
      -h|--help)   usage; exit 0 ;;
      *) die "unknown option: $1 (secrets are entered interactively, never as arguments)" ;;
    esac
  done
  case "$cmd" in
    status)    cmd_status ;;
    configure) cmd_configure "$yes" ;;
    verify)    cmd_verify ;;
    clean)     cmd_clean ;;
    eject)     cmd_eject ;;
    download)  cmd_download ;;
    tools)     cmd_tools ;;
    chip)      cmd_chip ;;
    ports)     cmd_ports ;;
    flash)     cmd_flash ;;
    oscilloscope) open_app EmotiBitOscilloscope ;;
    parser)    open_app EmotiBitDataParser ;;
    doctor)    cmd_doctor ;;
    all)       cmd_configure "$yes"; cmd_verify; cmd_clean; cmd_eject ;;
    -h|--help|help|"") usage ;;
    *) usage; exit 1 ;;
  esac
}

main "$@"
