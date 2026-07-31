//! One reading, start to finish: switch the external circuit in, bring the
//! analog blocks up, probe, range, take the frame, put everything back to
//! sleep, and turn the samples into amps.
//!
//! Everything true of a reading regardless of what asked for it lives here, so
//! the binaries differ only in *when* they measure and what they do with the
//! answer -- `main.rs` on a button press onto the panel, `bin/stream.rs` on a
//! timer out the radio. Neither can get the sequence subtly different from the
//! other, which matters because the order in `measure` is not arbitrary: the
//! calibration tables are only valid for readings taken exactly this way.

use embassy_mspm0::dma::Channel;
use embassy_mspm0::gpio::{Level, Output};
use embassy_mspm0::{Peri, peripherals};
use embassy_time::Timer;

use crate::cal::lsb_to_amps;
use crate::dsp::{coarse_pivot, rms_lsb};
use crate::range::{
    Gain, OUT_CENTER, Range, apply_range, dac_code_for, next_range, over_range, power_down_opa1,
    probe_stats, setup_opa1_pga,
};
use crate::sampler::{
    ADC_CH_RAW_IN, BUF, PINCM_PA16, PINCM_PA18, PROBE_BUF, capture, init_adc1_event, init_timer,
    set_adc_channel, set_hiz,
};

/// Settling allowed after the external circuit is switched into its measuring
/// state, before the probe frame starts.
///
/// What survives into the measurement frame is a *decaying* offset, and
/// `rms_lsb` subtracts the frame's own weighted mean -- that removes a constant
/// but not a decay, so the tail of the turn-on transient lands in the reading.
/// Set from the front end's measured turn-on time constant, long enough that
/// the residual at the start of the measurement frame is below a LSB.
const AFE_SETTLE_MS: u64 = 20;

/// How much a reading is worth. Ordered by which fault to report when more
/// than one is true, worst first -- that ordering is the reason this is one
/// enum rather than three flags each consumer re-prioritises for itself.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// The 2.5 V reference never reported ready, so every code in the frame is
    /// against an unsettled reference. Worst of the four because it is the only
    /// one where the samples look entirely ordinary: they are a plausible
    /// waveform, scaled by an unknown amount.
    RefBad,
    /// The probe found the input pinned at an ADC end, so the signal is
    /// leaving `0..VDDA` and no internal gain or bias choice can recover it --
    /// that needs external conditioning.
    InputBad,
    /// The arithmetic says the signal will not fit even on Direct, the least
    /// demanding range there is, so the frame is clipping. Every range is
    /// bounded by the supply and nothing tighter (see `range::OUT_LO` --
    /// OPA1's rails and ADC1's full scale are the same two voltages), which
    /// makes this exactly the same physical condition as `InputBad`, predicted
    /// from the probe's mean/pp rather than seen as pinned samples.
    OverRange,
    /// One of this cycle's DMA transfers never drained, so its buffer is part
    /// new samples and part stale ones.
    Incomplete,
    Good,
}

/// One measurement. `rms` and `range` together are one row of one CAL table,
/// which is why they travel with the answer rather than being consumed by it:
/// a reading that cannot say which table produced it cannot be calibrated
/// against, and one taken while `quality` is not `Good` must not be entered
/// into a table at all -- freezing that fiction into the instrument is worse
/// than having no row.
pub struct Reading {
    pub amps: f32,
    pub rms: f32,
    pub range: Range,
    /// Probe mean and peak-to-peak, in input-referred ADC LSB. `None` when the
    /// probe frame never drained, which is also when the range and the pivot
    /// are the previous cycle's rather than this one's.
    #[allow(dead_code)] // Read by bin/stream.rs, not by the panel.
    pub probe: Option<(i32, u32)>,
    ref_bad: bool,
    input_bad: bool,
    range_over: bool,
    incomplete: bool,
}

impl Reading {
    pub fn quality(&self) -> Quality {
        if self.ref_bad {
            Quality::RefBad
        } else if self.input_bad {
            Quality::InputBad
        } else if self.range_over {
            Quality::OverRange
        } else if self.incomplete {
            Quality::Incomplete
        } else {
            Quality::Good
        }
    }
}

pub struct Meter {
    afe_enable: Output<'static>,
    afe_enable_n: Output<'static>,
    /// Survive across cycles because the analog blocks do not: everything is
    /// powered down between readings, so each cycle rebuilds the front end
    /// from these two numbers rather than from whatever the registers held.
    ///
    /// `range` is where the next `next_range` starts from, and one cycle ago is
    /// a better guess than the ladder's pessimistic end. `mean_in_last` is the
    /// input DC the pivot is placed against.
    range: Range,
    mean_in_last: i32,
}

impl Meter {
    /// Claims the external circuit's control pair and puts the two analog pins
    /// in their idle state. Nothing else in this firmware touches PINCM40 or
    /// PINCM38.
    ///
    /// The initial range and pivot are the choice that assumes least -- x2
    /// cannot clip anything a higher step would have handled, and centre is the
    /// placement furthest from both rails. Neither survives the first cycle:
    /// the probe reads the input at unity gain and `next_range`/`dac_code_for`
    /// derive both from what it actually measured. Convergence takes one cycle
    /// rather than several, because the probe's numbers are input-referred and
    /// so do not depend on which range happens to be selected while it runs.
    pub fn new(
        pa8: Peri<'static, peripherals::PA8>,
        pa26: Peri<'static, peripherals::PA26>,
    ) -> Self {
        // The external circuit's two control lines, driven as one complementary
        // pair: measuring is PA8 high with PA26 low, idle is the reverse. They
        // are never set independently, which is why `set_measuring` takes a
        // single bool -- a state where both say "measure" or neither does is
        // not a state this circuit has, and the pair should not be able to
        // express one.
        //
        // Both pins need a physical resistor holding their idle level: between
        // power-on and this line they are inputs, not driven outputs, so PA8
        // needs a pull-down and PA26 a pull-up. That window is exactly when the
        // harvested supply has the least to spare, and it is also long -- it
        // covers the whole boot, not just these two statements.
        let meter = Self {
            afe_enable: Output::new(pa8, Level::Low),
            afe_enable_n: Output::new(pa26, Level::High),
            range: Range::Pga(Gain::X2),
            mean_in_last: OUT_CENTER,
        };
        set_hiz(PINCM_PA18);
        set_hiz(PINCM_PA16);
        meter
    }

    /// Leaving is the mirror of entering, so the circuit passes through the
    /// same intermediate state in both directions rather than a different one
    /// each way.
    fn set_measuring(&mut self, measuring: bool) {
        if measuring {
            self.afe_enable.set_high();
            self.afe_enable_n.set_low();
        } else {
            self.afe_enable_n.set_high();
            self.afe_enable.set_low();
        }
    }

    /// One complete cycle. Returns with every analog block powered down and the
    /// external circuit back in its idle state, whatever happened in between.
    pub async fn measure(&mut self, dma: &mut Channel<'_>) -> Reading {
        self.set_measuring(true);

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
        //
        // The reference goes first and everything downstream is measured
        // against it: the DAC's bias, the ADC's full scale, and therefore both
        // the window the autoranger fits into and the codes the RMS is formed
        // from. Its 200 us settle (datasheet 7.15.2) hides inside AFE_SETTLE_MS
        // like the rest.
        let ref_bad = !crate::vref::init(crate::vref::Output::V2_5);
        let gain = match self.range {
            Range::Pga(g) => g,
            Range::Direct => Gain::X2,
        };
        setup_opa1_pga(gain, dac_code_for(gain, self.mean_in_last));
        init_adc1_event();
        init_timer();

        Timer::after_millis(AFE_SETTLE_MS).await;

        // Both reset every cycle: one cycle is one independent reading, so a
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
        let probed = capture(dma, unsafe { &mut *core::ptr::addr_of_mut!(PROBE_BUF) }).await;
        let mut probe = None;
        if probed {
            // SAFETY: as above -- the transfer is finished or paused.
            let (mean_in, pp_in, railed) = probe_stats(unsafe { &*core::ptr::addr_of!(PROBE_BUF) });
            probe = Some((mean_in, pp_in));
            input_bad = railed;
            if !railed {
                self.mean_in_last = mean_in;
                self.range = next_range(self.range, mean_in, pp_in);
                range_over = over_range(self.range, mean_in, pp_in);
                if apply_range(self.range, mean_in) {
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
        let framed = capture(dma, unsafe { &mut *core::ptr::addr_of_mut!(BUF) }).await;

        // Everything below this line works out of BUF, so the whole signal
        // chain comes down here rather than after the caller is done with the
        // answer: the front end has no reader left, and the reader has nothing
        // left to read.
        //
        // Order is downstream first. The converter and its sample clock stop,
        // then the amplifier and its bias reference, then the external circuit
        // -- so nothing is ever driving into a block that has just lost power,
        // and the 2.5 V reference outlives both of the blocks that use it.
        // What this leaves on PA18 is the pad alone: the IOMUX has held it
        // high-Z since boot, and with OPA1 and ADC1 out of the power domain
        // there is no longer an analog mux on the other side of it either.
        crate::sampler::power_down();
        power_down_opa1();
        self.set_measuring(false);

        // BUF holds 800 hardware-timed samples at 4 kHz (TIM+ADC+DMA, zero CPU
        // involvement per sample).
        // SAFETY: as above -- the transfer is finished or paused, so BUF is
        // ours until the next `measure`.
        let buf = unsafe { &*core::ptr::addr_of!(BUF) };
        let pivot = coarse_pivot(buf);
        let rms = rms_lsb(buf, pivot);

        Reading {
            amps: lsb_to_amps(rms, self.range),
            rms,
            range: self.range,
            probe,
            ref_bad,
            input_bad,
            range_over,
            incomplete: !probed || !framed,
        }
    }
}
