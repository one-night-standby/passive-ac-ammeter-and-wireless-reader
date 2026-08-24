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
//! Power-on waits for the supply rail before any of that. Nothing analog is
//! enabled, the radio is not powered and the indicator is not lit until ADC0's
//! internal monitor puts VDD at `BOOT_MIN_MV`, so a meter coming up on a
//! harvested rail starts once, at a voltage every block downstream is rated
//! for, rather than half-starting on the way there. Nothing runs during the
//! wait: the rail charges against no load this firmware switched on.
//!
//! Past that, power-on takes one reading unasked, before the radio is up, and
//! reports it the moment the port opens. So the meter always has a number to
//! give, and a reader that connects before anyone has pressed anything is not
//! told the meter is silent when it is merely idle.
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
//!   `nvcal`   -- the field table those fall back from, in flash
//!   `supply`  -- the rail itself, off ADC0's internal monitor
//!   `ui`      -- the OLED readout
//!   `led`     -- the power-on indicator

use embassy_executor::Spawner;
use embassy_futures::select::select;
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
mod nvcal;
mod oled;
mod range;
mod sampler;
mod supply;
mod ui;
mod vref;

use link::{Address, Link};
use meter::Meter;
use ui::{DISPLAY_ON_MS, Panel};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
    UART2 => BufferedInterruptHandler<peripherals::UART2>;
});

/// Radio buffers. TX only has to hold the two frames of one reading. RX has to
/// hold more than one command line: a calibration push sends 40-byte `CALPT`
/// lines and re-sends one whose answer went missing, so two can land together,
/// and a ring that overflows turns the second into a line the parser has to
/// throw away. Both are `static` because `BufferedUart` keeps them for as long
/// as the port is open, which here is forever.
static mut TX_BUF: [u8; 192] = [0; 192];

static mut RX_BUF: [u8; 128] = [0; 128];

/// The rail this meter starts up on, in millivolts. Nothing analog and nothing
/// on the radio runs until `supply` measures at least this.
///
/// It is a threshold on the *measured* supply, and the measurement carries the
/// divider's 1.5% and the reference's 1.6% (datasheet 7.12.1, 7.15.1), so a rail
/// regulated to a hair under 3.3 V can read a hair under 3.3 V. With no deadline
/// on the wait, that is a meter that never starts: this constant is the one to
/// lower.
const BOOT_MIN_MV: u16 = 3_300;

/// Idle until something asks for a reading, announcing this meter's address
/// every `HEARTBEAT_MS` in the meantime.
///
/// The beat is not a trigger -- it goes out and the wait resumes. Only the
/// button and a `MEAS` command return from here, and which one did is not worth
/// distinguishing: they are the same request.
async fn wait_for_request(trigger: &mut Input<'static>, link: &mut Link, address: &Address) {
    // The heartbeat is not raced against the command here, it lives inside
    // `wait_command` -- see there for why cancelling a half-sent reply is a
    // thing worth designing against. What is left to race is the button, and a
    // press mid-reply is a person, not a timer.
    select(trigger.wait_for_falling_edge(), link.wait_command(address)).await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    let mut meter = Meter::new(p.PA8, p.PA26);
    let mut dma = Channel::new(p.DMA_CH0, Irqs);
    let mut panel = Panel::new();
    let address = Address::new(p.PB0, p.PB6, p.PB7, p.PB8);

    // LaunchPad S2 is the independent user button on PB21. It pulls the pin
    // low when pressed; unlike S1/PA18, it does not disturb the ADC input.
    let mut trigger = Input::new(p.PB21, Pull::Up);

    // Nothing above this line has drawn any current worth the name: the pins
    // are configured, nothing is lit, and no analog block has been enabled.
    // Nothing below it runs until the rail is up.
    //
    // This is the whole of the supply gate, and it is here rather than in front
    // of the display because everything downstream of it wants a rail that has
    // arrived, not just the SSD1306: the reference the reading is scaled
    // against needs VDD >= 2.7 V, the radio module wants its 3.3 V, and the
    // panel's charge pump wants the same. One wait covers all three.
    //
    // Waited on rather than watched: the rail is re-measured every POLL_MS with
    // VREF and ADC0 powered down in between. An always-armed watch is available
    // -- ADC0's window comparator raising HIGHIFG onto the event fabric -- and
    // costs more, because nothing but the CPU can cycle VREF, so arming it pins
    // the reference on at 189 uA typ (330 uA max) against the 19 uA a 10% duty
    // comes to. Nearly all of each look is the reference settling, which the
    // CPU sleeps through; an armed watch would be paying for that settle
    // continuously and would still be reading the same converter.
    //
    // There is no deadline. A rail that never reaches BOOT_MIN_MV parks the
    // meter here, dark, which is the honest state to be in: no reading it
    // could take would be in spec and nothing it drove would be either.
    supply::wait_above(BOOT_MIN_MV).await;

    // The indicator starts on this side of the gate because it is a load, and
    // the one stretch of a power cycle where a few milliamps through LED1 are
    // worth anything is the stretch just waited out: cold-start harvest is
    // milliwatts, and all of it belongs in the storage cap. So dark means the
    // rail has not arrived, blinking means it has and the executor is running,
    // and the indicator is never what kept the meter from starting.
    //
    // The task pool is one deep and this is its only spawn, so the token
    // cannot come back `Err`. Handled rather than unwrapped anyway: an
    // indicator that failed to start is a reason to run without it, not a
    // reason to halt a meter that is otherwise fine.
    if let Ok(token) = led::blink(Output::new(p.PA0, Level::Low)) {
        spawner.spawn(token);
    }

    // One reading on power-up, before anything can ask for it, and before the
    // radio exists. Measuring first is what the board notes ask for on a
    // harvested rail (6.1): the module's start-up current is the largest single
    // draw in this firmware, and it should not land while the supply has just
    // come up. Nothing is lost by waiting -- the reading is already in hand
    // when the port opens, so the first frame goes out as soon as there is
    // anything to send it over.
    // Before the first reading, because the first reading is reported like
    // every other one and there is no version of this that is allowed to go out
    // on the built-in table while a field table sits in flash.
    cal::load_field();

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
            link.send(address.read(), &reading).await;
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
