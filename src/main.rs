#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_mspm0::bind_interrupts;
use embassy_mspm0::dma::{self, Channel, TransferOptions};
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::pac;
use embassy_mspm0::peripherals;
use embassy_mspm0::uart::{Config as UartConfig, Uart};
use embassy_time::Timer;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_9X15;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use heapless::String;
use libm::{atan2f, cosf, sinf, sqrtf};
use panic_halt as _;

mod dac;
mod oled;
use oled::Oled;

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

/// Event fabric channel TIMG6 publishes its zero event on and ADC1
/// subscribes to. Free pick -- only one producer/consumer pair exists in
/// this firmware so far. (This is a *different* namespace from the DMA
/// trigger ID below: this one is the generic 16-channel event crossbar,
/// the other is ADC1's own hardwired "result loaded" line into the DMA.)
const EVT_CH_ADC1: u8 = 1;
/// ADC1's dedicated hardwired DMA request line (G350x datasheet:
/// DMA_ADC1_EVT_GEN_BD_TRIG). Confirmed against
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/sampler.rs, which
/// runs this exact TIM+ADC+DMA chain on the same silicon.
const DMA_TRIG_ADC1: u8 = 24;

const TIMER_CLK_HZ: u32 = 32_000_000; // BUSCLK
/// STRUCTURE.md: 4k samples/sec, 800 points (10 cycles of 50 Hz mains).
const SAMPLE_HZ: u32 = 4_000;
const LOAD: u32 = TIMER_CLK_HZ / SAMPLE_HZ - 1;

const PINCM_PA18: usize = 40; // OPA1_IN1+ (signal in)
const PINCM_PA16: usize = 38; // OPA1_OUT (also ADC1 channel 1, signal out)

const HC42_BAUD_RATE: u32 = 9_600;

fn set_analog(pincm: usize) {
    pac::IOMUX.pincm(pincm).modify(|w| {
        w.set_pf(0);
        w.set_pipu(false);
        w.set_pipd(false);
    });
}

const N: usize = 800;
static mut BUF: [u32; N] = [0; N];

/// ADC1 result width. MEMRES is 12-bit right-aligned; masking is defensive,
/// but it has to be applied *consistently* -- the previous `rms_lsb` summed
/// unmasked words for its mean and then subtracted that from masked ones.
const ADC_MASK: u32 = 0xFFF;

const TWO_PI: f32 = 2.0 * core::f32::consts::PI;

/// Nominal mains frequency. Only the *correlator* in `estimate_hz` assumes
/// this value; the RMS path (`rms_lsb`) does not depend on the mains
/// frequency at all -- see the Hann note there.
const MAINS_NOM_HZ: u32 = 50;
/// Samples per nominal mains cycle -- 80. Must divide evenly, so the
/// quadrature tables can be indexed by `n % SPP_NOM` instead of carrying a
/// running phase (no drift, no per-sample sin/cos on a Cortex-M0+ that has
/// no FPU).
const SPP_NOM: usize = (SAMPLE_HZ / MAINS_NOM_HZ) as usize;
/// Half-record length for the two-block phase-difference frequency
/// estimate.
const HALF: usize = N / 2;

const _: () = assert!(SAMPLE_HZ.is_multiple_of(MAINS_NOM_HZ), "SPP_NOM must be exact");
const _: () = assert!(N.is_multiple_of(2));
// The whole phase-difference trick rests on the *nominal* fundamental
// advancing an exact multiple of 2*pi across HALF samples (400 * 2pi/80 =
// 10pi). If HALF stopped being a whole number of nominal cycles, the wrapped
// phase difference would carry a constant bias instead of reading out pure
// frequency deviation.
const _: () = assert!(HALF.is_multiple_of(SPP_NOM));

/// Hann weights over the whole record, and one nominal-frequency cycle of
/// quadrature. Built once at startup rather than stored as `const` tables:
/// `cosf` is not const-callable, and 3.9 KB of the 32 KB SRAM is cheaper
/// than hand-rolling a const-evaluable cosine.
static mut HANN: [f32; N] = [0.0; N];
static mut COS_TAB: [f32; SPP_NOM] = [0.0; SPP_NOM];
static mut SIN_TAB: [f32; SPP_NOM] = [0.0; SPP_NOM];

/// Read-only views of the tables above, so the `static mut` raw-pointer
/// `unsafe` happens exactly once (at the top of `main`) instead of inside
/// every DSP loop.
struct Tables {
    hann: &'static [f32; N],
    cos: &'static [f32; SPP_NOM],
    sin: &'static [f32; SPP_NOM],
}

/// Periodic (DFT-even) Hann, `w[n] = 0.5*(1 - cos(2*pi*n/N))`. The periodic
/// form is the one that makes `sum(w)` come out to exactly N/2 and puts the
/// window's nulls on the analysis bins, which is the whole point here.
///
/// Mirrored about n = N/2 so only N/2+1 `cosf` calls run: on a soft-float
/// M0+ each call is ~1.5k cycles, so this is ~19 ms of startup instead of
/// ~38 ms. Startup time is a scored item; the 500 ms HC-42 settle below
/// still dominates, but there is no reason to pay double here.
fn init_dsp_tables() {
    unsafe {
        let hann = &mut *core::ptr::addr_of_mut!(HANN);
        for n in 0..=N / 2 {
            let w = 0.5 * (1.0 - cosf(TWO_PI * n as f32 / N as f32));
            hann[n] = w;
            if n > 0 && n < N / 2 {
                hann[N - n] = w;
            }
        }

        let cos_tab = &mut *core::ptr::addr_of_mut!(COS_TAB);
        let sin_tab = &mut *core::ptr::addr_of_mut!(SIN_TAB);
        for n in 0..SPP_NOM {
            let a = TWO_PI * n as f32 / SPP_NOM as f32;
            cos_tab[n] = cosf(a);
            sin_tab[n] = sinf(a);
        }
    }
}

/// Non-inverting PGA gain steps. The on-chip resistor ladder only has these
/// five fixed taps -- no continuous adjustment, and no step below 2x (unity
/// is a separate topology, plain buffer mode, not part of this ladder).
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum Gain {
    X2 = 1,
    X4 = 2,
    X8 = 3,
    X16 = 4,
    X32 = 5,
}

/// DAC12 output code for a 1.7 V bias reference (VDDA = 3.3 V assumed):
/// 1.7 / 3.3 * 4096 ~= 2110.
const DAC_CODE_1V7: u16 = 2110;

/// OPA1 as a non-inverting PGA (TRM 21.2.7.3.2): PSEL=EXTPIN1 (PA18, through
/// the P-MUX into the amplifier's true input -- datasheet SS7.19.1 puts this
/// mux's series resistance at 2.6 kOhm TYP, but the real op-amp input beyond
/// it draws only leakage-level bias current, so that resistance drops
/// negligible voltage and doesn't meaningfully load an external source),
/// NSEL=RTAP (the ladder tap feeds the inverting input), MSEL=DAC12OUT
/// (ladder bottom biased to
/// 1.7 V instead of ground, so the gain acts on (Vin - 1.7 V) rather than
/// Vin itself -- same pattern as
/// ~/embed/Single-Phase-Power-Analyzer's OPA1 setup, just a different bias
/// point and gain step). GAIN=the selected step. OUTPIN=1 drives PA16.
fn setup_opa1_pga(gain: Gain) {
    // The bias reference must be up and stable before the OPA turns on.
    dac::init();
    dac::set(DAC_CODE_1V7);

    let opa = pac::OPA1;

    opa.gprcm().rstctl().write(|w| {
        w.set_key(pac::opa::vals::ResetKey::KEY);
        w.set_resetassert(true);
        w.set_resetstkyclr(true);
    });
    opa.gprcm().pwren().write(|w| {
        w.set_key(pac::opa::vals::PwrenKey::KEY);
        w.set_enable(true);
    });
    cortex_m::asm::delay(16);

    opa.cfgbase().write(|w| {
        w.set_gbw(pac::opa::vals::Gbw::HIGHGAIN);
        w.set_rri(true);
    });

    opa.cfg().write(|w| {
        w.set_psel(pac::opa::vals::Psel::EXTPIN1);
        w.set_nsel(pac::opa::vals::Nsel::OANRTAP);
        w.set_msel(pac::opa::vals::Msel::DAC12OUT);
        w.set_gain(gain as u8);
        w.set_outpin(true);
    });

    opa.ctl().write(|w| w.set_enable(true));
    // Bounded wait -- never block the CPU forever on external hardware
    // state. If RDY never asserts (bad register sequencing, an invalid
    // config, or a real hardware limit on this gain/topology combo), give
    // up and move on rather than hanging the whole MCU (which is exactly
    // what made the last build unflashable without forcing BSL).
    for _ in 0..100_000u32 {
        if opa.stat().read().rdy() {
            break;
        }
    }
}

/// ADC1 channel 1 (PA16 = OPA1_OUT), event-triggered single conversions:
/// each TIMG6 zero-event (via the shared EVT_CH_ADC1 fabric channel) starts
/// one conversion into MEMRES0, which the DMA then drains. Register-for-
/// register match of
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/sampler.rs's
/// `init_adc` (confirmed working on this same silicon), minus the second
/// ADC and the hardware averaging it doesn't need here.
fn init_adc1_event() {
    use pac::adc::vals;
    let regs = pac::ADC1;

    regs.gprcm(0).rstctl().write(|w| {
        w.set_resetstkyclr(true);
        w.set_resetassert(true);
        w.set_key(vals::ResetKey::KEY);
    });
    regs.gprcm(0).pwren().write(|w| {
        w.set_enable(true);
        w.set_key(vals::PwrenKey::KEY);
    });
    cortex_m::asm::delay(16);

    regs.gprcm(0).clkcfg().write(|w| {
        w.set_key(vals::ClkcfgKey::KEY);
        w.set_sampclk(vals::Sampclk::SYSOSC);
    });
    regs.ctl0()
        .modify(|w| w.set_sclkdiv(vals::Sclkdiv::DIV_BY_4));
    regs.clkfreq()
        .modify(|w| w.set_frange(vals::Frange::RANGE24TO32));

    // REPEATSINGLE keeps ENC set across conversions (plain SINGLE hardware-
    // clears ENC after the first result); TRIGGER_NEXT below makes each
    // repeat wait for its own event instead of free-running.
    regs.ctl1().modify(|w| {
        w.set_conseq(vals::Conseq::REPEATSINGLE);
        w.set_sampmode(vals::Sampmode::AUTO);
        w.set_trigsrc(vals::Trigsrc::EVENT);
    });
    regs.ctl2().modify(|w| {
        w.set_startadd(0);
        w.set_endadd(0);
        w.set_res(vals::Res::BIT_12);
        w.set_df(false);
        // One converted sample per DMA trigger (0 = no trigger at all).
        w.set_sampcnt(vals::Sampcnt::from_bits(1));
        // Master DMA-request enable for this ADC (TI's DL_ADC12_enableDMA).
        // Separate from the DMA_TRIG.IMASK bit below -- that one only
        // selects *which* MEM-result-loaded event feeds the trigger; this
        // one is the switch that lets the ADC assert its DMA request line
        // at all. Missing this was exactly why conversions completed fine
        // (MEMRES0 held a real value) but the DMA channel never saw a
        // single trigger (SZ never moved off 800).
        w.set_dmaen(true);
    });
    regs.scomp0().modify(|w| w.set_val(25));

    regs.memctl(0).modify(|w| {
        w.set_chansel(1); // PA16
        w.set_vrsel(vals::Vrsel::from_bits(0)); // VDDA/VSSA
        w.set_stime(vals::Stime::SEL_SCOMP0);
        w.set_avgen(false);
        w.set_bcsen(false);
        w.set_trig(vals::Trig::TRIGGER_NEXT);
        w.set_wincomp(false);
    });

    // Subscribe the trigger to the timer's event channel.
    regs.fsub().write(|w| w.set_chanid(EVT_CH_ADC1));
    // MEMRES0 loaded -> DMA trigger.
    regs.dma_trig(0)
        .imask()
        .modify(|w| w.set_memresifg(0, true));

    // Enable conversions (they only run when events arrive).
    regs.ctl0().modify(|w| w.set_enc(true));
}

/// TIMG6: 32 MHz down-counter publishing its zero event on EVT_CH_ADC1
/// every 1/4000 s. Same register sequence as
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/sampler.rs's
/// `init_timer` (confirmed working on this same silicon), just one
/// publisher channel instead of two.
fn init_timer() {
    use pac::tim::vals;
    let tim = pac::TIMG6;

    tim.gprcm(0).rstctl().write(|w| {
        w.set_resetstkyclr(true);
        w.set_resetassert(true);
        w.set_key(vals::ResetKey::KEY);
    });
    tim.gprcm(0).pwren().write(|w| {
        w.set_enable(true);
        w.set_key(vals::PwrenKey::KEY);
    });
    cortex_m::asm::delay(16);

    tim.clksel().modify(|w| w.set_busclk_sel(true));
    tim.clkdiv().modify(|w| w.set_ratio(0));
    tim.commonregs(0).cclkctl().modify(|w| w.set_clken(true));

    tim.counterregs(0).load().write_value(LOAD);
    tim.counterregs(0).ctrctl().modify(|w| {
        w.set_cm(vals::Cm::DOWN);
        w.set_repeat(vals::Repeat::REPEAT_1);
        w.set_cvae(vals::Cvae::LDVAL);
        // CZC/CAC/CLC reset to a *reserved* 0x7 on some timers, which stops
        // the counter from advancing at all. Must be set to CCTL0 (0).
        w.set_czc(vals::CxC::CCTL0);
        w.set_cac(vals::CxC::CCTL0);
        w.set_clc(vals::CxC::CCTL0);
        w.set_en(false);
    });

    tim.evt_mode()
        .modify(|w| w.set_evt_cfg(1, vals::EvtCfg::HARDWARE));
    tim.gen_event(0).imask().modify(|w| w.set_z(true));
    tim.fpub(0).write(|w| w.set_chanid(EVT_CH_ADC1));
}

/// Calibration table: `(raw Hann-weighted RMS in LSB, amps on the reference
/// ammeter)`, ascending by LSB. Interpolated piecewise-linearly.
///
/// The scored quantity is agreement with the reference ammeter, not absolute
/// accuracy -- 1(1) is "误差 ... 相对于标定电流表读数". So any error a single
/// scale factor can absorb is already free, and what is left after fitting is
/// only what a straight line *cannot* absorb: curvature across the 20:1 range.
/// The CT is the suspect -- core permeability falls at low flux, so its ratio
/// error is worst at 0.1 A, exactly where 0.5% is 0.5 mA. A two-point fit
/// cannot follow that; a table can, and it needs no model of the mechanism.
///
/// Also why nothing here should "correct" toward theory: if the reference
/// instrument reads 2% low, matching it scores full marks and being right
/// scores zero. Fit what the reference says, verbatim.
///
/// TO FILL: set the load to each current, read the reference ammeter and the
/// OLED's `rms` line together, and enter the pair. Denser at the bottom --
/// that is where both the curvature and the relative sensitivity are worst.
/// Include a point past 2 A so the over-limit alarm region is interpolated
/// rather than extrapolated. Suggested: 0.10, 0.15, 0.20, 0.30, 0.50, 0.75,
/// 1.00, 1.50, 2.00, 2.20 A.
static CAL: &[(f32, f32)] = &[
    // (29.1, 0.100),
    // (43.7, 0.150),
    // ...
];

/// Fallback scale used only while CAL has fewer than two entries, so an
/// uncalibrated build still reads approximately right. Fitted at one point.
const CAL_FALLBACK_A_PER_LSB: f32 = 0.003433733774564919;

/// Piecewise-linear lookup. Outside the table the end segments are extended
/// rather than clamped -- a clamped reading would silently under-report an
/// over-limit current, and 2(3) needs `> 2 A` to raise the alarm.
fn lsb_to_amps(rms: f32) -> f32 {
    if CAL.len() < 2 {
        return CAL_FALLBACK_A_PER_LSB * rms;
    }

    // Pick the bracketing segment; fall back to the first/last segment when
    // the reading sits outside the calibrated span.
    let mut seg = CAL.len() - 2;
    for i in 0..CAL.len() - 1 {
        if rms < CAL[i + 1].0 {
            seg = i;
            break;
        }
    }

    let (x0, y0) = CAL[seg];
    let (x1, y1) = CAL[seg + 1];
    let span = x1 - x0;
    if span <= 0.0 {
        return y0; // malformed table: refuse to divide by zero
    }
    y0 + (y1 - y0) * (rms - x0) / span
}

/// How many recent frames the spread indicator remembers.
const SPREAD_N: usize = 16;

/// Peak-to-peak spread of the last SPREAD_N raw frame RMS values, in LSB.
///
/// Deliberately *displayed rather than filtered*. Cross-frame averaging was
/// tried here and removed: any IIR/running mean carries hidden state whose
/// lag is invisible on the display, and it misbehaves exactly in the band
/// that matters. A step smaller than the reset threshold but larger than the
/// 0.5% spec (say a 1.00 -> 1.01 A load nudge) is not detected as a change,
/// so the reading creeps toward it over tens of frames while the settle
/// indicator claims to be settled. A bench DMM has an aperture (NPLC) and
/// emits one reading per aperture; it keeps no memory across readings. If
/// this board turns out to need less noise, the correct lever is a longer
/// aperture -- accumulate M consecutive captures into one reading, which is
/// bounded, has no lag beyond the aperture itself, and stays fully valid one
/// aperture after any load change.
///
/// What this gives instead: the frame-to-frame spread, on the actual bench,
/// with no noise model assumed. That is the sigma measurement that decides
/// whether a longer aperture is needed at all -- and it comes for free from
/// the display, with no separate procedure.
struct Spread {
    hist: [f32; SPREAD_N],
    idx: usize,
    len: usize,
}

impl Spread {
    const fn new() -> Self {
        Self {
            hist: [0.0; SPREAD_N],
            idx: 0,
            len: 0,
        }
    }

    /// Records one frame and returns the current peak-to-peak spread.
    fn push(&mut self, x: f32) -> f32 {
        self.hist[self.idx] = x;
        self.idx = (self.idx + 1) % SPREAD_N;
        if self.len < SPREAD_N {
            self.len += 1;
        }

        let mut lo = self.hist[0];
        let mut hi = self.hist[0];
        for &v in &self.hist[..self.len] {
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
        hi - lo
    }
}

// NOTE ON THE NOISE BIAS -- deliberately *not* corrected here.
//
// Noise adds to the signal in quadrature, so the raw estimate reads
// sqrt(signal^2 + sigma^2), high by about sigma^2/(2*rms^2). It is a
// one-sided bias rather than spread, so frame averaging and any smoothing
// filter leave it untouched. At 0.1 A (rms ~29.1 LSB) it is worth 0.06% for
// sigma = 1 LSB and 0.53% for sigma = 3 LSB; at 2 A it is 400x smaller.
//
// It must NOT be corrected with a hardcoded sigma^2. The harvesting front
// end has least available power at light load, so its ripple is worst
// exactly where the spec is tightest -- sigma is a function of load current,
// not a constant. Simulating sigma rising 1 LSB (at 2 A) to 4 LSB (at 0.1 A):
// a two-point k/b fit alone leaves 0.23% worst-case, while subtracting a
// fixed sigma^2 = 9 leaves 0.41%. A constant correction is worse than none.
//
// What CAL_OFFSET does instead: a two-point fit absorbs most of this bias,
// because b lands near -sigma^2/(2*rms_lo) by construction. Residual is a
// bowl, zero at both fit points, worst near 0.2 A -- 0.12% at sigma = 3 LSB,
// 0.33% at 5, 1.26% at 10. So the bench measurement to make first is sigma
// *at several load currents*: if it stays under ~3 LSB across the range,
// CAL_OFFSET alone provably covers it and nothing more is needed. If it does
// not, the fix is to estimate the noise per frame at runtime (its power is
// separable -- all real load current sits on harmonics of the line
// frequency, so inter-harmonic bins such as 75/125/175 Hz see noise only),
// never to freeze a number into flash.
//
// To measure sigma: open the load, leave everything else powered exactly as
// in normal operation (the rectifier and supercap charging are the suspected
// source, so they must be live), and read the raw `rms` line off the OLED --
// that reading *is* sigma in LSB. Repeat under load at several currents by
// comparing the reading's spread against the reference ammeter.

/// Coarse DC pivot, integer and exact-to-1-LSB. Deliberately *not* the DC
/// estimate: it only has to land within a few LSB of the ~2048-count OPA
/// bias so the f32 accumulators downstream never hold a large sum whose
/// small difference matters. Its truncation error is solved out exactly by
/// the weighted offset in `rms_lsb`, which is why the old integer-division
/// mean (up to 1 LSB off, worth a one-sided ~0.06% RMS bias at 0.1 A) is
/// no longer a problem.
fn coarse_pivot(buf: &[u32; N]) -> i32 {
    (buf.iter().map(|&x| (x & ADC_MASK) as u64).sum::<u64>() / N as u64) as i32
}

/// Hann-weighted true RMS over one captured frame, in raw ADC-LSB units.
///
/// Why weighted: a plain rectangular average of x^2 is only unbiased when
/// the window spans a whole number of mains cycles. The error term is the
/// *weight* sequence's frequency response evaluated at 2f -- bin 20 of this
/// 200 ms record. A rectangular window's sidelobes there are only ~-35 dB,
/// which at 49.66 Hz is a 0.33% RMS error; Hann's are below -95 dB, so the
/// term vanishes. That happens without knowing the mains frequency at all,
/// which also makes it immune to SAMPLE_HZ itself being wrong (SYSOSC is an
/// internal RC, ~+-1%, and only the ratio f/SAMPLE_HZ ever mattered).
/// Harmonic h lands at bin 20h, i.e. suppressed even harder, so distorted
/// load currents are covered by the same mechanism -- and no zero crossing
/// is needed, so a dead-zone waveform is fine too.
///
/// Cost: effective noise bandwidth x1.5, taking the per-frame noise floor
/// from ~0.035% to ~0.043% at 0.1 A. Irrelevant against a 0.5% budget.
///
/// TESTING TRAP: do not bench this with a signal generator set to exactly
/// 50.000 Hz. At exactly SAMPLE_HZ/SPP_NOM the record contains only 80
/// distinct signal phases repeated 10 times, so the quantization error is
/// perfectly correlated with the signal and does not average down at all --
/// simulation puts the worst-phase error at 0.51% at 0.1 A, and no window
/// can fix it because it is not leakage. Real mains never sits exactly on
/// 50.000 Hz (and SYSOSC guarantees SAMPLE_HZ does not either), so the
/// incoherent case is the real one; at 49.8 Hz the same test gives 0.07%.
/// A generator at 50.02 Hz reproduces field behaviour, 50.000 does not.
fn rms_lsb(buf: &[u32; N], pivot: i32, t: &Tables) -> f32 {
    // Pass 1: weighted mean of the pivot-centred residual. The mean has to
    // be weighted by the *same* window as the sum of squares -- an
    // unweighted mean leaves fundamental residue that Hann cannot then
    // suppress, and re-introduces the error this function exists to remove.
    let mut sum_w = 0.0f32;
    let mut sum_wd = 0.0f32;
    for (&x, &w) in buf.iter().zip(t.hann.iter()) {
        sum_w += w;
        sum_wd += w * ((x & ADC_MASK) as i32 - pivot) as f32;
    }
    // True weighted DC, expressed as an offset from the integer pivot.
    let offset = sum_wd / sum_w;

    // Pass 2. Two passes are mandatory, not stylistic: folding this into
    // pass 1 as sum(w*x^2)/sum(w) - mean^2 subtracts two ~4.2e6 quantities
    // to recover ~848, and f32 has no mantissa left for that cancellation.
    let mut sum_wd2 = 0.0f32;
    for (&x, &w) in buf.iter().zip(t.hann.iter()) {
        let d = ((x & ADC_MASK) as i32 - pivot) as f32 - offset;
        sum_wd2 += w * d * d;
    }

    sqrtf(sum_wd2 / sum_w)
}

/// Phase of the MAINS_NOM_HZ component of one HALF-sample block, measured
/// at the block's midpoint, via direct quadrature correlation.
///
/// For x[n] = A*sin(w*n + phi) correlated against the nominal w0:
///   i_acc ~ (A/2)*sum(w)*sin(phi),  q_acc ~ (A/2)*sum(w)*cos(phi)
/// hence `atan2(i, q)`. Hann-weighted first: the image term at 2*f0 sits
/// only 10 bins away in a 400-sample block, where a rectangular window
/// leaks ~3% and would swing the phase by up to ~0.03 rad -- about 0.05 Hz.
/// A 400-point Hann is exactly the 800-point table decimated by 2.
///
/// `start` indexes the record, but the correlator is driven by the *local*
/// index, which is what makes the two blocks' phase difference come out as
/// the fundamental's advance across HALF samples.
fn half_phase(buf: &[u32; N], start: usize, pivot: i32, t: &Tables) -> f32 {
    let mut i_acc = 0.0f32;
    let mut q_acc = 0.0f32;
    for m in 0..HALF {
        let d = ((buf[start + m] & ADC_MASK) as i32 - pivot) as f32 * t.hann[2 * m];
        let p = m % SPP_NOM;
        i_acc += d * t.cos[p];
        q_acc += d * t.sin[p];
    }
    atan2f(i_acc, q_acc)
}

/// Mains frequency from the phase the fundamental advances between the two
/// halves of one record.
///
/// Across HALF samples the *nominal* fundamental advances 400*2pi/80 = 10pi
/// -- an exact multiple of 2pi -- so the wrapped phase difference is purely
/// the deviation:
///
///   dphi = 2*pi * (f - 50) * HALF / SAMPLE_HZ = 0.6283 * (f - 50)
///
/// Unambiguous over +-5 Hz, far wider than the grid ever wanders. This uses
/// all 800 samples with SNR-optimal weighting instead of the ~18 samples
/// nearest zero, so it neither cares about noise at the zero crossing nor
/// can be fooled by a spurious extra crossing.
///
/// Reported for the display and the design report only -- `rms_lsb` does
/// not consume it, so a bad frequency estimate cannot corrupt the accuracy
/// figure that carries the marks.
fn estimate_hz(buf: &[u32; N], pivot: i32, t: &Tables) -> f32 {
    let p1 = half_phase(buf, 0, pivot, t);
    let p2 = half_phase(buf, HALF, pivot, t);

    let mut dphi = p2 - p1;
    while dphi > core::f32::consts::PI {
        dphi -= TWO_PI;
    }
    while dphi <= -core::f32::consts::PI {
        dphi += TWO_PI;
    }

    let phi_per_hz = TWO_PI * HALF as f32 / SAMPLE_HZ as f32;
    MAINS_NOM_HZ as f32 + dphi / phi_per_hz
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    set_analog(PINCM_PA18);
    set_analog(PINCM_PA16);
    // X4 -- do NOT raise this to reclaim the "unused" range. The spare
    // headroom is crest-factor budget, not waste: the spec has to hold for
    // whatever load the bench uses, and 2(3) requires reading *above* 2 A to
    // raise the over-limit alarm.
    //
    // Budget, using STRUCTURE.md's observed 292..3802 OPA output swing (the
    // OPA clips well before the rails) against the DAC bias at code 2110:
    // the tight direction is upward, 3802 - 2110 = 1692 LSB. A 2 A sinusoid
    // peaks at 824 LSB, so crest factor up to ~2.9 fits at 2 A, covering
    // phase-controlled and rectifier-input loads. At X8 a plain sinusoid at
    // 2.2 A already peaks at ~1812 LSB and clips.
    //
    // VERIFY: 292..3802 is a number carried over from STRUCTURE.md, and the
    // bias landing at 2110 assumes DAC_CODE_1V7 with the OPA's own offset
    // ignored. Both are cheap to confirm from the raw `rms` line at full
    // load; the crest-factor conclusion does not change unless the usable
    // swing is far smaller than this.
    setup_opa1_pga(Gain::X4);
    init_adc1_event();
    init_timer();

    init_dsp_tables();
    // SAFETY: `init_dsp_tables` has run and nothing writes these tables
    // again, so these shared references are the only access from here on.
    let tables = unsafe {
        Tables {
            hann: &*core::ptr::addr_of!(HANN),
            cos: &*core::ptr::addr_of!(COS_TAB),
            sin: &*core::ptr::addr_of!(SIN_TAB),
        }
    };

    let mut dma = Channel::new(p.DMA_CH0, Irqs);
    // SAFETY: sole owner of the OLED's fixed pins (PB2/PB3) -- nothing
    // else in this firmware uses them. Created once, outside the loop --
    // re-running the full SSD1306 init sequence every measurement would
    // flicker the screen and waste time on the bit-banged I2C bus.
    let mut display = unsafe { Oled::new() };

    let mut led = Output::new(unsafe { peripherals::PA0::steal() }, Level::Low);
    led.set_inversion(true);

    let mut tx_green_led = Output::new(p.PB27, Level::Low);
    tx_green_led.set_inversion(true);

    // Each switch connects its GPIO to GND when ON. The internal pull-up makes
    // an open switch read as 0 and a closed switch read as 1 after inversion.
    let address_bit0 = Input::new(p.PB0, Pull::Up);
    let address_bit1 = Input::new(p.PB6, Pull::Up);
    let address_bit2 = Input::new(p.PB7, Pull::Up);
    let address_bit3 = Input::new(p.PB8, Pull::Up);

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = HC42_BAUD_RATE;
    let mut hc42 = Uart::new_blocking(p.UART2, p.PB16, p.PB15, uart_config).unwrap();

    let mut spread_track = Spread::new();

    // HC-42 needs about 300 ms after power-on before it is ready.
    Timer::after_millis(500).await;

    loop {
        // Stop the timer before re-arming so every frame starts aligned
        // on sample 0 (mirrors
        // ~/embed/Single-Phase-Power-Analyzer's Sampler::capture()).
        pac::TIMG6
            .counterregs(0)
            .ctrctl()
            .modify(|w| w.set_en(false));

        // DMAEN auto-clears when DMASZ decrements to zero (TRM 5.2.3.1)
        // -- must be re-armed before every capture, not just the first.
        pac::ADC1.ctl2().modify(|w| w.set_dmaen(true));

        // SAFETY: BUF outlives this transfer (waited on to completion
        // right below, before anything else touches it), and MEMRES0 is
        // a static peripheral register valid for the transfer's whole
        // lifetime.
        let mut xfer = unsafe {
            dma.read(
                DMA_TRIG_ADC1,
                pac::ADC1.memres(0).as_ptr() as *mut u32,
                &mut *core::ptr::addr_of_mut!(BUF),
                TransferOptions::default(),
            )
            .unwrap()
        };

        // First timer zero event starts the first conversion; the DMA
        // then drains one MEMRES0 result per event until all 800 are in.
        pac::TIMG6
            .counterregs(0)
            .ctrctl()
            .modify(|w| w.set_en(true));

        // Bounded wait -- same principle as setup_opa1_pga's RDY wait:
        // never block forever on external hardware. Busy-polling keeps
        // the CPU awake (and SWD reachable) even if a capture never
        // completes, rather than `.await`, which would let the
        // executor's idle loop put the CPU in WFE and (on this MCU+probe
        // combo) block SWD entirely.
        //
        // Checks DMASZ directly, NOT `xfer.is_running()` (req() &&
        // en()). Per the TRM (5.3, DMACTL[j].DMAREQ): "Software-
        // controlled DMA start. DMAREQ is reset automatically" -- REQ is
        // a one-shot kickoff pulse for hardware-triggered channels, not
        // an in-progress flag, so it self-clears almost immediately
        // after arming while EN and the real hardware-triggered transfer
        // keep going. DMASZ == 0 is the TRM-documented actual completion
        // signal for single transfer mode.
        //
        // Timed in cycles, not poll iterations -- a bare iteration-count
        // loop is uncalibratable (each poll is a volatile MMIO read plus
        // a fence, whose real cost on this core is opaque). 800 samples
        // at 4 kHz only need 200 ms if the chain works, so 1000 checks x
        // ~1 ms apart gives 5x margin and a predictable ~1 s cap.
        let mut captured = false;
        for _ in 0..1000u32 {
            if pac::DMA.chan(0).sz().read().size() == 0 {
                captured = true;
                break;
            }
            cortex_m::asm::delay(32_000); // ~1 ms at 32 MHz
        }
        if !captured {
            xfer.request_pause();
        }

        // Capture done; BUF now holds 800 hardware-timed samples of
        // OPA1_OUT at 4 kHz (TIM+ADC+DMA, zero CPU involvement per
        // sample).
        // SAFETY: the DMA transfer above either completed or was
        // paused, so nothing else touches BUF concurrently with this
        // read.
        // SAFETY: as above -- the transfer is finished or paused, so BUF is
        // ours for the rest of this iteration.
        let buf = unsafe { &*core::ptr::addr_of!(BUF) };
        let pivot = coarse_pivot(buf);
        // One capture, one reading -- no cross-frame state. See `Spread`.
        let rms = rms_lsb(buf, pivot, &tables);
        let spread = spread_track.push(rms);
        let hz = estimate_hz(buf, pivot, &tables);
        let amps = lsb_to_amps(rms);

        if let Ok(display) = &mut display {
            let _ = display.clear(BinaryColor::Off);
            let style = MonoTextStyle::new(&FONT_9X15, BinaryColor::On);

            let mut line: String<32> = String::new();
            let _ = write!(line, "{:.3} A", amps);
            let _ = Text::new(&line, Point::new(4, 20), style).draw(display);

            // Frequency, plus peak-to-peak spread of the last 16 frames in
            // LSB -- read this to find out whether noise is actually a
            // problem on this board. 0.5% at 0.1 A is 0.146 LSB, so a spread
            // much above ~0.3 means a longer aperture is needed.
            let mut line2: String<32> = String::new();
            let _ = write!(line2, "{:.2}Hz p{:.2}", hz, spread);
            let _ = Text::new(&line2, Point::new(4, 40), style).draw(display);

            // Raw single-frame RMS in LSB -- the number to enter into CAL.
            // Two decimals: 0.01 LSB is 0.03% at 0.1 A, so the display
            // resolution is not what limits the calibration.
            let mut line3: String<32> = String::new();
            let _ = write!(line3, "rms {:.2}", rms);
            let _ = Text::new(&line3, Point::new(4, 60), style).draw(display);

            let _ = display.flush();
        }

        // Address dip switches, read once per cycle -- the Android app
        // re-derives alarm status from CURRENT_MA itself (see
        // MeterReading::classify), so STATUS here is just a fixed token
        // that satisfies the app's frame parser.
        let address = u8::from(address_bit0.is_low())
            | (u8::from(address_bit1.is_low()) << 1)
            | (u8::from(address_bit2.is_low()) << 2)
            | (u8::from(address_bit3.is_low()) << 3);
        let current_ma = (amps * 1000.0) as u32;

        let mut frame: String<64> = String::new();
        let _ = write!(
            frame,
            "METER_TEST,ADDR={:02},CURRENT_MA={},STATUS=NORMAL\r\n",
            address, current_ma
        );
        let _ = hc42.blocking_write(frame.as_bytes());
        let _ = hc42.blocking_flush();
        tx_green_led.toggle();

        // Heartbeat: one toggle per measurement cycle.
        // led.toggle();
    }
}
