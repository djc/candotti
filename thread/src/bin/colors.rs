#![no_main]
#![no_std]
#![feature(type_alias_impl_trait)]

use core::mem;

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::usb::vbus_detect::{HardwareVbusDetect, VbusDetect};
use embassy_nrf::usb::{Driver, Instance};
use embassy_nrf::{bind_interrupts, pac, peripherals, usb};
use embassy_time::{Duration, Timer};
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use panic_probe as _;
use static_cell::StaticCell;

macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        let (x,) = STATIC_CELL.init(($val,));
        x
    }};
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let clock: pac::CLOCK = unsafe { mem::transmute(()) };
    clock.tasks_hfclkstart.write(|w| unsafe { w.bits(1) });
    while clock.events_hfclkstarted.read().bits() != 1 {}

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
        &mut singleton!([0; 256])[..],
        &mut singleton!([0; 256])[..],
        &mut singleton!([0; 256])[..],
        &mut singleton!([0; 128])[..],
        &mut singleton!([0; 128])[..],
    );

    let state = singleton!(State::new());
    let class = CdcAcmClass::new(&mut builder, state, 64);
    let usb = builder.build();
    defmt::unwrap!(spawner.spawn(usb_task(usb)));
    defmt::unwrap!(spawner.spawn(echo_task(class)));

    let led1 = Output::new(peripherals.P0_06, Level::High, OutputDrive::Standard);
    let led2_red = Output::new(peripherals.P0_08, Level::High, OutputDrive::Standard);
    let led2_green = Output::new(peripherals.P1_09, Level::High, OutputDrive::Standard);
    let led2_blue = Output::new(peripherals.P0_12, Level::High, OutputDrive::Standard);
    defmt::unwrap!(spawner.spawn(blinky(led1, led2_red, led2_green, led2_blue)));
}

#[embassy_executor::task]
async fn blinky(
    mut led1: Output<'static, peripherals::P0_06>,
    mut led2_red: Output<'static, peripherals::P0_08>,
    mut led2_green: Output<'static, peripherals::P1_09>,
    mut led2_blue: Output<'static, peripherals::P0_12>,
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
    POWER_CLOCK => usb::vbus_detect::InterruptHandler;
});

#[embassy_executor::task]
async fn usb_task(
    mut device: UsbDevice<'static, Driver<'static, peripherals::USBD, HardwareVbusDetect>>,
) {
    device.run().await;
}

#[embassy_executor::task]
async fn echo_task(
    mut class: CdcAcmClass<'static, Driver<'static, peripherals::USBD, HardwareVbusDetect>>,
) {
    loop {
        class.wait_connection().await;
        defmt::info!("Connected");
        let _ = echo(&mut class).await;
        defmt::info!("Disconnected");
    }
}

async fn echo<'d, T: Instance + 'd, P: VbusDetect + 'd>(
    class: &mut CdcAcmClass<'d, Driver<'d, T, P>>,
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
