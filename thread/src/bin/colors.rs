#![no_main]
#![no_std]

use core::sync::atomic::{self, AtomicU32, Ordering};
use core::time::Duration;

use cortex_m::asm;
use cortex_m_rt::entry;
use defmt_rtt as _;
use hal::gpio::{p0, p1, Level};
use hal::pac::{interrupt, RTC0};
use hal::prelude::OutputPin;
use panic_probe as _;

#[entry]
fn main() -> ! {
    // to enable more verbose logs, go to your `Cargo.toml` and set defmt logging levels
    // to `defmt-trace` by changing the `default = []` entry in `[features]`

    defmt::info!("initializing");
    let peripherals = hal::pac::Peripherals::take().unwrap();
    let mut timer = Timer::new(peripherals.TIMER0);
    let pins_0 = p0::Parts::new(peripherals.P0);
    let pins_1 = p1::Parts::new(peripherals.P1);

    let mut led1 = pins_0.p0_06.into_push_pull_output(Level::High);
    let mut led2_red = pins_0.p0_08.into_push_pull_output(Level::Low);
    let mut led2_green = pins_1.p1_09.into_push_pull_output(Level::High);
    let mut led2_blue = pins_0.p0_12.into_push_pull_output(Level::High);

    let mut color = Color::Red;
    let mut cycles = 5;
    loop {
        for _ in 0..4 {
            led1.set_low().unwrap();
            timer.wait(Duration::from_millis(100));
            led1.set_high().unwrap();
            timer.wait(Duration::from_millis(100));
        }

        color = match color {
            Color::Red => {
                led2_green.set_low().unwrap();
                Color::RedGreen
            }
            Color::RedGreen => {
                led2_red.set_high().unwrap();
                Color::Green
            }
            Color::Green => {
                led2_blue.set_low().unwrap();
                Color::GreenBlue
            }
            Color::GreenBlue => {
                led2_green.set_high().unwrap();
                Color::Blue
            }
            Color::Blue => {
                led2_red.set_low().unwrap();
                Color::BlueRed
            }
            Color::BlueRed => {
                led2_blue.set_high().unwrap();
                cycles -= 1;
                if cycles == 0 {
                    break;
                }

                Color::Red
            }
        };
    }

    unsafe {
        // turn off the USB D+ pull-up before pausing the device with a breakpoint
        // this disconnects the nRF device from the USB host so the USB host won't attempt further
        // USB communication (and see an unresponsive device). probe-run will also reset the nRF's
        // USBD peripheral when it sees the device in a halted state which has the same effect as
        // this line but that can take a while and the USB host may issue a power cycle of the USB
        // port / hub / root in the meantime, which can bring down the probe and break probe-run
        const USBD_USBPULLUP: *mut u32 = 0x4002_7504 as *mut u32;
        USBD_USBPULLUP.write_volatile(0)
    }

    defmt::println!("done");
    // force any pending memory operation to complete before the BKPT instruction that follows
    atomic::compiler_fence(Ordering::SeqCst);
    loop {
        asm::bkpt()
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

struct Timer {
    inner: hal::Timer<hal::pac::TIMER0>,
}

impl Timer {
    fn new(timer: hal::pac::TIMER0) -> Self {
        Self {
            inner: hal::Timer::new(timer),
        }
    }

    fn wait(&mut self, duration: Duration) {
        defmt::trace!("blocking for {:?} ...", duration);

        // 1 cycle = 1 microsecond
        let subsec_micros = duration.subsec_nanos() / NANOS_IN_ONE_MICRO;
        if subsec_micros != 0 {
            self.inner.delay(subsec_micros);
        }

        // maximum number of seconds that fit in a single `delay` call without overflowing the `u32`
        // argument

        let mut secs = duration.as_secs();
        while secs != 0 {
            let cycles = if secs > MAX_SECS as u64 {
                secs -= MAX_SECS as u64;
                MAX_SECS * MICROS_IN_ONE_SEC
            } else {
                let cycles = secs as u32 * MICROS_IN_ONE_SEC;
                secs = 0;
                cycles
            };

            self.inner.delay(cycles)
        }

        defmt::trace!("... DONE");
    }
}

// Counter of OVERFLOW events -- an OVERFLOW occurs every (1<<24) ticks
static OVERFLOWS: AtomicU32 = AtomicU32::new(0);

// NOTE this will run at the highest priority, higher priority than RTIC tasks
#[interrupt]
fn RTC0() {
    let curr = OVERFLOWS.load(Ordering::Relaxed);
    OVERFLOWS.store(curr + 1, Ordering::Relaxed);

    // clear the EVENT register
    unsafe { core::mem::transmute::<_, RTC0>(()).events_ovrflw.reset() }
}

// same panicking *behavior* as `panic-probe` but doesn't print a panic message
// this prevents the panic message being printed *twice* when `defmt::panic` is invoked
#[defmt::panic_handler]
fn panic() -> ! {
    cortex_m::asm::udf()
}

const NANOS_IN_ONE_MICRO: u32 = 1_000;
const MICROS_IN_ONE_SEC: u32 = 1_000_000;
const MAX_SECS: u32 = u32::MAX / MICROS_IN_ONE_SEC;
