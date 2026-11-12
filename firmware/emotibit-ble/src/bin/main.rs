#![no_std]
#![no_main]

//! EmotiBit BLE firmware: advertises as "EmotiBit-BLE-XXXX" (XXXX from the chip MAC) and
//! streams every sensor as notifications on a custom GATT service.
//!
//! Record layout (little endian, 20 bytes):
//!   u8  tag      channel id (see `sensors::tag`)
//!   u8  count    number of samples that follow
//!   u16 seq      per-channel sequence number
//!   i32 sample[] raw sample values (count of them, up to 4)
//!
//! Records are batched up to the negotiated ATT MTU, either as-is or in the compact form of
//! `codec`, as selected through the mode characteristic. Commands and status travel as stock
//! EmotiBit packet lines on the control and events characteristics (see `control`). The SD
//! card writer runs on the second core (see `sdcard`).

use bt_hci::controller::ExternalController;
use embassy_executor::Spawner;
use embassy_futures::{join::join5, select::{select, select4, Either}};
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use emotibit_ble::control::{self, Power};
use emotibit_ble::{adc_cal, sdcard, sensors};
use log::{info, warn};
use static_cell::{ConstStaticCell, StaticCell};
use trouble_host::prelude::*;

extern crate alloc;

/// Largest notification payload; the real limit is the negotiated ATT MTU minus 3.
const MAX_VALUE: usize = 500;
/// Largest control write: a few stock packet lines.
const MAX_CONTROL: usize = 500;
const CONNECTIONS_MAX: usize = 1;
/// Bump when characteristics are added or changed (see the address below).
const GATT_LAYOUT: u8 = 3;
const L2CAP_CHANNELS_MAX: usize = 2;

esp_bootloader_esp_idf::esp_app_desc!();

// 128-bit UUIDs shared with the host side (`emotibit::ble`).
#[gatt_server]
struct Server {
    emotibit: EmotiBitService,
}

#[gatt_service(uuid = "6e6f7469-6269-7400-0000-000000000001")]
struct EmotiBitService {
    /// Sensor data stream (notify).
    #[characteristic(uuid = "6e6f7469-6269-7400-0000-000000000002", notify)]
    data: heapless::Vec<u8, MAX_VALUE>,
    /// Wire format: 0 = raw 20-byte records (default), 1 = compact.
    #[characteristic(uuid = "6e6f7469-6269-7400-0000-000000000003", read, write, value = 0)]
    mode: u8,
    /// Host commands: stock packet lines (see `control`).
    #[characteristic(uuid = "6e6f7469-6269-7400-0000-000000000004", write)]
    control: heapless::Vec<u8, MAX_CONTROL>,
    /// Device status and time sync: stock packet lines (notify).
    #[characteristic(uuid = "6e6f7469-6269-7400-0000-000000000005", notify)]
    events: heapless::Vec<u8, 480>,
}

/// The SD card's SPI bus. `embedded-sdmmc` makes one SPI transaction per byte, and a chip
/// select that rises between them breaks multi-byte reads; the card is alone on the bus, so the
/// device gets a dummy chip select and the real one is held low.
type SdSpi = embedded_hal_bus::spi::ExclusiveDevice<
    esp_hal::spi::master::Spi<'static, esp_hal::Blocking>,
    NoCs,
    esp_hal::delay::Delay,
>;

/// A chip select that does nothing.
struct NoCs;

impl embedded_hal::digital::ErrorType for NoCs {
    type Error = core::convert::Infallible;
}

impl embedded_hal::digital::OutputPin for NoCs {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn set_high(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[embassy_executor::task]
async fn sd_task(mut spi: SdSpi, mut cs: Output<'static>) {
    // The card is powered by the EmotiBit's regulator, switched on at boot. Once it is up, the
    // card wants at least 74 clocks with chip select high before the first command.
    Timer::after(Duration::from_millis(500)).await;
    cs.set_high();
    let _ = embedded_hal::spi::SpiBus::write(spi.bus_mut(), &[0xFF; 10]);
    cs.set_low();
    sdcard::run(spi, esp_hal::delay::Delay::new(), |dev: &mut SdSpi| {
        let fast = esp_hal::spi::master::Config::default().with_frequency(Rate::from_mhz(20));
        if dev.bus_mut().apply_config(&fast).is_err() {
            warn!("SD card: cannot raise the SPI clock");
        }
    })
    .await
}

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 98768);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Feather ESP32 pin 32 enables the EmotiBit regulator (active high on V3+ boards).
    let mut vreg = Output::new(peripherals.GPIO32, Level::High, OutputConfig::default());

    // SD card on the second core: SPI SCK 5 / MOSI 18 / MISO 19, chip select 4 (as stock).
    {
        let spi = esp_hal::spi::master::Spi::new(
            peripherals.SPI2,
            esp_hal::spi::master::Config::default().with_frequency(Rate::from_khz(400)),
        )
        .unwrap()
        .with_sck(peripherals.GPIO5)
        .with_mosi(peripherals.GPIO18)
        .with_miso(peripherals.GPIO19);
        let cs = Output::new(peripherals.GPIO4, Level::High, OutputConfig::default());
        let dev = embedded_hal_bus::spi::ExclusiveDevice::new(spi, NoCs, esp_hal::delay::Delay::new()).unwrap();
        static STACK: ConstStaticCell<esp_hal::system::Stack<32768>> = ConstStaticCell::new(esp_hal::system::Stack::new());
        esp_rtos::start_second_core(peripherals.CPU_CTRL, peripherals.FROM_CPU_INTR1, STACK.take(), move || {
            static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();
            EXECUTOR.init(esp_rtos::embassy::Executor::new()).run(|spawner| spawner.spawn(sd_task(dev, cs).unwrap()));
        });
    }

    // Sensors: I2C on SDA 27 / SCL 13 once the regulator has had time to come up.
    Timer::after(Duration::from_millis(200)).await;
    let mut i2c = esp_hal::i2c::master::I2c::new(
        peripherals.I2C0,
        esp_hal::i2c::master::Config::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO27)
    .with_scl(peripherals.GPIO13)
    .into_async();
    // Battery: GPIO35 (ADC1) sees half the battery voltage.
    let mut adc_cfg = AdcConfig::new();
    let mut batt_pin = adc_cfg.enable_pin(peripherals.GPIO35, Attenuation::_11dB);
    let mut adc = Adc::new(peripherals.ADC1, adc_cfg);
    let adc_line = {
        use esp_hal::efuse::{self, ADC1_TP_HIGH, ADC1_TP_LOW, ADC_VREF, BLK3_PART_RESERVE};
        adc_cal::Line::from_efuse(
            efuse::read_bit(BLK3_PART_RESERVE),
            efuse::read_field_le::<u8>(ADC1_TP_LOW) as u32,
            efuse::read_field_le::<u16>(ADC1_TP_HIGH) as u32,
            efuse::read_field_le::<u8>(ADC_VREF) as u32,
        )
    };
    info!("ADC calibration {:?}", adc_line);
    let mut read_battery = move || {
        // Single ESP32 ADC readings are noisy; average a few.
        let mut sum = 0u32;
        for _ in 0..8 {
            sum += nb::block!(adc.read_oneshot(&mut batt_pin)).ok()? as u32;
        }
        let raw = (sum / 8) as u16;
        Some((raw, adc_line.millivolts(raw)))
    };

    // Button (GPIO12, high while pressed), as the stock firmware uses it.
    let mut button = Input::new(peripherals.GPIO12, InputConfig::default().with_pull(Pull::Down));
    let mut lpwr = esp_hal::rtc_cntl::sleep::LowPower::new(peripherals.LPWR);

    let transport = BleConnector::new(peripherals.BT, Default::default()).unwrap();
    let controller = ExternalController::<_, 1>::new(transport);
    let mut resources: HostResources<_, DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let mac = esp_hal::efuse::base_mac_address();
    let mac = mac.as_bytes();
    // Who this board is (EEPROM) and the card's settings (CONFIG.TXT; the card task reads it
    // within about two seconds of boot, or gives up).
    let identity = emotibit_ble::board::identity(&mut i2c).await;
    // Settings: CONFIG.TXT on the card wins, then what was set over Bluetooth (flash).
    let mut flash = esp_storage::FlashStorage::new(peripherals.FLASH).multicore_auto_park();
    let stored = emotibit_ble::settings::load(&mut flash);
    let deadline = embassy_time::Instant::now() + Duration::from_secs(4);
    while !control::CONFIG_DONE.load(core::sync::atomic::Ordering::Relaxed) && embassy_time::Instant::now() < deadline {
        Timer::after(Duration::from_millis(50)).await;
    }
    let card = control::CONFIG.lock(|c| *c.borrow());
    let (config, source) = match (card, stored) {
        (Some(c), _) => (c, control::Source::Card),
        (None, Some(c)) => (c, control::Source::Flash),
        (None, None) => (control::Config::default(), control::Source::Default),
    };
    control::STORED.lock(|s| *s.borrow_mut() = (stored.unwrap_or_default(), source, false));
    let board = {
        use core::fmt::Write as _;
        let mut b = control::Board::new();
        if let Some(id) = &identity {
            b.device_id = id.device_id;
            b.hardware = id.hardware;
            b.sku = id.sku;
            control::FOUND.fetch_or(control::found::EEPROM, core::sync::atomic::Ordering::Relaxed);
        }
        // Advertised as "EmotiBit-BLE-<name>" (22 bytes at most): a configured name, the serial
        // number from the device id (MD-V7-0001010 -> 0001010), or MAC bytes.
        match b.device_id.as_str().rsplit('-').next().filter(|s| s.len() == 7) {
            Some(sn) => {
                let _ = b.default_name.write_str(sn);
            }
            None => {
                let _ = write!(b.default_name, "{:02X}{:02X}", mac[4], mac[5]);
            }
        }
        match &config.name {
            Some(n) => {
                let _ = b.name.write_str(n.as_str().get(..9).unwrap_or(n.as_str()));
            }
            None => b.name = b.default_name,
        }
        b.ppg_hz = config.ppg_hz.unwrap_or(25);
        b.leds = config.leds.unwrap_or(true);
        control::BOARD.lock(|cell| *cell.borrow_mut() = b.clone());
        b
    };
    let name: &'static str = alloc::boxed::Box::leak(alloc::format!("EmotiBit-BLE-{}", board.name.as_str()).into_boxed_str());
    // Static random address derived from the chip MAC, so each board is distinct, from the GATT
    // layout version and from the name: centrals (macOS in particular) cache a device's GATT
    // table and name by address, so a new layout or name needs a new address to show.
    let name_hash = name.bytes().fold(0x811C_9DC5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    let mut addr = [mac[5] ^ GATT_LAYOUT, mac[4] ^ name_hash as u8, mac[3] ^ (name_hash >> 8) as u8, mac[2], mac[1], mac[0]];
    addr[5] |= 0xC0; // static random addresses have the two top bits set
    let address = Address::random(addr);
    info!(
        "board {} {} {}, BLE address {:?}, name {}",
        if board.device_id.as_str().is_empty() { "(no id)" } else { board.device_id.as_str() },
        board.hardware.as_str(),
        board.sku.as_str(),
        address,
        name
    );

    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .build();
    let mut peripheral = stack.peripheral();
    let mut runner = stack.runner();

    let server = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name,
        appearance: &appearance::sensor::GENERIC_SENSOR,
    }))
    .unwrap();

    let radio = async {
        loop {
            // Wireless off: no advertising until a button press (or hibernate).
            while matches!(control::power(), Power::WirelessOff | Power::Hibernate) {
                Timer::after(Duration::from_millis(100)).await;
            }
            let mut adv_data = [0u8; 31];
            let len = AdStructure::encode_slice(
                &[
                    AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
                    AdStructure::CompleteLocalName(name.as_bytes()),
                ],
                &mut adv_data[..],
            )
            .unwrap();
            let advertiser = match peripheral
                .advertise(
                    &Default::default(),
                    Advertisement::ConnectableScannableUndirected { adv_data: &adv_data[..len], scan_data: &[] },
                )
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    warn!("advertise: {:?}", e);
                    Timer::after(Duration::from_secs(1)).await;
                    continue;
                }
            };
            // Stop advertising if the power mode turns the radio off meanwhile.
            let accepted = select(advertiser.accept(), async {
                while !matches!(control::power(), Power::WirelessOff | Power::Hibernate) {
                    Timer::after(Duration::from_millis(100)).await;
                }
            })
            .await;
            let conn = match accepted {
                Either::First(Ok(c)) => c,
                _ => continue,
            };
            let conn = match conn.with_attribute_server(&server) {
                Ok(c) => c,
                Err(_) => continue,
            };
            info!("connected");
            control::CONNECTED.store(true, core::sync::atomic::Ordering::Relaxed);
            session(&stack, &server, &conn).await;
            control::CONNECTED.store(false, core::sync::atomic::Ordering::Relaxed);
            info!("session ended");
        }
    };

    // Short press: wireless off <-> normal; long press (3 s): hibernate, as the stock firmware.
    // Hibernate (from the button or the host) closes the recording, powers the sensors down and
    // deep-sleeps until the button is pressed.
    let buttons = async {
        let mut pressed_at: Option<Instant> = None;
        let mut long_done = false;
        loop {
            Timer::after(Duration::from_millis(20)).await;
            if control::power() == Power::Hibernate {
                break;
            }
            control::BUTTON_HELD.store(button.is_high(), core::sync::atomic::Ordering::Relaxed);
            match (button.is_high(), pressed_at) {
                (true, None) => {
                    pressed_at = Some(Instant::now());
                    long_done = false;
                }
                (true, Some(t)) if !long_done && t.elapsed() >= Duration::from_secs(3) => {
                    long_done = true;
                    control::emit("BL", &[], true);
                    control::set_power(Power::Hibernate);
                }
                (false, Some(t)) => {
                    pressed_at = None;
                    if !long_done && t.elapsed() >= Duration::from_millis(50) {
                        control::emit("BS", &[], true);
                        let next = if control::power() == Power::Normal { Power::WirelessOff } else { Power::Normal };
                        control::set_power(next);
                    }
                }
                _ => {}
            }
        }
        info!("hibernate: closing the recording");
        let _ = control::SD_CMD.try_send(control::SdCmd::End);
        Timer::after(Duration::from_millis(1500)).await;
        vreg.set_low();
        // Wake on the button; wait for it to be released first or it wakes us at once.
        while button.is_high() {
            Timer::after(Duration::from_millis(20)).await;
        }
        button.listen(esp_hal::gpio::Event::HighLevel);
        let wake = esp_hal::gpio::WakeupConfig::default().with_low_power_path(true);
        if button.apply_wakeup_config(&wake).is_err() {
            warn!("button cannot wake the chip");
        }
        info!("hibernate: deep sleep until the button is pressed");
        Timer::after(Duration::from_millis(50)).await;
        lpwr.sleep_deep(esp_hal::rtc_cntl::sleep::RtcSleepConfig::deep())
    };

    // Saves settings set over Bluetooth; restarts on request (`CR`).
    let settings = async {
        loop {
            match select(control::SAVE.receive(), async {
                while !control::RESTART.load(core::sync::atomic::Ordering::Relaxed) {
                    Timer::after(Duration::from_millis(100)).await;
                }
            })
            .await
            {
                Either::First(cfg) => {
                    let result = match &cfg {
                        Some(c) => emotibit_ble::settings::save(&mut flash, c),
                        None => emotibit_ble::settings::clear(&mut flash),
                    };
                    match result {
                        Ok(()) => info!("settings {} in flash", if cfg.is_some() { "saved" } else { "cleared" }),
                        Err(e) => warn!("settings: flash write failed ({:?})", e),
                    }
                }
                Either::Second(()) => {
                    info!("restart requested");
                    let _ = control::SD_CMD.try_send(control::SdCmd::End);
                    Timer::after(Duration::from_millis(1500)).await;
                    esp_hal::system::software_reset();
                }
            }
        }
    };

    let _ = join5(
        sensors::run(i2c, &mut read_battery, board.clone(), identity.as_ref().map(|i| i.hw_version)),
        async {
            loop {
                if let Err(e) = runner.run().await {
                    warn!("ble runner: {:?}", e);
                }
            }
        },
        radio,
        buttons,
        settings,
    )
    .await;
    loop {
        Timer::after(Duration::from_secs(1)).await;
    }
}

/// One connection: stream data and events, handle writes, tune the link, report status.
/// Returns when the central disconnects or the power mode turns the radio off.
async fn session(
    stack: &Stack<'_, ExternalController<BleConnector<'static>, 1>, DefaultPacketPool>,
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, DefaultPacketPool>,
) {
    let streaming = stream(server, conn);
    let events = async {
        loop {
            match conn.next().await {
                GattConnectionEvent::Disconnected { reason } => {
                    info!("disconnected: {:?}", reason);
                    break;
                }
                GattConnectionEvent::Gatt { event } => {
                    if let GattEvent::Write(w) = &event {
                        if w.handle() == server.emotibit.mode.handle {
                            if let Ok(m) = w.value(&server.emotibit.mode) {
                                sensors::MODE.store(m, core::sync::atomic::Ordering::Relaxed);
                                sensors::RESYNC.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                                info!("wire format {}", m);
                            }
                        } else if w.handle() == server.emotibit.control.handle
                            && let Ok(bytes) = w.value(&server.emotibit.control) {
                                control::handle(&bytes);
                            }
                    }
                    if let Ok(reply) = event.accept() {
                        reply.send().await;
                    }
                }
                GattConnectionEvent::ConnectionParamsUpdated { conn_interval, peripheral_latency, supervision_timeout } => {
                    info!(
                        "connection interval {} us, latency {}, timeout {} ms",
                        conn_interval.as_micros(),
                        peripheral_latency,
                        supervision_timeout.as_millis()
                    );
                }
                GattConnectionEvent::DataLengthUpdated { max_tx_octets, max_rx_octets, .. } => {
                    info!("data length tx {} rx {}", max_tx_octets, max_rx_octets);
                }
                GattConnectionEvent::PhyUpdated { tx_phy, rx_phy } => {
                    info!("phy tx {:?} rx {:?}", tx_phy, rx_phy);
                }
                _ => {}
            }
        }
    };
    // Ask for long link-layer packets (a full notification in two radio packets instead of ~19)
    // and a short connection interval within Apple's accessory guidelines (min >= 15 ms,
    // max >= min + 15 ms); in max-low power a slow one. The central may refuse either. Ends the
    // session when the power mode turns the radio off.
    let tune = async {
        Timer::after(Duration::from_millis(500)).await;
        if let Err(e) = conn.raw().update_data_length(stack, 251, 2120).await {
            warn!("data length request: {:?}", e);
        }
        let mut applied: Option<bool> = None;
        loop {
            let power = control::power();
            if matches!(power, Power::WirelessOff | Power::Hibernate) {
                // Report the new mode before the link goes.
                let _ = control::EVENTS.try_send(control::mode_line());
                Timer::after(Duration::from_millis(300)).await;
                conn.raw().disconnect();
                Timer::after(Duration::from_millis(300)).await;
                return;
            }
            let slow = power == Power::MaxLow;
            if applied != Some(slow) {
                applied = Some(slow);
                let (min, max) = if slow { (400, 500) } else { (15, 30) };
                let params = RequestedConnParams {
                    min_connection_interval: Duration::from_millis(min),
                    max_connection_interval: Duration::from_millis(max),
                    max_latency: 0,
                    min_event_length: Duration::from_millis(0),
                    max_event_length: Duration::from_millis(0),
                    supervision_timeout: Duration::from_secs(6),
                };
                if let Err(e) = conn.raw().update_connection_params(stack, &params).await {
                    warn!("connection parameter request: {:?}", e);
                }
            }
            Timer::after(Duration::from_millis(200)).await;
        }
    };
    // Mode once a second; a time-sync request (RD) every 5 s, as the stock firmware.
    let status = async {
        let mut n = 0u32;
        loop {
            let _ = control::EVENTS.try_send(control::mode_line());
            if n % 5 == 1 {
                control::emit("RD", &["TL", "TU"], true);
            }
            n += 1;
            Timer::after(Duration::from_secs(1)).await;
        }
    };
    if let embassy_futures::select::Either4::First(_) = select4(streaming, events, tune, status).await {
        // The stream stopped first; report why the link went.
        if let GattConnectionEvent::Disconnected { reason } = conn.next().await {
            info!("disconnected: {:?}", reason);
        }
    }
}

/// Forward queued sensor records and event lines to the connected central. Records are batched
/// into one notification as far as the negotiated ATT MTU allows, in the wire format the host
/// selected.
async fn stream<P: PacketPool>(server: &Server<'_>, conn: &GattConnection<'_, '_, P>) {
    use core::sync::atomic::Ordering::Relaxed;
    // Drop anything queued while nobody was connected, and start the counters fresh.
    while sensors::QUEUE.try_receive().is_ok() {}
    while control::EVENTS.try_receive().is_ok() {}
    sensors::DROPPED.store(0, Relaxed);
    sensors::BYTES.store(0, Relaxed);
    let mut link_seq: u16 = 0;
    let mut clock_seq: u16 = 0;
    let mut next_link = embassy_time::Instant::now();
    let mut encoder = emotibit_ble::codec::Encoder::new();
    let mut resync = sensors::RESYNC.load(Relaxed);
    loop {
        let (first_ts, first) = match select(sensors::QUEUE.receive(), control::EVENTS.receive()).await {
            Either::First(p) => p,
            Either::Second(line) => {
                let mut payload: heapless::Vec<u8, 480> = heapless::Vec::new();
                let _ = payload.extend_from_slice(line.as_bytes());
                if !send(conn, || server.emotibit.events.notify(conn, &payload, false)).await {
                    break;
                }
                continue;
            }
        };
        let mode = sensors::MODE.load(Relaxed);
        let now_resync = sensors::RESYNC.load(Relaxed);
        if now_resync != resync {
            resync = now_resync;
            encoder.reset(); // every tag sends a key frame next
        }
        let room = (conn.raw().att_mtu() as usize).saturating_sub(3).min(MAX_VALUE);
        let mut wire = [0u8; MAX_VALUE];
        let mut len = 0usize;
        let mut records = 0u32;
        let put = |rec: &[u8], wire: &mut [u8; MAX_VALUE], len: &mut usize, encoder: &mut emotibit_ble::codec::Encoder| -> bool {
            if mode == 1 {
                encoder.encode(rec, &mut wire[..room], len)
            } else if *len + rec.len() <= room {
                wire[*len..*len + rec.len()].copy_from_slice(rec);
                *len += rec.len();
                true
            } else {
                false
            }
        };
        // The device time of this batch, as a stock packet's timestamp: the first record's.
        let mut clock = [0u8; 20];
        clock[0] = sensors::tag::CLOCK;
        clock[1] = 1;
        clock[2..4].copy_from_slice(&clock_seq.to_le_bytes());
        clock_seq = clock_seq.wrapping_add(1);
        clock[4..8].copy_from_slice(&first_ts.to_le_bytes());
        put(&clock, &mut wire, &mut len, &mut encoder);
        put(&first, &mut wire, &mut len, &mut encoder);
        records += 1;
        // Keep room for the diagnostics record (at most 20 bytes).
        while len + 20 < room {
            match sensors::QUEUE.try_receive() {
                Ok((_, p)) => {
                    if !put(&p, &mut wire, &mut len, &mut encoder) {
                        break;
                    }
                    records += 1;
                }
                Err(_) => break,
            }
        }
        if embassy_time::Instant::now() >= next_link {
            next_link = embassy_time::Instant::now() + Duration::from_secs(1);
            let mut rec = [0u8; 20];
            rec[0] = sensors::tag::LINK;
            rec[1] = 4;
            rec[2..4].copy_from_slice(&link_seq.to_le_bytes());
            link_seq = link_seq.wrapping_add(1);
            rec[4..8].copy_from_slice(&sensors::DROPPED.load(Relaxed).to_le_bytes());
            rec[8..12].copy_from_slice(&(conn.raw().att_mtu() as u32).to_le_bytes());
            rec[12..16].copy_from_slice(&records.to_le_bytes());
            rec[16..20].copy_from_slice(&sensors::BYTES.load(Relaxed).to_le_bytes());
            put(&rec, &mut wire, &mut len, &mut encoder);
        }
        sensors::BYTES.fetch_add(len as u32, Relaxed);
        let mut payload: heapless::Vec<u8, MAX_VALUE> = heapless::Vec::new();
        let _ = payload.extend_from_slice(&wire[..len]);
        if !send(conn, || server.emotibit.data.notify(conn, &payload, false)).await {
            break;
        }
    }
}

/// Sends a notification, retrying while the host stack is short of buffers (a file download
/// and the data stream at once can exhaust its packet pool). False once the link is gone.
async fn send<P: PacketPool, F, Fut>(conn: &GattConnection<'_, '_, P>, notify: F) -> bool
where
    F: Fn() -> Fut,
    Fut: core::future::Future<Output = Result<(), trouble_host::Error>>,
{
    for attempt in 0..50 {
        match notify().await {
            Ok(()) => return true,
            Err(e) => {
                if !conn.raw().is_connected() {
                    warn!("notify: {:?}; link is down", e);
                    return false;
                }
                if attempt == 0 {
                    warn!("notify: {:?}; retrying", e);
                }
                Timer::after(Duration::from_millis(10)).await;
            }
        }
    }
    false
}

// The ESP32 Bluetooth controller blob references three classic-BT (inquiry/page scan)
// scheduler callbacks that this ROM linker script does not define. BLE never calls them.
#[unsafe(no_mangle)]
extern "C" fn ld_iscan_evt_start_cbk() {}
#[unsafe(no_mangle)]
extern "C" fn ld_page_evt_start_cbk() {}
#[unsafe(no_mangle)]
extern "C" fn ld_pscan_evt_start_cbk() {}
