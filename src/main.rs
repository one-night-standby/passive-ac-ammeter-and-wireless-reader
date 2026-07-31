#![no_std]
#![no_main]

//! "Passive" AC ammeter, circuit A: one reading per request.
//!
//! A request is either a press of S2 or a `MEAS` command off the radio; the two
//! are the same trigger and take the same path. One request runs one
//! `meter::measure`, which switches the external circuit in, takes the reading
//! and puts everything back to sleep. The result goes out over the radio and
//! onto the panel, which stays lit for `DISPLAY_ON_MS` and then goes dark.
//!
//! Power-on takes one reading unasked, before the radio is up, and reports it
//! the moment the port opens. So the meter always has a number to give, and a
//! reader that connects before anyone has pressed anything is not told the
//! meter is silent when it is merely idle.
//!
//! Requests that land inside a cycle are dropped, not queued, from either
//! source: `wait_for_falling_edge` clears pending edges before it arms, and
//! `Link::discard_pending` throws away buffered command bytes at the same
//! point. A reading answers the request that started it.
//!
//! Between cycles nothing runs -- the executor idles on the two triggers, with
//! every analog block powered down and the radio up.
//!
//! This file is the trigger and nothing else. The substance lives in the
//! modules below.
//!   `meter`   -- one reading: AFE control, bring-up, probe, frame, power-down
//!   `link`    -- HC-42 on UART2: the address, the frames out, the command in
//!   `sampler` -- TIMG6/ADC1/DMA and the sample buffers
//!   `range`   -- OPA1 PGA, DAC bias pivot, and the autoranger
//!   `dsp`     -- windowing, true RMS
//!   `cal`     -- per-range LSB -> amps tables
//!   `ui`      -- the OLED readout
//!   `led`     -- the power-on indicator

use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_mspm0::dma::{self, Channel};
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::uart::BufferedInterruptHandler;
use embassy_mspm0::{bind_interrupts, peripherals};
use embassy_time::Timer;
use panic_halt as _;

mod cal;
mod dac;
mod dsp;
mod led;
mod link;
mod meter;
mod oled;
mod range;
mod sampler;
mod ui;
mod vref;

use link::{Address, HEARTBEAT_MS, Link};
use meter::Meter;
use ui::{DISPLAY_ON_MS, Panel};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
    UART2 => BufferedInterruptHandler<peripherals::UART2>;
});

/// Radio buffers. TX only has to hold the two frames of one reading; RX only
/// has to hold one command line, and anything past that is a reader talking
/// over itself. Both are `static` because `BufferedUart` keeps them for as long
/// as the port is open, which here is forever.
static mut TX_BUF: [u8; 192] = [0; 192];

static mut RX_BUF: [u8; 64] = [0; 64];

/// Idle until something asks for a reading, announcing this meter's address
/// every `HEARTBEAT_MS` in the meantime.
///
/// The beat is not a trigger -- it goes out and the wait resumes. Only the
/// button and a `MEAS` command return from here, and which one did is not worth
/// distinguishing: they are the same request.
async fn wait_for_request(trigger: &mut Input<'static>, link: &mut Link, address: &Address) {
    loop {
        let addr = address.read();
        match select3(
            trigger.wait_for_falling_edge(),
            link.wait_command(addr),
            Timer::after_millis(HEARTBEAT_MS),
        )
        .await
        {
            Either3::First(()) | Either3::Second(_) => return,
            // Re-read the switch on every beat rather than caching it: moving
            // the switch is the one operation 2(2) allows during a run, so the
            // address has to be able to change between two beats.
            Either3::Third(()) => link.send_alive(address.read()),
        }
    }
}

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
    let address = Address::new(p.PB0, p.PB6, p.PB7, p.PB8);

    // LaunchPad S2 is the independent user button on PB21. It pulls the pin
    // low when pressed; unlike S1/PA18, it does not disturb the ADC input.
    let mut trigger = Input::new(p.PB21, Pull::Up);

    // One reading on power-up, before anything can ask for it, and before the
    // radio exists. Measuring first is what the board notes ask for on a
    // harvested rail (6.1): the module's start-up current is the largest single
    // draw in this firmware, and it should not land while the supply has just
    // come up. Nothing is lost by waiting -- the reading is already in hand
    // when the port opens, so the first frame goes out as soon as there is
    // anything to send it over.
    let mut in_hand = Some(meter.measure(&mut dma).await);

    // SAFETY: this is the only place either buffer is named, and the `Link`
    // built from them lives for the rest of the program.
    let mut link = Link::power_up(
        p.PB17,
        p.UART2,
        p.PB15,
        p.PB16,
        Irqs,
        unsafe { &mut *core::ptr::addr_of_mut!(TX_BUF) },
        unsafe { &mut *core::ptr::addr_of_mut!(RX_BUF) },
    )
    .await;

    loop {
        // The power-on reading is already taken; every later one has to be
        // asked for. Both then travel the identical path below, so the first
        // reading is reported exactly like the ones that follow.
        let reading = match in_hand.take() {
            Some(reading) => reading,
            None => {
                // Idle here on both triggers at once. Whichever arrives first
                // wins and the other future is dropped -- for the button that
                // discards a pending edge, and for the radio it leaves any
                // buffered bytes where they are, which the discard below then
                // clears. Which one won does not matter: the two are the same
                // request.
                match &mut link {
                    Some(link) => wait_for_request(&mut trigger, link, &address).await,
                    // A radio that failed to open leaves the meter working off
                    // its button and its panel, which is strictly better than
                    // refusing to measure at all.
                    None => trigger.wait_for_falling_edge().await,
                }
                meter.measure(&mut dma).await
            }
        };

        // Report before the panel: the reader is waiting on an answer, and the
        // second the display spends lighting up is a second the radio could
        // have spent delivering it.
        if let Some(link) = &mut link {
            link.send(address.read(), &reading);
        }
        panel.show(&reading);

        // Anything that arrived while the meter was busy is dropped here, so
        // the next `wait_command` starts from silence -- the same rule the
        // button already follows.
        if let Some(link) = &mut link {
            link.discard_pending().await;
        }

        Timer::after_millis(DISPLAY_ON_MS).await;
        panel.blank();
    }
}
