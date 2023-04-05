#![no_main]
#![no_std]
#![feature(type_alias_impl_trait)]

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::config::Config;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_time::{Duration, Timer};
use panic_probe as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // to enable more verbose logs, go to your `Cargo.toml` and set defmt logging levels
    // to `defmt-trace` by changing the `default = []` entry in `[features]`

    defmt::info!("initializing");
    let peripherals = embassy_nrf::init(Config::default());
    let mut led1 = Output::new(peripherals.P0_06, Level::High, OutputDrive::Standard);
    let mut led2_red = Output::new(peripherals.P0_08, Level::High, OutputDrive::Standard);
    let mut led2_green = Output::new(peripherals.P1_09, Level::High, OutputDrive::Standard);
    let mut led2_blue = Output::new(peripherals.P0_12, Level::High, OutputDrive::Standard);

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
