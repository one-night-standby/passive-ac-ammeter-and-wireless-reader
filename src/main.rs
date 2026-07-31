#![no_std]
#![no_main]

//! "Passive" AC ammeter, circuit A: one reading per press of S2.
//!
//! The cycle is: S2 raises PA8 to power the external sampling circuit, the
//! probe and measurement frames run, PA8 drops again before any arithmetic
//! starts, and the result is shown for `DISPLAY_ON_MS` before the panel goes
//! dark. Presses that land inside a cycle are dropped, not queued. Between
//! cycles nothing runs -- the executor idles on the S2 edge.
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
use embassy_time::Timer;
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
    DAC_MAX, Gain, Range, apply_range, next_range, over_range, probe_stats, setup_opa1_pga,
};
use sampler::{
    ADC_CH_RAW_IN, BUF, PINCM_PA16, PINCM_PA18, PROBE_BUF, capture, init_adc1_event, init_timer,
    set_adc_channel, set_analog,
};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

/// Settling allowed after PA8 powers the external sampling circuit, before the
/// probe frame starts.
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

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    // Active-high enable for the external sampling circuit. This pin needs a
    // physical pull-down: between power-on and this line it is an input, not a
    // driven low, and that window is exactly when the harvested supply has the
    // least to spare.
    let mut afe_enable = Output::new(p.PA8, Level::Low);

    set_analog(PINCM_PA18);
    set_analog(PINCM_PA16);
    // Starting range only, and deliberately the pessimistic end of the
    // ladder: x2 cannot clip anything a higher step would have handled. The
    // starting DAC code is mid-scale for the same reason -- not because the
    // input is expected there, but because it is the choice that assumes
    // least. Neither survives the first cycle: the probe reads the input at
    // unity gain, and `next_range`/`dac_code_for` derive both from what it
    // actually measured. Convergence takes one cycle rather than several,
    // because the probe's numbers are input-referred and so do not depend on
    // which range happens to be selected while it runs.
    setup_opa1_pga(Gain::X2, (DAC_MAX / 2) as u16);
    init_adc1_event();
    init_timer();

    let mut dma = Channel::new(p.DMA_CH0, Irqs);
    // `None` leaves PB2/PB3 in their reset state until the first reading
    // exists.
    let mut display: Option<Oled> = None;
    let mut oled_attempts_left = OLED_INIT_ATTEMPTS;

    // LaunchPad S2 is the independent user button on PB21. It pulls the pin
    // low when pressed; unlike S1/PA18, it does not disturb the ADC input.
    let mut trigger = Input::new(p.PB21, Pull::Up);

    // Survives across cycles: it is where the next `next_range` starts from,
    // and one cycle ago is a better guess than the ladder's pessimistic end.
    let mut range = Range::Pga(Gain::X2);

    loop {
        // Idle here, and this is also where a press made during the previous
        // cycle gets dropped: `wait_for_falling_edge` clears pending edge
        // events before it arms, so only edges from here on count.
        trigger.wait_for_falling_edge().await;

        afe_enable.set_high();
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

        // Everything below this line works out of BUF. The front end has no
        // reader left, so it comes down before the arithmetic rather than
        // after the display.
        afe_enable.set_low();

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
