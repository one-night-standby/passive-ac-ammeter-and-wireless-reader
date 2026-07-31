#![no_std]
#![no_main]

//! "Passive" AC ammeter, circuit A: one reading per press of S2.
//!
//! The cycle is: S2 triggers one `meter::measure`, which switches the external
//! circuit in, takes the reading and puts everything back to sleep, and the
//! result is shown for `DISPLAY_ON_MS` before the panel goes dark. Presses that
//! land inside a cycle are dropped, not queued. Between cycles nothing runs --
//! the executor idles on the S2 edge.
//!
//! This file is the trigger and nothing else. The substance lives in the
//! modules below.
//!   `meter`   -- one reading: AFE control, bring-up, probe, frame, power-down
//!   `sampler` -- TIMG6/ADC1/DMA and the sample buffers
//!   `range`   -- OPA1 PGA, DAC bias pivot, and the autoranger
//!   `dsp`     -- windowing, true RMS
//!   `cal`     -- per-range LSB -> amps tables
//!   `ui`      -- the OLED readout
//!   `led`     -- the power-on indicator

use embassy_executor::Spawner;
use embassy_mspm0::bind_interrupts;
use embassy_mspm0::dma::{self, Channel};
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::peripherals;
use embassy_time::Timer;
use panic_halt as _;

mod cal;
mod dac;
mod dsp;
mod led;
mod meter;
mod oled;
mod range;
mod sampler;
mod ui;
mod vref;

use meter::Meter;
use ui::{DISPLAY_ON_MS, Panel};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    // First thing after the clocks, so the indicator is up before anything
    // that can fail or block: from here on, a dark LED means the firmware
    // never got this far.
    //
    // The task pool is one deep and this is its only spawn, so the token
    // cannot come back `Err`. Handled rather than unwrapped anyway: an
    // indicator that failed to start is a reason to run without it, not a
    // reason to halt a meter that is otherwise fine.
    if let Ok(token) = led::blink(Output::new(p.PA0, Level::Low)) {
        spawner.spawn(token);
    }

    let mut meter = Meter::new(p.PA8, p.PA26);
    let mut dma = Channel::new(p.DMA_CH0, Irqs);
    let mut panel = Panel::new();

    // LaunchPad S2 is the independent user button on PB21. It pulls the pin
    // low when pressed; unlike S1/PA18, it does not disturb the ADC input.
    let mut trigger = Input::new(p.PB21, Pull::Up);

    loop {
        // Idle here, and this is also where a press made during the previous
        // cycle gets dropped: `wait_for_falling_edge` clears pending edge
        // events before it arms, so only edges from here on count.
        trigger.wait_for_falling_edge().await;

        let reading = meter.measure(&mut dma).await;
        panel.show(&reading);
        Timer::after_millis(DISPLAY_ON_MS).await;
        panel.blank();
    }
}
