#![no_std]
#![no_main]

//! "Passive" AC ammeter, circuit A: one reading per press of S2.
//!
//! The cycle is: S2 switches the external circuit into its measuring state
//! (PA8 high, PA26 low), the probe and measurement frames run, the pair goes
//! back to idle before any arithmetic starts, and the result is shown for
//! `DISPLAY_ON_MS` before the panel goes dark. Presses that land inside a
//! cycle are dropped, not queued. Between cycles nothing runs -- the executor
//! idles on the S2 edge.
//!
//! This file is the wiring and the measurement period; the substance lives in
//! the four modules below.
//!   `sampler` -- TIMG6/ADC1/DMA and the sample buffers
//!   `range`   -- OPA1 PGA, DAC bias pivot, and the autoranger
//!   `dsp`     -- windowing, true RMS
//!   `cal`     -- per-range LSB -> amps tables

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_mspm0::bind_interrupts;
use embassy_mspm0::dma::{self, Channel};
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::peripherals;
use embassy_time::{Duration, Timer};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_9X15;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use heapless::String;
use panic_halt as _;

mod cal;
mod dac;
mod dsp;
mod oled;
mod range;
mod sampler;

use cal::lsb_to_amps;
use dsp::{coarse_pivot, rms_lsb};
use oled::Oled;
use range::{
    Gain, OUT_CENTER, Range, apply_range, dac_code_for, next_range, over_range, power_down_opa1,
    probe_stats, setup_opa1_pga,
};
use sampler::{
    ADC_CH_RAW_IN, BUF, PINCM_PA16, PINCM_PA18, PROBE_BUF, capture, init_adc1_event, init_timer,
    set_adc_channel, set_hiz,
};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

/// Settling allowed after the external circuit is switched into its measuring
/// state, before the probe frame starts.
///
/// What survives into the measurement frame is a *decaying* offset, and
/// `rms_lsb` subtracts the frame's own weighted mean -- that removes a constant
/// but not a decay, so the tail of the turn-on transient lands in the reading.
/// Set from the front end's measured turn-on time constant, long enough that
/// the residual at the start of the measurement frame is below a LSB.
const AFE_SETTLE_MS: u64 = 20;

/// How long a reading stays lit before the panel is blanked.
const DISPLAY_ON_MS: u64 = 1_000;

/// How many times the SSD1306 may be brought up before this power cycle gives
/// up on it. A failed attempt is retried on a later press rather than being
/// remembered as final, because the press most likely to fail is the first one
/// -- it lands while the harvested rail is still weak. The cap is what keeps a
/// display that is simply absent from costing a bus transaction on every press
/// for the rest of the power cycle.
const OLED_INIT_ATTEMPTS: u8 = 4;

/// Blink rate of the power-on indicator, in blinks per second.
const BLINK_HZ: u64 = 3;

/// Power-on indicator on the LaunchPad's red LED1 (PA0, jumper J4).
///
/// It says the firmware is alive, which is the one thing the OLED cannot: the
/// panel is dark except for the second after a reading, and a dark panel looks
/// the same whether the board is unpowered, stuck before the first press, or
/// simply idle. Blinking rather than steady, because a steady LED cannot tell
/// a running executor from a hung one.
///
/// Runs as its own task so it keeps its rate through a measurement cycle,
/// which holds the main loop for a few hundred milliseconds at a stretch.
/// Toggling is twice per blink, hence the doubled rate.
#[embassy_executor::task]
async fn blink(mut led: Output<'static>) -> ! {
    let half_period = Duration::from_hz(2 * BLINK_HZ);
    loop {
        led.toggle();
        Timer::after(half_period).await;
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
    if let Ok(token) = blink(Output::new(p.PA0, Level::Low)) {
        spawner.spawn(token);
    }

    // The external circuit's two control lines, driven as one complementary
    // pair: measuring is PA8 high with PA26 low, idle is the reverse. They are
    // never set independently, which is why `measuring()` below takes a single
    // bool -- a state where both say "measure" or neither does is not a state
    // this circuit has, and the pair should not be able to express one.
    //
    // Both pins need a physical resistor holding their idle level: between
    // power-on and this line they are inputs, not driven outputs, so PA8 needs
    // a pull-down and PA26 a pull-up. That window is exactly when the
    // harvested supply has the least to spare, and it is also long -- it
    // covers the whole boot, not just these two statements.
    let mut afe_enable = Output::new(p.PA8, Level::Low);
    let mut afe_enable_n = Output::new(p.PA26, Level::High);
    // Leaving is the mirror of entering, so the circuit passes through the
    // same intermediate state in both directions rather than a different one
    // each way.
    let mut set_measuring = |measuring: bool| {
        if measuring {
            afe_enable.set_high();
            afe_enable_n.set_low();
        } else {
            afe_enable_n.set_high();
            afe_enable.set_low();
        }
    };

    // The idle state of the two analog pins, and the state they are left in
    // after every cycle. Nothing else in this firmware touches PINCM40/PINCM38.
    set_hiz(PINCM_PA18);
    set_hiz(PINCM_PA16);

    let mut dma = Channel::new(p.DMA_CH0, Irqs);
    // `None` leaves PB2/PB3 in their reset state until the first reading
    // exists.
    let mut display: Option<Oled> = None;
    let mut oled_attempts_left = OLED_INIT_ATTEMPTS;

    // LaunchPad S2 is the independent user button on PB21. It pulls the pin
    // low when pressed; unlike S1/PA18, it does not disturb the ADC input.
    let mut trigger = Input::new(p.PB21, Pull::Up);

    // Survive across cycles because the analog blocks do not: everything below
    // is powered down between readings, so each cycle rebuilds the front end
    // from these two numbers rather than from whatever the registers held.
    //
    // `range` is where the next `next_range` starts from, and one cycle ago is
    // a better guess than the ladder's pessimistic end. `mean_in_last` is the
    // input DC the pivot is placed against. The initial pair is the choice that
    // assumes least -- x2 cannot clip anything a higher step would have
    // handled, and centre is the placement furthest from both rails -- and
    // neither survives the first cycle: the probe reads the input at unity
    // gain, and `next_range`/`dac_code_for` derive both from what it actually
    // measured. Convergence takes one cycle rather than several, because the
    // probe's numbers are input-referred and so do not depend on which range
    // happens to be selected while it runs.
    let mut range = Range::Pga(Gain::X2);
    let mut mean_in_last = OUT_CENTER;

    loop {
        // Idle here, and this is also where a press made during the previous
        // cycle gets dropped: `wait_for_falling_edge` clears pending edge
        // events before it arms, so only edges from here on count.
        trigger.wait_for_falling_edge().await;

        set_measuring(true);

        // The analog blocks come up inside the front end's own settling window,
        // so bringing them back costs no latency of its own: the DAC's turn-on
        // (datasheet 7.17.5, 6.9 us max) and OPA1's enable time (7.19.2, 6 us
        // max at GBW=HIGHGAIN) are microseconds against AFE_SETTLE_MS here.
        // Each is a full bring-up, not a resume: the end of every cycle clears
        // PWREN on all four blocks, and that resets every register in them.
        //
        // The gain and the pivot are last cycle's, so a cycle whose probe fails
        // still measures at a setting that was right rather than at a blind
        // mid-scale one. The probe replaces both a few milliseconds from now.
        let gain = match range {
            Range::Pga(g) => g,
            Range::Direct => Gain::X2,
        };
        setup_opa1_pga(gain, dac_code_for(gain, mean_in_last));
        init_adc1_event();
        init_timer();

        Timer::after_millis(AFE_SETTLE_MS).await;

        // Both reset every press: one press is one independent reading, so a
        // mark can only ever describe the frames this cycle actually took.
        let mut input_bad = false;
        let mut range_over = false;

        // --- Probe: the raw input at unity gain, straight off PA18 ---
        //
        // The OPA is not involved and does not move, so this costs one MEMCTL
        // write and 40 ms, with no settle. Both numbers it yields are
        // input-referred and therefore independent of the gain currently
        // selected, which is what lets the range converge in a single cycle
        // rather than hunting toward it.
        set_adc_channel(ADC_CH_RAW_IN);
        // SAFETY: `capture` waits the transfer out (or pauses it) before
        // returning, so nothing else touches PROBE_BUF concurrently.
        let probed = capture(&mut dma, unsafe {
            &mut *core::ptr::addr_of_mut!(PROBE_BUF)
        })
        .await;
        if probed {
            // SAFETY: as above -- the transfer is finished or paused.
            let (mean_in, pp_in, railed) = probe_stats(unsafe { &*core::ptr::addr_of!(PROBE_BUF) });
            input_bad = railed;
            if !railed {
                mean_in_last = mean_in;
                range = next_range(range, mean_in, pp_in);
                range_over = over_range(range, mean_in, pp_in);
                if apply_range(range, mean_in) {
                    // Settle the DAC and the re-tapped ladder before the frame
                    // that carries the marks. Both are microsecond-scale; 1 ms
                    // is slack.
                    // Skipped on Direct, which changes nothing analog.
                    Timer::after_millis(1).await;
                }
            }
        }
        // A failed or railed probe leaves the range exactly as it was.
        // Re-ranging on numbers known to be wrong is strictly worse than
        // keeping a setting that was right one cycle ago.

        // --- Measurement frame: whatever the chosen range put the signal on
        // (OPA1_OUT on PA16 for the PGA ranges, PA18 itself for Direct, in
        // which case the channel is already the one the probe just used) ---
        // SAFETY: `capture` waits the transfer out (or pauses it) before
        // returning, so nothing else touches BUF concurrently.
        let framed = capture(&mut dma, unsafe { &mut *core::ptr::addr_of_mut!(BUF) }).await;

        // Everything below this line works out of BUF, so the whole signal
        // chain comes down here rather than after the display: the front end
        // has no reader left, and the reader has nothing left to read.
        //
        // Order is downstream first. The converter and its sample clock stop,
        // then the amplifier and its bias reference, then the external circuit
        // -- so nothing is ever driving into a block that has just lost power.
        // What this leaves on PA18 is the pad alone: the IOMUX has held it
        // high-Z since boot, and with OPA1 and ADC1 out of the power domain
        // there is no longer an analog mux on the other side of it either.
        sampler::power_down();
        power_down_opa1();
        set_measuring(false);

        // BUF holds 800 hardware-timed samples at 4 kHz (TIM+ADC+DMA, zero CPU
        // involvement per sample).
        // SAFETY: as above -- the transfer is finished or paused, so BUF is
        // ours for the rest of this iteration.
        let buf = unsafe { &*core::ptr::addr_of!(BUF) };
        let pivot = coarse_pivot(buf);
        let rms = rms_lsb(buf, pivot);
        let amps = lsb_to_amps(rms, range);

        // PB2/PB3 and the SSD1306 stay untouched until there is a reading to
        // put on them. A failed bring-up leaves `display` at `None` and is
        // tried again on a later press: the attempt that failed dropped its
        // half-built driver, and with it the pins it had stolen, so the next
        // attempt starts from the same state as the first one did.
        if display.is_none() && oled_attempts_left > 0 {
            oled_attempts_left -= 1;
            // SAFETY: PB2/PB3 are used by nothing else in this firmware, and
            // no live `Oled` exists here -- `display` is `None`.
            display = unsafe { Oled::new() }.ok();
        }
        if let Some(display) = &mut display {
            let _ = display.clear(BinaryColor::Off);
            let style = MonoTextStyle::new(&FONT_9X15, BinaryColor::On);

            // A leading marker means the reading below it is not to be
            // believed, and says which of the three ways it went wrong:
            //   '!' the probe found the input pinned at an ADC end, so the
            //       signal is leaving `0..VDDA` and no internal gain or bias
            //       choice can recover it -- that needs external conditioning.
            //   '>' the arithmetic says the signal will not fit even on
            //       Direct, the least demanding range there is, so the frame
            //       is clipping. Every range is bounded by the supply and
            //       nothing tighter (see range::OUT_LO -- OPA1's rails and
            //       ADC1's full scale are the same two voltages), which makes
            //       this exactly the same physical condition as '!', predicted
            //       from the probe's mean/pp rather than seen as pinned
            //       samples. Adding Direct to the ladder is what made it
            //       near-unreachable; it is kept as the arithmetic backstop.
            //   '?' one of this cycle's DMA transfers never drained, so its
            //       buffer is part new samples and part stale ones.
            // All three sit on the primary line on purpose: a number this
            // display cannot stand behind should not look like the others.
            // They matter most while calibrating -- a marked frame's `rms`
            // below is fiction, and entering it into a CAL table freezes the
            // fiction into the instrument.
            let marker = if input_bad {
                "!"
            } else if range_over {
                ">"
            } else if !probed || !framed {
                "?"
            } else {
                ""
            };
            let mut line: String<32> = String::new();
            let _ = write!(line, "{}{:.3} A", marker, amps);
            let _ = Text::new(&line, Point::new(4, 26), style).draw(display);

            // Raw single-frame RMS in LSB and the gain it was taken at --
            // together they are one row of one CAL table, and the gain says
            // *which* table, so the calibration cannot be entered against the
            // wrong one. Two decimals: 0.01 LSB is 0.03% at 0.1 A, so the
            // display resolution is not what limits the calibration.
            //
            // Starts at x=0, not x=4 like the line above: 14 characters of
            // FONT_9X15 is exactly 126 px, so the margin is the difference
            // between fitting and losing the last digit of the gain.
            let mut line2: String<32> = String::new();
            let _ = write!(line2, "rms {:.2} x{}", rms, range.nominal());
            let _ = Text::new(&line2, Point::new(0, 52), style).draw(display);

            let _ = display.flush();
            let _ = display.set_display_on(true);
        }

        Timer::after_millis(DISPLAY_ON_MS).await;

        if let Some(display) = &mut display {
            let _ = display.set_display_on(false);
        }
    }
}
