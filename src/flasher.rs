//! Firmware for the Feather ESP32 Huzzah under the EmotiBit: see what is on a board, back it up,
//! and install the stock EmotiBit firmware, the `emotibit-ble` firmware, or any ESP32 image.
//!
//! Used by `emotibit firmware …` and by the `emotibit-flash` binary (same commands).
//!
//! | command | writes |
//! |---|---|
//! | `info` | nothing: chip, MAC, flash, partition table, the firmware in each app partition |
//! | `backup` | nothing on the board: the whole flash to a file |
//! | `ble` | `emotibit-ble`: bootloader, partition table, app (built in, or `--image`) |
//! | `stock` | the official installer's four parts (bootloader@0x1000, partitions@0x8000, boot_app0@0xe000, app@0x10000) |
//! | `write` | any image: merged (at 0), app (at 0x10000), partition table (at 0x8000), or `FILE@ADDRESS` |
//! | `restore` | a `backup` file, all of it, at 0 |
//! | `ota` | `emotibit-ble` over Bluetooth, no USB (`emotibit firmware ota`; the app part only) |
//!
//! `ble` and `stock` keep the board's settings partition (`nvs`) when the partition table stays
//! the same, and erase it when the table changes, so neither firmware finds the other's data
//! (`ble` keeps it across emotibit-ble's own table change, which added update slots). When the
//! table has update slots, `ble` also resets which slot starts, so the board runs what it wrote.
//! The Feather M0 is not supported (it is flashed with `bossac`).

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::Flasher;
use espflash::image_format::Segment;
use espflash::target::ProgressCallbacks;
use indicatif::{ProgressBar, ProgressStyle};
use serialport::{SerialPortType, UsbPortInfo};

const CONNECT_BAUD: u32 = 115_200;
const DEFAULT_FLASH_BAUD: u32 = 921_600;
/// Where `scripts/cli.sh download` puts the official installer (macOS).
const INSTALLER_RESOURCES: &str = ".emotibit/software/EmotiBitFirmwareInstaller.app/Contents/Resources";
/// Name of the BLE firmware image next to the binary or in `firmware/`.
const BLE_IMAGE: &str = "emotibit-ble.bin";

const BOOTLOADER_AT: u32 = 0x1000;
const PARTITIONS_AT: u32 = 0x8000;
const APP_AT: u32 = 0x10000;
const SECTOR: u32 = 0x1000;

/// The `emotibit-ble` image built into this binary (`build.rs`: `firmware/emotibit-ble.bin`
/// without its trailing erased bytes), if it was there at compile time.
#[cfg(embedded_ble_firmware)]
const EMBEDDED_BLE: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/emotibit-ble.bin")));
#[cfg(not(embedded_ble_firmware))]
const EMBEDDED_BLE: Option<&[u8]> = None;

#[derive(Subcommand)]
pub enum FirmwareCommand {
    /// List USB serial ports and mark the likely Feather
    Ports,
    /// Show what is on the board: chip, MAC, flash size, partitions, firmware (read-only)
    #[command(alias = "chip")]
    Info {
        #[command(flatten)]
        port: PortArgs,
    },
    /// Save the board's whole flash to a file, to put it back later with `restore`
    Backup {
        /// File to write (default: emotibit-backup-MAC-DATE.bin here)
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        port: PortArgs,
    },
    /// Install the emotibit-ble firmware (Bluetooth; its settings are kept across updates)
    Ble {
        /// A merged image (`espflash save-image --merge`) instead of the one built in
        #[arg(long)]
        image: Option<PathBuf>,
        #[command(flatten)]
        target: Target,
    },
    /// Install the stock EmotiBit firmware (WiFi), from the official installer's files
    #[command(alias = "flash")]
    Stock {
        /// The installer's files: a directory holding `esp32/boot_app0.bin` somewhere below it
        /// (e.g. the extracted EmotiBitSoftware zip, any OS). Default: .emotibit/software
        #[arg(short, long, env = "EMOTIBIT_FIRMWARE_DIR")]
        firmware_dir: Option<PathBuf>,
        /// Another app image at 0x10000, e.g. EmotiBit_stock_firmware_PPG_100Hz.feather_esp32.bin
        /// from github.com/EmotiBit/EmotiBit_FeatherWing/releases
        #[arg(long)]
        app: Option<PathBuf>,
        #[command(flatten)]
        target: Target,
    },
    /// Write any ESP32 image. FILE is placed by its contents (a merged image at 0, an app at
    /// 0x10000, a partition table at 0x8000); FILE@ADDRESS (e.g. app.bin@0x10000) places it
    Write {
        #[arg(required = true, value_name = "FILE[@ADDRESS]")]
        images: Vec<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Write a file made by `backup` (or any full image) back, from address 0
    Restore {
        file: PathBuf,
        #[command(flatten)]
        target: Target,
    },
    /// Update the emotibit-ble firmware over Bluetooth, without USB (pick the board with
    /// `emotibit --device NAME`). Needs emotibit-ble 0.1.1 or later on the board (with update
    /// slots), installed over USB once with `firmware ble`
    Ota {
        /// An emotibit-ble image (merged, or the app alone) instead of the one built in
        #[arg(long)]
        image: Option<PathBuf>,
        /// Check the image and find the board, without updating it
        #[arg(long)]
        dry_run: bool,
        /// Do not ask before updating
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Args, Clone)]
pub struct PortArgs {
    /// Serial port (default: the one CP210x port, the Feather's USB bridge)
    #[arg(short, long)]
    pub port: Option<String>,
    /// Baud rate for the transfer (lower it, e.g. 460800, if it is unreliable)
    #[arg(short, long, default_value_t = DEFAULT_FLASH_BAUD)]
    pub baud: u32,
}

#[derive(Args, Clone)]
pub struct Target {
    #[command(flatten)]
    pub port: PortArgs,
    /// Show the plan and check the files, without touching the board
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask before writing
    #[arg(short, long)]
    pub yes: bool,
}

pub fn run(cmd: FirmwareCommand) -> Result<()> {
    match cmd {
        FirmwareCommand::Ports => ports(),
        FirmwareCommand::Info { port } => info(&port),
        FirmwareCommand::Backup { output, port } => backup(output, &port),
        FirmwareCommand::Ble { image, target } => ble(image.as_deref(), &target),
        FirmwareCommand::Stock { firmware_dir, app, target } => stock(firmware_dir.as_deref(), app.as_deref(), &target),
        FirmwareCommand::Write { images, target } => write_any(&images, &target),
        FirmwareCommand::Restore { file, target } => restore(&file, &target),
        FirmwareCommand::Ota { .. } => bail!("updates over Bluetooth are `emotibit firmware ota`"),
    }
}

// ── Ports and connection ─────────────────────────────────────────────────────

/// USB serial ports. On macOS only `/dev/cu.*` is used (the `tty.*` twin blocks on open).
fn usb_ports() -> Result<Vec<(String, UsbPortInfo)>> {
    Ok(serialport::available_ports()?
        .into_iter()
        .filter_map(|p| match p.port_type {
            SerialPortType::UsbPort(info) if !p.port_name.contains("/tty.") => Some((p.port_name, info)),
            _ => None,
        })
        .collect())
}

/// Silicon Labs CP210x (the Feather ESP32's USB-serial bridge): VID 0x10C4.
fn is_cp210x(info: &UsbPortInfo) -> bool {
    info.vid == 0x10C4
}

fn ports() -> Result<()> {
    let list = usb_ports()?;
    if list.is_empty() {
        bail!("no USB serial ports found (check the cable carries data, power, the HIB switch, the CP210x driver)");
    }
    for (name, info) in &list {
        let guess = if is_cp210x(info) { "ESP32 Feather (CP210x)" } else { "other" };
        println!("{name}\t{:04x}:{:04x}\t{guess}", info.vid, info.pid);
    }
    Ok(())
}

fn pick_port(explicit: Option<&str>) -> Result<(String, UsbPortInfo)> {
    let list = usb_ports()?;
    match explicit {
        Some(name) => list
            .into_iter()
            .find(|(n, _)| n == name)
            .with_context(|| format!("{name} is not a connected USB serial port")),
        None => {
            let mut cp: Vec<_> = list.into_iter().filter(|(_, i)| is_cp210x(i)).collect();
            match cp.len() {
                0 => bail!("no CP210x (ESP32 Feather) port found; run `ports`, or pass --port"),
                1 => Ok(cp.remove(0)),
                _ => bail!("several CP210x ports found; pass --port"),
            }
        }
    }
}

fn connect(port: &PortArgs) -> Result<Flasher> {
    let (name, info) = pick_port(port.port.as_deref())?;
    println!("Using {name}");
    let serial = serialport::new(&name, CONNECT_BAUD)
        .flow_control(serialport::FlowControl::None)
        .open_native()
        .with_context(|| format!("could not open {name} (is the installer, a serial monitor or another tool using it?)"))?;
    let conn = Connection::new(serial, info, ResetAfterOperation::HardReset, ResetBeforeOperation::DefaultReset, CONNECT_BAUD);
    let mut flasher = Flasher::connect(conn, true, true, false, None, Some(port.baud))
        .context("could not enter the ESP32 bootloader (press Reset, check the HIB switch, try another cable or --baud 460800)")?;
    let chip = flasher.device_info()?.chip;
    if chip.to_string().to_lowercase() != "esp32" {
        bail!("expected an ESP32, found {chip}; refusing to go on");
    }
    Ok(flasher)
}

/// Restarts the board into its firmware. Writes do this themselves; after reads the board
/// would otherwise stay in the bootloader.
fn restart(mut flasher: Flasher) -> Result<()> {
    let chip = flasher.device_info()?.chip;
    flasher.connection().reset_after(true, chip).context("could not restart the board (press Reset)")
}

/// Reads `len` bytes of flash at `offset` (through a temporary file, as espflash wants).
fn read(flasher: &mut Flasher, offset: u32, len: u32) -> Result<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("emotibit-flash-{}-{offset:x}.bin", std::process::id()));
    let result = flasher.read_flash(offset, len, 0x1000, 64, path.clone()).context("reading the flash failed");
    let data = result.and_then(|_| std::fs::read(&path).context("reading the flash failed"));
    let _ = std::fs::remove_file(&path);
    data
}

// ── Images ───────────────────────────────────────────────────────────────────

/// One entry of an ESP32 partition table.
#[derive(Clone, PartialEq, Eq)]
struct Partition {
    label: String,
    kind: u8,
    subtype: u8,
    offset: u32,
    size: u32,
}

impl Partition {
    fn is_app(&self) -> bool {
        self.kind == 0
    }
    fn describe(&self) -> &'static str {
        match (self.kind, self.subtype) {
            (0, 0x00) => "app (factory)",
            (0, 0x10..=0x1f) => "app (OTA)",
            (0, _) => "app",
            (1, 0x00) => "OTA state",
            (1, 0x01) => "PHY calibration",
            (1, 0x02) => "settings (NVS)",
            (1, 0x81) => "FAT",
            (1, 0x82) => "SPIFFS",
            _ => "data",
        }
    }
}

/// The entries of a partition table (as written at 0x8000).
fn partitions(table: &[u8]) -> Vec<Partition> {
    table
        .as_chunks::<32>()
        .0
        .iter()
        .take_while(|e| e[..2] == [0xAA, 0x50])
        .map(|e| Partition {
            label: String::from_utf8_lossy(&e[12..28]).trim_end_matches('\0').to_string(),
            kind: e[2],
            subtype: e[3],
            offset: u32::from_le_bytes(e[4..8].try_into().unwrap()),
            size: u32::from_le_bytes(e[8..12].try_into().unwrap()),
        })
        .collect()
}

fn is_partition_table(data: &[u8]) -> bool {
    data.len() >= 32 && data[..2] == [0xAA, 0x50]
}

/// An ESP32 app or bootloader image starts with 0xE9.
fn is_esp_image(data: &[u8]) -> bool {
    data.first() == Some(&0xE9)
}

/// A merged image (`espflash save-image --merge`, `esptool merge_bin`, or a `backup`): starts at
/// address 0, bootloader at 0x1000, partition table at 0x8000.
fn is_merged(data: &[u8]) -> bool {
    data.len() > PARTITIONS_AT as usize + 32 && is_esp_image(&data[BOOTLOADER_AT as usize..]) && is_partition_table(&data[PARTITIONS_AT as usize..])
}

fn trim_erased(data: &[u8]) -> &[u8] {
    let end = data.iter().rposition(|&b| b != 0xFF).map_or(0, |i| i + 1);
    &data[..end]
}

/// The app descriptor (`esp_app_desc_t`) in an app image: project, version, IDF. (Its build
/// date is the descriptor library's, for both firmwares, so it is not shown.)
struct AppDesc {
    project: String,
    version: String,
    idf: String,
}

fn app_desc(image: &[u8]) -> Option<AppDesc> {
    // Image header (24 bytes) and the first segment's header (8), then the descriptor.
    let d = image.get(32..32 + 144)?;
    if d[..4] != 0xABCD_5432u32.to_le_bytes() {
        return None;
    }
    let s = |r: std::ops::Range<usize>| String::from_utf8_lossy(&d[r]).trim_end_matches('\0').trim().to_string();
    Some(AppDesc { version: s(16..48), project: s(48..80), idf: s(112..144) })
}

/// Which firmware an app descriptor belongs to, in words.
fn firmware_name(d: &AppDesc) -> String {
    match d.project.as_str() {
        "emotibit-ble" => format!("emotibit-ble {} (Bluetooth)", d.version),
        // The stock firmware is an Arduino sketch; the descriptor names Arduino's builder.
        "arduino-lib-builder" => "an Arduino sketch (the stock EmotiBit firmware is one)".to_string(),
        p => format!("{p} {}", d.version),
    }
}

/// A part to write.
struct Part {
    name: String,
    addr: u32,
    data: Vec<u8>,
}

/// What to do to the board.
struct Plan {
    title: String,
    parts: Vec<Part>,
    /// The partition table this plan installs, if any: the board's settings partition is erased
    /// when the board has a different one.
    table: Option<Vec<Partition>>,
    /// emotibit-ble: its settings are kept across its own table changes.
    ble: bool,
}

/// The bootloader, partition table and app partitions of a merged image; its data partitions
/// (settings, calibration) are left out, so the board keeps its own.
fn split_merged(data: &[u8], what: &str) -> Result<(Vec<Part>, Vec<Partition>)> {
    if !is_merged(data) {
        bail!("{what} is not a merged ESP32 image (no bootloader at 0x1000 and partition table at 0x8000)");
    }
    let table = partitions(&data[PARTITIONS_AT as usize..]);
    let slice = |from: u32, to: u32| trim_erased(&data[(from as usize).min(data.len())..(to as usize).min(data.len())]).to_vec();
    let mut parts = vec![
        Part { name: "bootloader".into(), addr: BOOTLOADER_AT, data: slice(BOOTLOADER_AT, PARTITIONS_AT) },
        Part { name: "partition table".into(), addr: PARTITIONS_AT, data: slice(PARTITIONS_AT, PARTITIONS_AT + 0x1000) },
    ];
    for p in table.iter().filter(|p| p.is_app()) {
        let bytes = slice(p.offset, p.offset.saturating_add(p.size));
        if !bytes.is_empty() {
            parts.push(Part { name: format!("app ({})", p.label), addr: p.offset, data: bytes });
        }
    }
    if parts.len() < 3 {
        bail!("{what} holds no app");
    }
    Ok((parts, table))
}

// ── Commands ─────────────────────────────────────────────────────────────────

fn info(port: &PortArgs) -> Result<()> {
    let mut flasher = connect(port)?;
    let info = flasher.device_info()?;
    println!("Chip:        {} {}", info.chip, info.revision.map_or(String::new(), |(a, b)| format!("rev {a}.{b}")));
    println!("Flash:       {}", info.flash_size);
    if let Some(mac) = &info.mac_address {
        println!("MAC:         {mac}");
    }
    let table = partitions(&read(&mut flasher, PARTITIONS_AT, 0x1000)?);
    if table.is_empty() {
        println!("No partition table: the flash is erased or holds no ESP-IDF/Arduino firmware.");
        return restart(flasher);
    }
    println!("Partitions:");
    for p in &table {
        println!("  {:<10} 0x{:06x}  {:>5} KB  {}", p.label, p.offset, p.size / 1024, p.describe());
    }
    for p in table.iter().filter(|p| p.is_app()) {
        let head = read(&mut flasher, p.offset, 0x1000)?;
        let what = match app_desc(&head) {
            Some(d) => format!("{}{}", firmware_name(&d), if d.idf.is_empty() || d.idf == "0.0.0" { String::new() } else { format!(", ESP-IDF {}", d.idf) }),
            None if is_esp_image(&head) => "an app without a descriptor".into(),
            None => "empty".into(),
        };
        println!("Firmware in {}: {what}", p.label);
    }
    // With update slots: the one otadata selects (the higher of its two sequence numbers).
    let slots: Vec<&Partition> = table.iter().filter(|p| p.kind == 0 && (0x10..=0x1f).contains(&p.subtype)).collect();
    if let Some(ota) = table.iter().find(|p| p.kind == 1 && p.subtype == 0x00)
        && !slots.is_empty()
    {
        let data = read(&mut flasher, ota.offset, 0x2000)?;
        let seq = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
        let best = [seq(0), seq(0x1000)].into_iter().filter(|&s| s != u32::MAX && s != 0).max();
        let starts = match best {
            // The bootloader also writes it, for the first slot, when it finds it empty.
            Some(s) => slots.iter().find(|p| p.subtype == 0x10 + ((s - 1) % slots.len() as u32) as u8).map_or("?".into(), |p| p.label.clone()),
            None => format!("{} ({} is empty)", slots[0].label, ota.label),
        };
        println!("Starts:      {starts}");
    }
    restart(flasher)
}

fn backup(output: Option<PathBuf>, port: &PortArgs) -> Result<()> {
    let mut flasher = connect(port)?;
    let info = flasher.device_info()?;
    let size = info.flash_size.size();
    let path = output.unwrap_or_else(|| {
        let mac = info.mac_address.clone().unwrap_or_default().replace(':', "");
        PathBuf::from(format!("emotibit-backup-{mac}-{}.bin", chrono::Local::now().format("%Y%m%d-%H%M%S")))
    });
    if path.exists() {
        bail!("{} exists; pass another --output", path.display());
    }
    println!("Reading {} MB of flash (about a minute at {} baud)…", size / 0x10_0000, port.baud);
    flasher.read_flash(0, size, 0x1000, 64, path.clone()).context("reading the flash failed")?;
    restart(flasher)?;
    // The previous firmware's settings are in it (the stock firmware's WiFi library may keep
    // the last network there): keep it private.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    println!("Saved {} ({size} bytes, checked against the board's MD5).", path.display());
    println!("It can hold saved WiFi settings: keep it private. Put it back with `restore {}`.", path.display());
    Ok(())
}

/// Finds the BLE image: `--image`, the one built in, or `emotibit-ble.bin` next to the binary or
/// in `firmware/`.
fn ble_image(image: Option<&Path>) -> Result<(Vec<u8>, String)> {
    if let Some(p) = image {
        return Ok((std::fs::read(p).with_context(|| format!("cannot read {}", p.display()))?, p.display().to_string()));
    }
    if let Some(data) = EMBEDDED_BLE {
        return Ok((data.to_vec(), format!("built-in emotibit-ble image ({} bytes)", data.len())));
    }
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_default();
    let candidates = [exe_dir.join(BLE_IMAGE), exe_dir.join("firmware").join(BLE_IMAGE), PathBuf::from("firmware").join(BLE_IMAGE)];
    for c in &candidates {
        if let Ok(data) = std::fs::read(c) {
            return Ok((data, c.display().to_string()));
        }
    }
    bail!("no emotibit-ble image: pass --image FILE (firmware/emotibit-ble.bin in the repository or the release archive)")
}

fn ble(image: Option<&Path>, target: &Target) -> Result<()> {
    let (data, what) = ble_image(image)?;
    let (parts, table) = split_merged(&data, &what)?;
    if let Some(d) = parts.iter().find(|p| p.name.starts_with("app")).and_then(|p| app_desc(&p.data)) {
        println!("Image: {what}: {}", firmware_name(&d));
    }
    execute(Plan { title: "emotibit-ble firmware".into(), parts, table: Some(table), ble: true }, target)
}

/// The emotibit-ble app image for an update over Bluetooth (`--image`, merged or the app alone,
/// or the one built in), and what it is in words.
pub fn ota_image(image: Option<&Path>) -> Result<(Vec<u8>, String)> {
    let (data, what) = ble_image(image)?;
    let app = if is_merged(&data) {
        let (parts, _) = split_merged(&data, &what)?;
        parts.into_iter().find(|p| p.name.starts_with("app")).map(|p| p.data).context("no app in the image")?
    } else if is_esp_image(&data) {
        trim_erased(&data).to_vec()
    } else {
        bail!("{what} is neither a merged image nor an app image");
    };
    match app_desc(&app) {
        Some(d) if d.project == "emotibit-ble" => Ok((app, format!("{what}: {}", firmware_name(&d)))),
        Some(d) => bail!("{what} holds {}, not emotibit-ble; only emotibit-ble updates over Bluetooth", firmware_name(&d)),
        None => bail!("{what} has no app descriptor; only emotibit-ble updates over Bluetooth"),
    }
}

/// The directory holding `esp32/boot_app0.bin` at or below `root` (the installer's Resources).
fn find_installer(root: &Path, depth: usize) -> Option<PathBuf> {
    if root.join("esp32").join("boot_app0.bin").is_file() {
        return Some(root.to_path_buf());
    }
    if depth == 0 {
        return None;
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.iter().find_map(|d| find_installer(d, depth - 1))
}

fn load(path: &Path) -> Result<Vec<u8>> {
    let data = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    if data.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(data)
}

fn stock(firmware_dir: Option<&Path>, app: Option<&Path>, target: &Target) -> Result<()> {
    let root = firmware_dir.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(INSTALLER_RESOURCES));
    let dir = find_installer(&root, 6).with_context(|| {
        format!(
            "no installer files (esp32/boot_app0.bin) under {}. Get the EmotiBit software from \
             github.com/EmotiBit/ofxEmotiBit/releases (macOS: scripts/cli.sh download), extract it, \
             and pass its folder with --firmware-dir",
            root.display()
        )
    })?;
    let esp32 = dir.join("esp32");
    let bootloader = load(&esp32.join("EmotiBit_stock_firmware.ino.bootloader.bin"))?;
    let table = load(&esp32.join("EmotiBit_stock_firmware.partitions.bin"))?;
    let boot_app0 = load(&esp32.join("boot_app0.bin"))?;
    let (app_name, app_data) = match app {
        Some(p) => (format!("app ({})", p.file_name().map_or("custom".into(), |n| n.to_string_lossy().to_string())), load(p)?),
        None => ("app (stock EmotiBit)".to_string(), load(&dir.join("EmotiBit_stock_firmware.ino.feather_esp32.bin"))?),
    };
    for (name, d) in [("bootloader", &bootloader), (app_name.as_str(), &app_data)] {
        if !is_esp_image(d) {
            bail!("{name} is not an ESP32 image (a Feather M0 .bin, or another file?)");
        }
    }
    if !is_partition_table(&table) {
        bail!("the installer's partition table is not one");
    }
    let parts = vec![
        Part { name: "bootloader".into(), addr: BOOTLOADER_AT, data: bootloader },
        Part { name: "partition table".into(), addr: PARTITIONS_AT, data: table.clone() },
        Part { name: "boot_app0 (OTA state)".into(), addr: 0xe000, data: boot_app0 },
        Part { name: app_name, addr: APP_AT, data: app_data },
    ];
    println!("Installer files: {}", dir.display());
    execute(Plan { title: "stock EmotiBit firmware".into(), parts, table: Some(partitions(&table)), ble: false }, target)
}

/// `FILE` or `FILE@ADDRESS` (hex with 0x, or decimal).
fn parse_image_arg(arg: &str) -> Result<Part> {
    let (file, addr) = match arg.rsplit_once('@') {
        Some((f, a)) => {
            let addr = match a.strip_prefix("0x").or_else(|| a.strip_prefix("0X")) {
                Some(h) => u32::from_str_radix(h, 16),
                None => a.parse(),
            }
            .with_context(|| format!("bad address in {arg}"))?;
            (f, Some(addr))
        }
        None => (arg, None),
    };
    let data = load(Path::new(file))?;
    if data.starts_with(b"\x7fELF") {
        bail!("{file} is an ELF file; make an image first: espflash save-image --chip esp32 --merge {file} image.bin");
    }
    let (name, addr) = match addr {
        Some(a) => (file.to_string(), a),
        None if is_merged(&data) => (format!("{file} (merged image)"), 0),
        None if is_partition_table(&data) => (format!("{file} (partition table)"), PARTITIONS_AT),
        None if is_esp_image(&data) => {
            println!("note: {file} looks like an app; writing it at 0x{APP_AT:x}. The board's bootloader and partition table must suit it.");
            (format!("{file} (app)"), APP_AT)
        }
        None => bail!("cannot tell where {file} goes; write it as {file}@ADDRESS"),
    };
    Ok(Part { name, addr, data })
}

fn write_any(images: &[String], target: &Target) -> Result<()> {
    let parts = images.iter().map(|a| parse_image_arg(a)).collect::<Result<Vec<_>>>()?;
    execute(Plan { title: "images".into(), parts, table: None, ble: false }, target)
}

fn restore(file: &Path, target: &Target) -> Result<()> {
    let data = load(file)?;
    if !is_merged(&data) {
        bail!("{} is not a full flash image (no bootloader at 0x1000 and partition table at 0x8000)", file.display());
    }
    let parts = vec![Part { name: format!("{} (all of it)", file.display()), addr: 0, data }];
    execute(Plan { title: "a full flash image".into(), parts, table: None, ble: false }, target)
}

// ── Writing ──────────────────────────────────────────────────────────────────

struct Bar(Option<ProgressBar>);

impl ProgressCallbacks for Bar {
    fn init(&mut self, addr: u32, total: usize) {
        let pb = ProgressBar::new(total as u64);
        pb.set_style(ProgressStyle::with_template("{msg:>12} [{bar:40}] {bytes}/{total_bytes}").unwrap().progress_chars("=> "));
        pb.set_message(format!("0x{addr:x}"));
        self.0 = Some(pb);
    }
    fn update(&mut self, current: usize) {
        if let Some(pb) = &self.0 {
            pb.set_position(current as u64);
        }
    }
    fn verifying(&mut self) {
        if let Some(pb) = &self.0 {
            pb.set_message("verifying");
        }
    }
    fn finish(&mut self, _skipped: bool) {
        if let Some(pb) = self.0.take() {
            pb.finish();
        }
    }
}

fn check(parts: &[Part]) -> Result<()> {
    let mut spans: Vec<(u32, u32, &str)> = parts.iter().map(|p| (p.addr, p.addr + p.data.len() as u32, p.name.as_str())).collect();
    spans.sort();
    for p in parts {
        if p.addr % SECTOR != 0 {
            bail!("{} at 0x{:x}: addresses must be multiples of 0x1000", p.name, p.addr);
        }
        if p.data.is_empty() {
            bail!("{} is empty", p.name);
        }
    }
    for w in spans.windows(2) {
        if w[1].0 < w[0].1 {
            bail!("{} and {} overlap", w[0].2, w[1].2);
        }
    }
    Ok(())
}

fn confirm(question: &str) -> Result<()> {
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let mut ans = String::new();
    io::stdin().read_line(&mut ans)?;
    if !ans.trim().eq_ignore_ascii_case("y") {
        bail!("aborted");
    }
    Ok(())
}

fn execute(mut plan: Plan, target: &Target) -> Result<()> {
    check(&plan.parts)?;
    plan.parts.sort_by_key(|p| p.addr);
    println!("Plan: {} (ESP32, {} baud):", plan.title, target.port.baud);
    for p in &plan.parts {
        println!("  0x{:06x}  {:>8} bytes  {}", p.addr, p.data.len(), p.name);
    }
    if plan.table.is_some() {
        println!("  The board's settings partition is kept if its partition table is the same, erased otherwise.");
    }
    if target.dry_run {
        println!("Dry run: the files look right; the board was not touched.");
        return Ok(());
    }
    if !target.yes {
        println!(
            "\nBefore writing: HIB switch not on HIB, boards stacked, battery connected, nothing else using the\n\
             port (installer, serial monitor), the EmotiBit not worn. A `backup` first lets you undo this."
        );
        confirm("Write to the board now?")?;
    }

    let mut flasher = connect(&target.port)?;
    let info = flasher.device_info()?;
    println!("Connected: {} ({} flash)", info.chip, info.flash_size);
    let end = plan.parts.iter().map(|p| p.addr as u64 + p.data.len() as u64).max().unwrap_or(0);
    if end > info.flash_size.size() as u64 {
        bail!("the images end at 0x{end:x}, past the board's {} flash", info.flash_size);
    }

    // A different partition table: the settings partition holds another firmware's data,
    // unless the board already runs emotibit-ble and its settings stay where they were.
    if let Some(new) = &plan.table {
        let old = partitions(&read(&mut flasher, PARTITIONS_AT, 0x1000)?);
        if &old != new {
            let mut was_ble = false;
            for p in old.iter().filter(|p| p.is_app()) {
                was_ble |= app_desc(&read(&mut flasher, p.offset, 0x100)?).is_some_and(|d| d.project == "emotibit-ble");
            }
            for p in new.iter().filter(|p| p.kind == 1 && p.subtype == 0x02) {
                let same_place = old.iter().any(|o| o.kind == 1 && o.subtype == 0x02 && o.offset == p.offset);
                if plan.ble && was_ble && same_place {
                    println!("New partition table: keeping the emotibit-ble settings in {} (0x{:x})", p.label, p.offset);
                    continue;
                }
                println!("New partition table: erasing {} (0x{:x}, {} KB)", p.label, p.offset, p.size / 1024);
                flasher.erase_region(p.offset, p.size).context("erasing the settings partition failed")?;
            }
        }
        // Which update slot starts: reset, so the board runs the app written now (the first
        // slot), not one an earlier update over Bluetooth selected. Not when a part writes it
        // (the stock installer's boot_app0).
        for p in new.iter().filter(|p| p.kind == 1 && p.subtype == 0x00) {
            if !plan.parts.iter().any(|w| w.addr >= p.offset && w.addr < p.offset + p.size) {
                println!("Resetting {} (which app slot starts)", p.label);
                flasher.erase_region(p.offset, p.size).context("erasing otadata failed")?;
            }
        }
    }

    // One flash session for all parts: separate write calls re-attach the SPI flash each time,
    // which timed out on the second call. Data is padded to a multiple of 4 bytes.
    let segments: Vec<Segment> = plan
        .parts
        .iter()
        .map(|p| {
            let mut data = p.data.clone();
            data.resize(data.len().next_multiple_of(4), 0xFF);
            Segment { addr: p.addr, data: Cow::Owned(data) }
        })
        .collect();
    flasher.write_bins_to_flash(&segments, &mut Bar(None)).context("writing failed")?;
    println!("Done; the board has been reset.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged() -> Vec<u8> {
        let mut d = vec![0xFF; 0x20000];
        d[0x1000] = 0xE9;
        d[0x1001..0x1010].fill(1);
        let mut e = [0u8; 32];
        e[..2].copy_from_slice(&[0xAA, 0x50]);
        e[2] = 0;
        e[4..8].copy_from_slice(&0x10000u32.to_le_bytes());
        e[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        e[12..19].copy_from_slice(b"factory");
        d[0x8000..0x8020].copy_from_slice(&e);
        let mut nvs = e;
        nvs[2] = 1;
        nvs[3] = 2;
        nvs[4..8].copy_from_slice(&0x9000u32.to_le_bytes());
        nvs[8..12].copy_from_slice(&0x6000u32.to_le_bytes());
        nvs[12..19].copy_from_slice(b"nvs\0\0\0\0");
        d[0x8020..0x8040].copy_from_slice(&nvs);
        d[0x10000] = 0xE9;
        d[0x10001..0x10100].fill(2);
        d
    }

    #[test]
    fn merged_images_split_into_boot_table_and_apps() {
        let d = merged();
        assert!(is_merged(&d));
        let (parts, table) = split_merged(&d, "test").unwrap();
        assert_eq!(table.len(), 2);
        let spans: Vec<(u32, usize)> = parts.iter().map(|p| (p.addr, p.data.len())).collect();
        // The erased tails are dropped; nvs (data) is not written.
        assert_eq!(spans, vec![(0x1000, 0x10), (0x8000, 0x40), (0x10000, 0x100)]);
        check(&parts).unwrap();
    }

    #[test]
    fn overlapping_or_unaligned_parts_are_refused() {
        let p = |addr, len| Part { name: format!("{addr:x}"), addr, data: vec![0; len] };
        assert!(check(&[p(0x1000, 0x8000), p(0x8000, 16)]).is_err());
        assert!(check(&[p(0x1800, 16)]).is_err());
        assert!(check(&[p(0x1000, 16), p(0x8000, 16)]).is_ok());
    }

    #[test]
    fn app_descriptor() {
        let mut img = vec![0u8; 256];
        img[0] = 0xE9;
        img[32..36].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
        img[48..53].copy_from_slice(b"0.1.0");
        img[80..92].copy_from_slice(b"emotibit-ble");
        let d = app_desc(&img).unwrap();
        assert_eq!(firmware_name(&d), "emotibit-ble 0.1.0 (Bluetooth)");
    }
}
