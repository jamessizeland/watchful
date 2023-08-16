#![cfg_attr(not(test), no_std)]
#![no_main]
#![feature(type_alias_impl_trait)]

use core::cell::RefCell;

use defmt::{info, warn};
use embassy_boot::State as FwState;
use embassy_boot_nrf::{AlignedBuffer, BlockingFirmwareState as FirmwareState, FirmwareUpdaterConfig};
use embassy_embedded_hal::shared_bus::blocking::spi::SpiDevice;
use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::interrupt::Priority;
use embassy_nrf::peripherals::{P0_05, P0_18, P0_25, P0_26, TWISPI0};
use embassy_nrf::spim::Spim;
use embassy_nrf::spis::MODE_3;
use embassy_nrf::{bind_interrupts, pac, peripherals, spim, twim};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::blocking_mutex::Mutex as BMutex;
use embassy_time::{Delay, Duration, Timer};
use embedded_graphics::pixelcolor::Rgb565 as Rgb;
use embedded_graphics::prelude::*;
use embedded_storage::nor_flash::NorFlash;
use heapless::Vec;
use mipidsi::models::ST7789;
use nrf_dfu_target::prelude::*;
use nrf_softdevice::ble::gatt_server::NotifyValueError;
use nrf_softdevice::ble::{gatt_client, gatt_server, peripheral, Connection};
use nrf_softdevice::{raw, Softdevice};
use pinetime_flash::XtFlash;
use static_cell::StaticCell;
use watchful_ui::{FirmwareDetails, FirmwareView, MenuChoice, MenuView, WatchView};
use {defmt_rtt as _, panic_probe as _};

mod clock;
use crate::clock::clock;
use crate::my_display_interface::SPIDeviceInterface;

bind_interrupts!(struct Irqs {
    SPIM0_SPIS0_TWIM0_TWIS0_SPI0_TWI0 => spim::InterruptHandler<peripherals::TWISPI0>;
    SPIM1_SPIS1_TWIM1_TWIS1_SPI1_TWI1 => twim::InterruptHandler<peripherals::TWISPI1>;

});

const MTU: usize = 120;
// Aligned to 4 bytes + 3 bytes for header
const ATT_MTU: usize = MTU + 3;

type Target = DfuTarget<256>;

static CLOCK: clock::Clock = clock::Clock::new();

type Flash = XtFlash<SpiDevice<'static, NoopRawMutex, Spim<'static, TWISPI0>, Output<'static, P0_05>>>;
type Display = mipidsi::Display<
    SPIDeviceInterface<
        SpiDevice<'static, NoopRawMutex, Spim<'static, TWISPI0>, Output<'static, P0_25>>,
        Output<'static, P0_18>,
    >,
    ST7789,
    Output<'static, P0_26>,
>;

fn firmware_details(validated: bool) -> FirmwareDetails {
    const CARGO_NAME: &str = env!("CARGO_PKG_NAME");
    const CARGO_VERSION: &str = env!("CARGO_PKG_VERSION");
    const COMMIT: &str = env!("VERGEN_GIT_SHA");
    const BUILD_TIMESTAMP: &str = env!("VERGEN_BUILD_TIMESTAMP");

    FirmwareDetails::new(CARGO_NAME, CARGO_VERSION, COMMIT, BUILD_TIMESTAMP, validated)
}

#[embassy_executor::main]
async fn main(s: Spawner) {
    let mut config = embassy_nrf::config::Config::default();
    config.gpiote_interrupt_priority = Priority::P2;
    config.time_interrupt_priority = Priority::P2;
    let p = embassy_nrf::init(config);

    let sd = enable_softdevice("Pinetime Embassy");

    static GATT: StaticCell<PineTimeServer> = StaticCell::new();
    let server = GATT.init(PineTimeServer::new(sd).unwrap());

    s.spawn(softdevice_task(sd)).unwrap();
    s.spawn(watchdog_task()).unwrap();
    s.spawn(clock(&CLOCK)).unwrap();

    // Touch peripheral
    let mut twim_config = twim::Config::default();
    twim_config.frequency = twim::Frequency::K400;
    let i2c = twim::Twim::new(p.TWISPI1, Irqs, p.P0_06, p.P0_07, twim_config);
    // setup touchpad external interrupt pin: P0.28/AIN4 (TP_INT)
    let touch_int = Input::new(p.P0_28, Pull::Up);
    // setup touchpad reset pin: P0.10/NFC2 (TP_RESET)
    let touch_rst = Output::new(p.P0_10, Level::High, OutputDrive::Standard);

    let mut touchpad = cst816s::CST816S::new(i2c, touch_int, touch_rst);
    touchpad.setup(&mut embassy_time::Delay).unwrap();

    // Button enable
    let _btn_enable = Output::new(p.P0_15, Level::High, OutputDrive::Standard);

    let mut btn = Input::new(p.P0_13, Pull::Down);

    // Medium backlight
    let _backlight = Output::new(p.P0_22, Level::Low, OutputDrive::Standard);

    // Reset pin
    let rst = Output::new(p.P0_26, Level::Low, OutputDrive::Standard);

    // Keep low while driving display
    let display_cs = Output::new(p.P0_25, Level::High, OutputDrive::Standard);

    // Data/clock
    let dc = Output::new(p.P0_18, Level::Low, OutputDrive::Standard);

    let mut default_config = spim::Config::default();
    default_config.frequency = spim::Frequency::M8;
    default_config.mode = MODE_3;

    let spim = spim::Spim::new(p.TWISPI0, Irqs, p.P0_02, p.P0_04, p.P0_03, default_config);
    static SPI_BUS: StaticCell<BMutex<NoopRawMutex, RefCell<Spim<'static, TWISPI0>>>> = StaticCell::new();
    let spi_bus = SPI_BUS.init(BMutex::new(RefCell::new(spim)));

    // Create a DisplayInterface from SPI and DC pin.
    let display_spi = SpiDevice::new(spi_bus, display_cs);

    // Create flash device
    let flash_cs = Output::new(p.P0_05, Level::High, OutputDrive::Standard);
    let flash_spi = SpiDevice::new(spi_bus, flash_cs);
    let xt_flash = XtFlash::new(flash_spi).unwrap();
    static FLASH: StaticCell<BMutex<NoopRawMutex, RefCell<Flash>>> = StaticCell::new();
    let flash = FLASH.init(BMutex::new(RefCell::new(xt_flash)));

    s.spawn(advertiser_task(s, sd, server, flash, "Pinetime Embassy"))
        .unwrap();

    let di = SPIDeviceInterface::new(display_spi, dc);

    // create the ILI9486 display driver from the display interface and optional RST pin
    let mut display = mipidsi::Builder::st7789(di)
        .with_display_size(240, 240)
        .with_invert_colors(mipidsi::ColorInversion::Inverted)
        .init(&mut Delay, Some(rst))
        .unwrap();

    display.set_orientation(mipidsi::Orientation::Portrait(false)).unwrap();

    let config = FirmwareUpdaterConfig::from_linkerfile_blocking(flash);
    let mut magic = AlignedBuffer([0; 1]);
    let mut fw = FirmwareState::new(config.state, &mut magic.0);
    let mut state = WatchState::Idle;
    loop {
        match state {
            WatchState::Idle => {
                // TODO: Power save
                display.clear(Rgb::BLACK).unwrap();
                btn.wait_for_any_edge().await;
                if btn.is_high() {
                    state = WatchState::ViewTime;
                }
                // Idle task wait for reactions
                // select(wait_for_button, wait_for_touch, timeout)
            }
            WatchState::ViewTime => {
                WatchView::new(CLOCK.get()).draw(&mut display).unwrap();
                let mut timeout = Timer::after(Duration::from_secs(10));
                loop {
                    match select3(
                        Timer::after(Duration::from_secs(1)),
                        &mut timeout,
                        btn.wait_for_any_edge(),
                    )
                    .await
                    {
                        Either3::First(_) => {
                            WatchView::new(CLOCK.get()).draw(&mut display).unwrap();
                        }
                        Either3::Second(_) => {
                            state = WatchState::Idle;
                            break;
                        }
                        Either3::Third(_) => {
                            if btn.is_high() {
                                state = WatchState::ViewMenu(MenuView::main());
                                break;
                            }
                        }
                    }
                }
            }
            WatchState::ViewMenu(menu) => {
                menu.draw(&mut display).unwrap();

                match select(btn.wait_for_any_edge(), async {
                    let selected;
                    loop {
                        if let Some(evt) = touchpad.read_one_touch_event(true) {
                            if let cst816s::TouchGesture::SingleClick = evt.gesture {
                                let touched = Point::new(evt.x, evt.y);
                                if let Some(s) = menu.intersects(touched) {
                                    selected = s;
                                    break;
                                }
                            }
                        } else {
                            Timer::after(Duration::from_micros(1)).await;
                        }
                    }
                    selected
                })
                .await
                {
                    Either::First(_) => {
                        if let MenuView::Settings { .. } = menu {
                            state = WatchState::ViewMenu(MenuView::main());
                        } else {
                            state = WatchState::ViewTime;
                        }
                    }
                    Either::Second(selected) => match selected {
                        MenuChoice::Workout => {
                            defmt::info!("Not implemented");
                            state = WatchState::ViewTime;
                        }
                        MenuChoice::FindPhone => {
                            defmt::info!("Not implemented");
                            state = WatchState::ViewTime;
                        }
                        MenuChoice::Settings => {
                            state = WatchState::ViewMenu(MenuView::settings());
                        }
                        MenuChoice::Firmware => {
                            let validated = FwState::Boot == fw.get_state().expect("Failed to read firmware state");
                            state = WatchState::ViewFirmware(FirmwareView::new(firmware_details(validated)));
                        }
                    },
                }
            }
            WatchState::ViewFirmware(view) => {
                view.draw(&mut display).unwrap();

                match select(btn.wait_for_any_edge(), async {
                    loop {
                        if let Some(evt) = touchpad.read_one_touch_event(true) {
                            if let cst816s::TouchGesture::SingleClick = evt.gesture {
                                let touched = Point::new(evt.x, evt.y);
                                if view.validated(touched) {
                                    break;
                                }
                            }
                        } else {
                            Timer::after(Duration::from_millis(1)).await;
                        }
                    }
                })
                .await
                {
                    Either::First(_) => {
                        info!("Button pressed, back to settings");
                        state = WatchState::ViewMenu(MenuView::settings());
                    }
                    Either::Second(_) => {
                        let validated = FwState::Boot == fw.get_state().expect("Failed to read firmware state");
                        if !validated {
                            fw.mark_booted().expect("Failed to mark current firmware as good");
                            info!("Firmware marked as valid");
                            state = WatchState::ViewMenu(MenuView::main());
                        } else {
                            state = WatchState::ViewFirmware(FirmwareView::new(firmware_details(validated)));
                        }
                    }
                }
            } /*
              WatchState::Workout => {
                  // start pulse reading
                  // start accelerometer reading
                  // display exercise view
                  // refresh display until timout (go black)
              }
              WatchState::FindPhone => {
                  // try connecting to phone over BLE
                  // tell phone to make some noise
              }*/
        }
        // Main is the 'idle task'
    }
}

pub enum WatchState<'a> {
    Idle,
    ViewTime,
    ViewMenu(MenuView<'a>),
    ViewFirmware(FirmwareView),
    //  FindPhone,
    //  Workout,
}

#[nrf_softdevice::gatt_service(uuid = "FE59")]
pub struct NrfDfuService {
    #[characteristic(uuid = "8EC90001-F315-4F60-9FB8-838830DAEA50", write, notify)]
    control: Vec<u8, ATT_MTU>,

    /// The maximum size of each packet is derived from the Att MTU size of the connection.
    /// The maximum Att MTU size of the DFU Service is 256 bytes (saved in NRF_SDH_BLE_GATT_MAX_MTU_SIZE),
    /// making the maximum size of the DFU Packet characteristic 253 bytes. (3 bytes are used for opcode and handle ID upon writing.)
    #[characteristic(uuid = "8EC90002-F315-4F60-9FB8-838830DAEA50", write_without_response, notify)]
    packet: Vec<u8, ATT_MTU>,
}

struct DfuConnection {
    pub connection: Connection,
    pub notify_control: bool,
    pub notify_packet: bool,
}

impl NrfDfuService {
    fn process<DFU: NorFlash, F: FnOnce(&DfuConnection, &[u8]) -> Result<(), NotifyValueError>>(
        &self,
        target: &mut Target,
        dfu: &mut DFU,
        conn: &mut DfuConnection,
        request: DfuRequest<'_>,
        notify: F,
    ) -> DfuStatus {
        let (response, status) = target.process(request, dfu);
        let mut buf: [u8; 32] = [0; 32];
        match response.encode(&mut buf[..]) {
            Ok(len) => match notify(&conn, &buf[..len]) {
                Ok(_) => {}
                Err(e) => {
                    warn!("Error sending notification: {:?}", e);
                }
            },
            Err(e) => {
                warn!("Error encoding DFU response: {:?}", e);
            }
        }
        status
    }

    fn handle<DFU: NorFlash>(
        &self,
        target: &mut Target,
        dfu: &mut DFU,
        connection: &mut DfuConnection,
        event: NrfDfuServiceEvent,
    ) -> Option<DfuStatus> {
        match event {
            NrfDfuServiceEvent::ControlWrite(data) => {
                if let Ok((request, _)) = DfuRequest::decode(&data) {
                    return Some(self.process(target, dfu, connection, request, |conn, response| {
                        if conn.notify_control {
                            self.control_notify(&conn.connection, &Vec::from_slice(response).unwrap())?;
                        }
                        Ok(())
                    }));
                }
            }
            NrfDfuServiceEvent::ControlCccdWrite { notifications } => {
                connection.notify_control = notifications;
            }
            NrfDfuServiceEvent::PacketWrite(data) => {
                let request = DfuRequest::Write { data: &data[..] };
                return Some(self.process(target, dfu, connection, request, |conn, response| {
                    if conn.notify_packet {
                        self.packet_notify(&conn.connection, &Vec::from_slice(response).unwrap())?;
                    }
                    Ok(())
                }));
            }
            NrfDfuServiceEvent::PacketCccdWrite { notifications } => {
                connection.notify_packet = notifications;
            }
        }
        None
    }
}

#[nrf_softdevice::gatt_server]
pub struct PineTimeServer {
    dfu: NrfDfuService,
}

#[nrf_softdevice::gatt_client(uuid = "1805")]
pub struct CurrentTimeServiceClient {
    #[characteristic(uuid = "2a2b", write, read, notify)]
    current_time: Vec<u8, 10>,
}

impl CurrentTimeServiceClient {
    async fn get_time(&self) -> Result<time::PrimitiveDateTime, gatt_client::ReadError> {
        let data = self.current_time_read().await?;
        if data.len() == 10 {
            let year = u16::from_le_bytes([data[0], data[1]]);
            let month = data[2];
            let day = data[3];
            let hour = data[4];
            let minute = data[5];
            let second = data[6];
            let _weekday = data[7];
            let secs_frac = data[8];

            if let Ok(month) = month.try_into() {
                let date = time::Date::from_calendar_date(year as i32, month, day);
                let micros = secs_frac as u32 * 1000000 / 256;
                let time = time::Time::from_hms_micro(hour, minute, second, micros);
                if let (Ok(time), Ok(date)) = (time, date) {
                    let dt = time::PrimitiveDateTime::new(date, time);
                    return Ok(dt);
                }
            }
        }
        Err(gatt_client::ReadError::Truncated)
    }
}

impl PineTimeServer {
    fn handle<DFU: NorFlash>(
        &self,
        target: &mut Target,
        dfu: &mut DFU,
        conn: &mut DfuConnection,
        event: PineTimeServerEvent,
    ) -> Option<DfuStatus> {
        match event {
            PineTimeServerEvent::Dfu(event) => self.dfu.handle(target, dfu, conn, event),
        }
    }
}

#[embassy_executor::task(pool_size = "1")]
pub async fn gatt_server_task(
    conn: Connection,
    server: &'static PineTimeServer,
    flash: &'static BMutex<NoopRawMutex, RefCell<Flash>>,
) {
    let p = unsafe { pac::Peripherals::steal() };
    let part = p.FICR.info.part.read().part().bits();
    let variant = p.FICR.info.variant.read().variant().bits();

    let hw_info = HardwareInfo {
        part,
        variant,
        rom_size: 0,
        ram_size: 0,
        rom_page_size: 0,
    };

    let fw_info = FirmwareInfo {
        ftype: FirmwareType::Application,
        version: 1,
        addr: 0,
        len: 0,
    };

    let mut dfu_conn = DfuConnection {
        connection: conn.clone(),
        notify_control: false,
        notify_packet: false,
    };

    info!("Running GATT server");
    let mut config = FirmwareUpdaterConfig::from_linkerfile_blocking(flash);
    let mut target = DfuTarget::new(config.dfu.size(), fw_info, hw_info);
    let mut magic = AlignedBuffer([0; 1]);
    let mut state = FirmwareState::new(config.state, &mut magic.0);

    let _ = gatt_server::run(&conn, server, |e| {
        if let Some(DfuStatus::DoneReset) = server.handle(&mut target, &mut config.dfu, &mut dfu_conn, e) {
            info!("Firmware updated!");
            match state.mark_updated() {
                Ok(_) => {
                    info!("Firmware updated, resetting");
                    cortex_m::peripheral::SCB::sys_reset();
                }
                Err(e) => {
                    panic!("Error marking firmware updated: {:?}", e);
                }
            }
        }
    })
    .await;
    info!("Disconnected");
}

#[embassy_executor::task]
pub async fn advertiser_task(
    spawner: Spawner,
    sd: &'static Softdevice,
    server: &'static PineTimeServer,
    flash: &'static BMutex<NoopRawMutex, RefCell<Flash>>,
    name: &'static str,
) {
    let mut adv_data: Vec<u8, 31> = Vec::new();
    #[rustfmt::skip]
    adv_data.extend_from_slice(&[
        0x02, 0x01, raw::BLE_GAP_ADV_FLAGS_LE_ONLY_GENERAL_DISC_MODE as u8,
        0x03, 0x03, 0xFE, 0x59,
        (1 + name.len() as u8), 0x09]).unwrap();

    adv_data.extend_from_slice(name.as_bytes()).ok().unwrap();

    #[rustfmt::skip]
    let scan_data = &[
        0x03, 0x03, 0x0A, 0x18,
    ];

    loop {
        let config = peripheral::Config::default();
        let adv = peripheral::ConnectableAdvertisement::ScannableUndirected {
            adv_data: &adv_data[..],
            scan_data,
        };
        info!("Advertising");
        let conn = peripheral::advertise_connectable(sd, adv, &config).await.unwrap();

        info!("Connection established");
        if let Ok(time_client) = gatt_client::discover::<CurrentTimeServiceClient>(&conn).await {
            info!("Found time server on peer, synchronizing time");
            match time_client.get_time().await {
                Ok(time) => {
                    info!("Got time from peer: {:?}", defmt::Debug2Format(&time));
                    CLOCK.set(time);
                }
                Err(e) => {
                    info!("Error retrieving time: {:?}", e);
                }
            }
        }

        if let Err(e) = spawner.spawn(gatt_server_task(conn, server, flash)) {
            defmt::info!("Error spawning gatt task: {:?}", e);
        }
    }
}

fn enable_softdevice(name: &'static str) -> &'static mut Softdevice {
    let config = nrf_softdevice::Config {
        clock: Some(raw::nrf_clock_lf_cfg_t {
            source: raw::NRF_CLOCK_LF_SRC_RC as u8,
            rc_ctiv: 4,
            rc_temp_ctiv: 2,
            accuracy: 7,
        }),
        conn_gap: Some(raw::ble_gap_conn_cfg_t {
            conn_count: 2,
            event_length: 24,
        }),
        conn_gatt: Some(raw::ble_gatt_conn_cfg_t {
            att_mtu: ATT_MTU as u16,
        }),
        gatts_attr_tab_size: Some(raw::ble_gatts_cfg_attr_tab_size_t { attr_tab_size: 32768 }),
        gap_role_count: Some(raw::ble_gap_cfg_role_count_t {
            adv_set_count: 1,
            periph_role_count: 3,
            central_role_count: 1,
            central_sec_count: 1,
            _bitfield_1: Default::default(),
        }),
        gap_device_name: Some(raw::ble_gap_cfg_device_name_t {
            p_value: name.as_ptr() as *const u8 as _,
            current_len: name.len() as u16,
            max_len: name.len() as u16,
            write_perm: unsafe { core::mem::zeroed() },
            _bitfield_1: raw::ble_gap_cfg_device_name_t::new_bitfield_1(raw::BLE_GATTS_VLOC_STACK as u8),
        }),
        ..Default::default()
    };
    Softdevice::enable(&config)
}

#[embassy_executor::task]
async fn softdevice_task(sd: &'static Softdevice) {
    sd.run().await;
}

// Keeps our system alive
#[embassy_executor::task]
async fn watchdog_task() {
    let mut handle = unsafe { embassy_nrf::wdt::WatchdogHandle::steal(0) };
    loop {
        handle.pet();
        Timer::after(Duration::from_secs(4)).await;
    }
}

/*
pub enum ViewState {}

pub type Signal = Signal<CriticalSectionRawMutex, Command>;
pub enum Command {
    Stop,
}

struct HealthTracker {}

impl HealthTracker {
    pub async fn run(&mut self) {}
}

#[embassy_executor::task]
async fn exercise(signal: StopSignal, tracker: HealthTracker) {
    loop {
        match select(signal.wait(), tracker.run()).await {
            Either::First(command) => match command {
                Command::Stop => break,
            },
            Either::Second(_) => {}
        }
    }
}
*/

mod my_display_interface {
    use display_interface::{DataFormat, DisplayError, WriteOnlyDataCommand};
    use embedded_hal::digital::OutputPin;
    use embedded_hal::spi::SpiDevice;

    /// SPI display interface.
    ///
    /// This combines the SPI peripheral and a data/command pin
    pub struct SPIDeviceInterface<SPI, DC> {
        spi: SPI,
        dc: DC,
    }

    impl<SPI, DC> SPIDeviceInterface<SPI, DC>
    where
        SPI: SpiDevice,
        DC: OutputPin,
    {
        /// Create new SPI interface for communciation with a display driver
        pub fn new(spi: SPI, dc: DC) -> Self {
            Self { spi, dc }
        }
    }

    impl<SPI, DC> WriteOnlyDataCommand for SPIDeviceInterface<SPI, DC>
    where
        SPI: SpiDevice,
        DC: OutputPin,
    {
        fn send_commands(&mut self, cmds: DataFormat<'_>) -> Result<(), DisplayError> {
            // 1 = data, 0 = command
            self.dc.set_low().map_err(|_| DisplayError::DCError)?;

            send_u8(&mut self.spi, cmds).map_err(|_| DisplayError::BusWriteError)?;
            Ok(())
        }

        fn send_data(&mut self, buf: DataFormat<'_>) -> Result<(), DisplayError> {
            // 1 = data, 0 = command
            self.dc.set_high().map_err(|_| DisplayError::DCError)?;

            send_u8(&mut self.spi, buf).map_err(|_| DisplayError::BusWriteError)?;
            Ok(())
        }
    }

    fn send_u8<T: SpiDevice>(spi: &mut T, words: DataFormat<'_>) -> Result<(), T::Error> {
        match words {
            DataFormat::U8(slice) => spi.write(slice),
            DataFormat::U16(slice) => {
                use byte_slice_cast::*;
                spi.write(slice.as_byte_slice())
            }
            DataFormat::U16LE(slice) => {
                use byte_slice_cast::*;
                for v in slice.as_mut() {
                    *v = v.to_le();
                }
                spi.write(slice.as_byte_slice())
            }
            DataFormat::U16BE(slice) => {
                use byte_slice_cast::*;
                for v in slice.as_mut() {
                    *v = v.to_be();
                }
                spi.write(slice.as_byte_slice())
            }
            DataFormat::U8Iter(iter) => {
                let mut buf = [0; 32];
                let mut i = 0;

                for v in iter.into_iter() {
                    buf[i] = v;
                    i += 1;

                    if i == buf.len() {
                        spi.write(&buf)?;
                        i = 0;
                    }
                }

                if i > 0 {
                    spi.write(&buf[..i])?;
                }

                Ok(())
            }
            DataFormat::U16LEIter(iter) => {
                use byte_slice_cast::*;
                let mut buf = [0; 32];
                let mut i = 0;

                for v in iter.map(u16::to_le) {
                    buf[i] = v;
                    i += 1;

                    if i == buf.len() {
                        spi.write(&buf.as_byte_slice())?;
                        i = 0;
                    }
                }

                if i > 0 {
                    spi.write(&buf[..i].as_byte_slice())?;
                }

                Ok(())
            }
            DataFormat::U16BEIter(iter) => {
                use byte_slice_cast::*;
                let mut buf = [0; 64];
                let mut i = 0;
                let len = buf.len();

                for v in iter.map(u16::to_be) {
                    buf[i] = v;
                    i += 1;

                    if i == len {
                        spi.write(&buf.as_byte_slice())?;
                        i = 0;
                    }
                }

                if i > 0 {
                    spi.write(&buf[..i].as_byte_slice())?;
                }

                Ok(())
            }
            _ => unimplemented!(),
        }
    }
}
