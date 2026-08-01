#![no_std]
#![no_main]
// The shared modules carry items this binary does not use -- it has no panel,
// no button and no bench report -- and every one of them would otherwise be a
// dead_code warning here.
#![allow(dead_code)]

//! Free-running meter: measure, transmit, sleep the analog, repeat.
//!
//! Same measurement as `main.rs` -- literally the same `meter::measure`, so a
//! CAL table filled from one binary is valid in the other -- but triggered by a
//! timer instead of S2 and reported out UART2 to the HC-42 instead of onto the
//! OLED. Between readings the analog blocks are powered down exactly as they
//! are there; what stays up is the radio, because a module that has lost power
//! cannot be woken over the air.
//!
//! Two lines go out per reading. `METER_TEST` is the frame the Android reader
//! parses (`MeterFrameParser.FRAME_PATTERN`), and its regex anchors at the end
//! of the line, so nothing may be appended to it. `METER_CAL` carries what a
//! calibration run needs and the phone silently ignores: the raw RMS, the gain
//! it was taken at, and the probe's view of the input. `cargo xtask cal-log`
//! parses both.

use embassy_executor::Spawner;
use embassy_mspm0::dma::{self, Channel};
use embassy_mspm0::gpio::{Level, Output};
use embassy_mspm0::uart::BufferedInterruptHandler;
use embassy_mspm0::{bind_interrupts, peripherals};
use embassy_time::Timer;
use panic_halt as _;

#[path = "../cal.rs"]
mod cal;
#[path = "../dac.rs"]
mod dac;
#[path = "../dsp.rs"]
mod dsp;
#[path = "../led.rs"]
mod led;
#[path = "../link.rs"]
mod link;
#[path = "../meter.rs"]
mod meter;
#[path = "../range.rs"]
mod range;
#[path = "../sampler.rs"]
mod sampler;
#[path = "../vref.rs"]
mod vref;

use link::{Address, Link};
use meter::Meter;

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
    UART2 => BufferedInterruptHandler<peripherals::UART2>;
});

/// Idle between the end of one reading and the start of the next. The analog
/// blocks are down for all of it, so this is the duty cycle: at 250 ms of
/// measurement per reading, one second gives the front end roughly three
/// quarters of its life powered off, and the phone an update rate that still
/// looks live.
const PERIOD_MS: u64 = 1_000;

/// Radio buffers, sized as in `main.rs`: the frames of one reading, and one
/// command line this binary never reads.
static mut TX_BUF: [u8; 192] = [0; 192];

static mut RX_BUF: [u8; 64] = [0; 64];

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    if let Ok(token) = led::blink(Output::new(p.PA0, Level::Low)) {
        spawner.spawn(token);
    }

    let address = Address::new(p.PB0, p.PB6, p.PB7, p.PB8);
    let mut meter = Meter::new(p.PA8, p.PA26);
    let mut dma = Channel::new(p.DMA_CH0, Irqs);

    // The first reading is taken with the radio still dark. Its start-up
    // current is the largest single draw in this firmware and it lands on a
    // harvested rail that has just come up; putting the measurement first
    // means the two never contend, and by the time the module powers on there
    // is already something to say. `main.rs` cannot do this -- it has to be
    // commandable from the first moment -- but nothing here needs commands.
    let mut reading = meter.measure(&mut dma).await;

    // SAFETY: this is the only place either buffer is named, and the `Link`
    // built from them lives for the rest of the program.
    let Some(mut link) = Link::power_up(
        p.PB17,
        p.UART2,
        p.PB15,
        p.PB16,
        Irqs,
        unsafe { &mut *core::ptr::addr_of_mut!(TX_BUF) },
        unsafe { &mut *core::ptr::addr_of_mut!(RX_BUF) },
    )
    .await
    else {
        // No radio, no reason to keep measuring: this binary has no panel and
        // no button, so the frames are the only output it has.
        panic!("UART2 config rejected");
    };

    loop {
        link.send(address.read(), &reading).await;
        Timer::after_millis(PERIOD_MS).await;
        reading = meter.measure(&mut dma).await;
    }
}
