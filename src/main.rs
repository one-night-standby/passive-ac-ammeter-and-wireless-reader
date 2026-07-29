#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_mspm0::gpio::{Level, Output};
use embassy_mspm0::peripherals;
use embassy_time::Timer;
use panic_halt as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let _ = embassy_mspm0::init(Default::default());
    // SAFETY: this firmware creates exactly one PA0 owner.
    let mut led = Output::new(unsafe { peripherals::PA0::steal() }, Level::Low);
    led.set_inversion(true);
    loop {
        led.toggle();
        Timer::after_millis(400).await;
    }
}
