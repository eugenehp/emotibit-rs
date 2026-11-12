//! `emotibit-flash`: the firmware commands of `emotibit firmware` as a binary of their own.
//! See `emotibit::flasher`.

use clap::Parser;
use emotibit::flasher::{self, FirmwareCommand};

#[derive(Parser)]
#[command(name = "emotibit-flash", version, about = "Firmware for the EmotiBit's Feather ESP32: info, backup, stock, emotibit-ble, any image, restore")]
struct Cli {
    #[command(subcommand)]
    command: FirmwareCommand,
}

fn main() -> anyhow::Result<()> {
    flasher::run(Cli::parse().command)
}
