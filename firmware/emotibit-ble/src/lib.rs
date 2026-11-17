#![no_std]

extern crate alloc;

pub mod adc_cal;
pub mod board;
#[path = "../../../src/shared/codec.rs"]
pub mod codec;
pub mod control;
#[path = "../../../src/shared/derive.rs"]
pub mod derive;
pub mod sdcard;
pub mod sensors;
pub mod ota;
pub mod radio;
pub mod settings;
#[path = "../../../src/shared/stock.rs"]
pub mod stock;
#[path = "../../../src/shared/tuning.rs"]
pub mod tuning;
#[path = "../../../src/shared/units.rs"]
pub mod units;
