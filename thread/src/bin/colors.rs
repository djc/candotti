#![no_main]
#![no_std]

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::{HardwareVbusDetect, VbusDetect};
use embassy_nrf::{bind_interrupts, pac, peripherals, usb};
use embassy_time::{Duration, Timer};
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use panic_probe as _;
use static_cell::StaticCell;

macro_rules! singleton {
    ($ty:ty, $val:expr) => {{
        static STATIC_CELL: StaticCell<$ty> = StaticCell::new();
        STATIC_CELL.init($val)
    }};
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    let peripherals = embassy_nrf::init(embassy_nrf::config::Config::default());
    let driver = Driver::new(peripherals.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("XavaMedia");
    config.product = Some("Candotti Border Router");
    config.serial_number = Some("20230001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Required for windows compatiblity.
    // https://developer.nordicsemi.com/nRF_Connect_SDK/doc/1.9.1/kconfig/CONFIG_CDC_ACM_IAD.html#help
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;

    let mut builder = embassy_usb::Builder::new(
        driver,
        config,
        &mut singleton!([u8; 256], [0; 256])[..],
        &mut singleton!([u8; 256], [0; 256])[..],
        &mut singleton!([u8; 128], [0; 128])[..],
        &mut singleton!([u8; 128], [0; 128])[..],
    );

    let state = singleton!(State<'static>, State::new());
    let class = CdcAcmClass::new(&mut builder, state, 64);
    let usb = builder.build();
    spawner.spawn(defmt::unwrap!(usb_task(usb)));
    spawner.spawn(defmt::unwrap!(echo_task(class)));

    let led1 = Output::new(peripherals.P0_06, Level::High, OutputDrive::Standard);
    let led2_red = Output::new(peripherals.P0_08, Level::High, OutputDrive::Standard);
    let led2_green = Output::new(peripherals.P1_09, Level::High, OutputDrive::Standard);
    let led2_blue = Output::new(peripherals.P0_12, Level::High, OutputDrive::Standard);
    spawner.spawn(defmt::unwrap!(blinky(
        led1, led2_red, led2_green, led2_blue
    )));
}

#[embassy_executor::task]
async fn blinky(
    mut led1: Output<'static>,
    mut led2_red: Output<'static>,
    mut led2_green: Output<'static>,
    mut led2_blue: Output<'static>,
) {
    let mut color = Color::Red;
    let mut cycles = 5;
    loop {
        for _ in 0..4 {
            led1.set_low();
            Timer::after(Duration::from_millis(100)).await;
            led1.set_high();
            Timer::after(Duration::from_millis(100)).await;
        }

        color = match color {
            Color::Red => {
                led2_green.set_low();
                Color::RedGreen
            }
            Color::RedGreen => {
                led2_red.set_high();
                Color::Green
            }
            Color::Green => {
                led2_blue.set_low();
                Color::GreenBlue
            }
            Color::GreenBlue => {
                led2_green.set_high();
                Color::Blue
            }
            Color::Blue => {
                led2_red.set_low();
                Color::BlueRed
            }
            Color::BlueRed => {
                led2_blue.set_high();
                cycles -= 1;
                if cycles == 0 {
                    break;
                }

                Color::Red
            }
        };
    }
}

pub enum Color {
    Red,
    RedGreen,
    Green,
    GreenBlue,
    Blue,
    BlueRed,
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, Driver<'static, HardwareVbusDetect>>) {
    device.run().await;
}

#[embassy_executor::task]
async fn echo_task(mut class: CdcAcmClass<'static, Driver<'static, HardwareVbusDetect>>) {
    loop {
        class.wait_connection().await;
        defmt::info!("Connected");
        let _ = echo(&mut class).await;
        defmt::info!("Disconnected");
    }
}

async fn echo<'d, P: VbusDetect + 'd>(
    class: &mut CdcAcmClass<'d, Driver<'d, P>>,
) -> Result<(), Disconnected> {
    let mut buf = [0; 64];
    loop {
        let n = class.read_packet(&mut buf).await?;
        let data = &buf[..n];
        defmt::info!("data: {:x}", data);
        class.write_packet(data).await?;
    }
}

struct Disconnected;

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => panic!("Buffer overflow"),
            EndpointError::Disabled => Disconnected {},
        }
    }
}
