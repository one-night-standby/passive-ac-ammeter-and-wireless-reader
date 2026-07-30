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

/// ADC1 channel numbers for the two pins above, taken from the PAC's own pin
/// table (`impl_adc_pin!(ADC1, PA16, 1u8)`, `impl_adc_pin!(ADC1, PA18, 3u8)`).
///
/// PA18 being an ADC channel *in its own right* is what makes the unity-gain
/// probe free: the raw input can be sampled without touching the OPA at all,
/// so the probe costs one MEMCTL write instead of an OPA disable/re-enable.
/// (True x1 is not reachable through the PGA ladder -- TRM Table 21-6 lists
/// non-inverting GAIN=0x0 as "Not valid", the ladder starts at 2x. The only
/// other x1 path is buffer mode, NSEL=RTOP with GAIN=0x0, and TRM 21.2.8
/// requires the full clear-ENABLE/rewrite-CFG/set-ENABLE dance plus an OPA
/// enable wait every time GAIN crosses 0x0.)
const ADC_CH_OPA_OUT: u8 = 1; // PA16
const ADC_CH_RAW_IN: u8 = 3; // PA18

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

/// Unity-gain probe frame: 160 samples = 40 ms = two nominal mains cycles.
/// Only mean and peak-to-peak come out of it, and both are settled after one
/// cycle, so a second cycle is already margin. Kept short because it is pure
/// overhead on every measurement period, and startup time is a scored item.
const PROBE_N: usize = 160;
/// Separate storage rather than the head of `BUF`. Sharing would be 640 bytes
/// cheaper but creates a real failure mode: if the *main* capture times out,
/// the first 160 words of BUF would still hold unamplified probe samples
/// spliced onto the previous frame's tail, and the DSP would report a
/// confident, entirely fictional number instead of a stale one.
static mut PROBE_BUF: [u32; PROBE_N] = [0; PROBE_N];

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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gain {
    X2 = 1,
    X4 = 2,
    X8 = 3,
    X16 = 4,
    X32 = 5,
}

/// A measurement range: either the amplifier, or no amplifier.
///
/// `Direct` is unity gain done the only way this chip offers it for free --
/// the ADC reads PA18 itself, exactly as the probe does, with the OPA left
/// out of the signal path entirely. That makes it a genuine range rather than
/// a special case, and it is the one the ladder cannot reach: the PGA floor
/// is 2x, and 2x amplifies the input's DC along with its signal, so the
/// output cannot exceed 2*Vin_dc whatever the DAC does. An input sitting low
/// therefore loses the top of its span at *every* PGA step while `Direct`
/// still holds it, because nothing there multiplies the offset.
///
/// `Direct` is also provably the least demanding range, which is what makes
/// it the right floor to fall back to. Against `Pga(X2)` it needs half the
/// swing (multiplier 1 versus 2) while its window bound is `2*mean - OUT_LO`
/// tighter at worst -- so wherever `Pga(X2)` fits, `Direct` fits too.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    /// ADC1 reads PA18 straight, OPA bypassed. No gain, no DAC pivot.
    Direct,
    /// OPA1 as a non-inverting PGA at this ladder step.
    Pga(Gain),
}

impl Range {
    /// Ascending, which `pick_range` relies on to keep the last one that fits.
    const ALL: [Range; 6] = [
        Range::Direct,
        Range::Pga(Gain::X2),
        Range::Pga(Gain::X4),
        Range::Pga(Gain::X8),
        Range::Pga(Gain::X16),
        Range::Pga(Gain::X32),
    ];

    /// Nominal multiplier. The CFG.GAIN encoding is the log2 of the ladder
    /// step, so 1..=5 maps to 2..=32 by a shift; `Direct` is 1 by definition,
    /// and exactly 1 -- it is a wire, not a trimmed resistor ratio.
    ///
    /// Used *only* for range arithmetic and the DAC pivot, never in the
    /// reading itself. That distinction matters: the on-chip ladder's real
    /// ratios deviate from these nominal values by roughly the same order as
    /// the 0.5% accuracy budget, so a reading derived from `nominal()` would
    /// blow the spec on its own. Each range instead carries its own CAL table,
    /// which absorbs its true ratio along with everything else.
    const fn nominal(self) -> i32 {
        match self {
            Range::Direct => 1,
            Range::Pga(g) => 1 << (g as u8),
        }
    }

    /// Which ADC channel carries this range's signal.
    const fn channel(self) -> u8 {
        match self {
            Range::Direct => ADC_CH_RAW_IN,
            Range::Pga(_) => ADC_CH_OPA_OUT,
        }
    }

    /// The window samples must stay inside. The PGA path is bounded by the
    /// OPA's own clipping; the direct path has no OPA in it, so the only
    /// bound is the converter's full scale.
    const fn window(self) -> (i32, i32) {
        match self {
            Range::Direct => (0, ADC_MASK as i32),
            Range::Pga(_) => (OUT_LO, OUT_HI),
        }
    }
}

/// Usable OPA output window in ADC LSB, from STRUCTURE.md's observed
/// 292..3802 -- the OPA clips well before the rails, so the rails are not the
/// limit that matters.
///
/// This is the one bench-derived constant the autoranger still leans on, and
/// unlike the numbers it replaced it is a property of the OPA and its supply
/// rather than of whatever is driving the input, so it carries over from one
/// signal source to another. Re-measure it by driving the input past clipping
/// at a known gain and reading where `rms` stops growing; nothing else in
/// this file needs to change if it moves.
const OUT_LO: i32 = 292;
const OUT_HI: i32 = 3802;
const OUT_CENTER: i32 = (OUT_LO + OUT_HI) / 2;

/// DAC12 full-scale code. The pivot solution below is not always reachable
/// -- see `dac_code_for` -- so this bound is part of the range arithmetic,
/// not just a defensive clamp.
const DAC_MAX: i32 = 4095;

/// Fraction of the *available headroom* the chosen range aims to fill, in
/// percent. Headroom, not total span: when the input's DC forces the output
/// off centre the two sides are not equal, and only the tighter one can be
/// spent.
///
/// The old fixed-x4 build had to budget crest factor blindly (it assumed a
/// sinusoid and reserved room for a CF of ~2.9). The probe measures actual
/// peak-to-peak, so distorted loads are handled by construction and this
/// margin only has to cover what the probe cannot see: a load that changes
/// between the probe and the main frame, peaks rarer than the probe's 40 ms
/// window, and noise on the probe's own pp estimate.
const FILL_TARGET_PCT: i32 = 65;
/// Re-range thresholds, in percent of available headroom, applied to the
/// projected fill at the *current* range. Deliberately wider than
/// FILL_TARGET_PCT on both sides -- see `next_range`.
const RERANGE_HI_PCT: i32 = 75;
const RERANGE_LO_PCT: i32 = 25;

/// DAC12 code that places the amplified output's DC on `OUT_CENTER`, given
/// the gain and the input's measured DC.
///
/// The non-inverting PGA with MSEL=DAC12OUT has transfer function
///
///   Vout = Vdac + G*(Vin - Vdac) = G*Vin + (1 - G)*Vdac
///
/// so the DAC does not touch the gain at all -- it is the single free
/// parameter that sets where the amplification pivots. Solving for the DAC
/// that lands the output DC on a chosen centre C:
///
///   Vdac = (C - G*Vin_dc) / (1 - G) = (G*Vin_dc - C) / (G - 1)
///
/// Nothing here assumes the input's DC is anywhere in particular; Vin_dc is
/// whatever the probe measured, and the expression re-centres around it. That
/// is the point. The old build froze the DAC at 1.7 V, which is only correct
/// for a source that happens to sit there -- and the sensitivity is
/// dVout_dc/dVin_dc = G, so at 32x a 50 mV mismatch drives the output 1.6 V
/// off centre and into the clip.
///
/// The solution is not always reachable. Both bounds work out to *lower*
/// limits on G: dac >= 0 needs G >= C/Vin_dc, and dac <= FS needs
/// G >= (FS - C)/(FS - Vin_dc). An input sitting well below centre therefore
/// cannot be centred at low gain at all -- at 2x the output can never exceed
/// 2*Vin_dc, whatever the DAC does. Clamping is the right response (it still
/// gets as close to centre as the part allows), but it means the clamped case
/// must be *checked* rather than assumed away, which is what `headroom` and
/// `fits` below do.
///
/// Arithmetic is in raw codes, not volts. The DAC and the ADC are both 12-bit
/// and both referenced to VDDA/VSSA, so their codes share one scale and VDDA
/// cancels out of the expression entirely -- the placement does not care what
/// the supply actually is.
fn dac_code_for(gain: Gain, mean_in: i32) -> u16 {
    let g = g_nominal(gain);
    ((g * mean_in - OUT_CENTER) / (g - 1)).clamp(0, DAC_MAX) as u16
}

/// Where this range's DC actually lands. For `Direct` that is the input's own
/// DC, untouched -- there is nothing in the path to move it. For the PGA it is
/// `OUT_CENTER`, except where `dac_code_for` had to clamp.
fn dc_for(range: Range, mean_in: i32) -> i32 {
    match range {
        Range::Direct => mean_in,
        Range::Pga(g) => {
            let d = dac_code_for(g, mean_in) as i32;
            d + g_nominal(g) * (mean_in - d)
        }
    }
}

/// `Range::nominal` for a bare ladder step, so `dac_code_for` and friends do
/// not have to wrap one in a `Range` just to read its multiplier.
const fn g_nominal(g: Gain) -> i32 {
    Range::Pga(g).nominal()
}

/// Usable half-swing on this range: the distance from its DC to the nearer
/// end of its window. Zero (or negative, reported as zero) means the DC is
/// already outside the window and no signal fits at all.
fn headroom(range: Range, mean_in: i32) -> i32 {
    let (lo, hi) = range.window();
    let dc = dc_for(range, mean_in);
    (dc - lo).min(hi - dc).max(0)
}

/// Does this range's signal fit inside `fill_pct` of the headroom it actually
/// has? Both constraints -- signal size and DC placement -- are enforced here,
/// so a range that cannot be centred is rejected for the right reason instead
/// of quietly clipping.
fn fits(range: Range, mean_in: i32, pp_in: u32, fill_pct: i32) -> bool {
    let half = range.nominal() * pp_in.max(1) as i32 / 2;
    half * 100 <= headroom(range, mean_in) * fill_pct
}

/// Largest range that fits, falling back to `Direct` when none does.
///
/// The fallback is not a guess: `Direct` is the least demanding range there
/// is (see `Range`), so if it does not fit, nothing would have. Reaching it
/// means the input's own excursion leaves 0..VDDA -- an analog front-end
/// limit that no choice inside this chip can widen, which is why the caller
/// flags that case rather than the planner hiding it.
fn pick_range(mean_in: i32, pp_in: u32) -> Range {
    let mut best = Range::Direct;
    for r in Range::ALL {
        if fits(r, mean_in, pp_in, FILL_TARGET_PCT) {
            best = r;
        }
    }
    best
}

/// Whether the signal genuinely will not fit the window on this range -- as
/// opposed to merely exceeding `FILL_TARGET_PCT`, which is a margin, not a
/// limit. Tested at 100% so the flag means "this frame is clipping", not
/// "this frame spent its reserve".
fn over_range(range: Range, mean_in: i32, pp_in: u32) -> bool {
    !fits(range, mean_in, pp_in, 100)
}

/// Range decision with hysteresis.
///
/// The hysteresis is not cosmetic. Each range carries its own CAL table,
/// so the tables' residual disagreement -- which is exactly the ladder ratio
/// error the per-range tables exist to hide -- becomes visible the moment the
/// range hops. A load parked on a threshold would otherwise re-range every
/// period and make the display alternate between two values that are both
/// "calibrated". Holding the current range across a wide band means a hop is
/// a real range change, not dither.
///
/// The bands are chosen so one step of correction always lands inside them:
/// re-ranging down from >75% fill halves it to ~37%, re-ranging up from <25%
/// roughly doubles it. Neither lands back outside, so a steady load cannot
/// ping-pong. (Only "roughly": changing the range also moves the DAC pivot and
/// hence the headroom the fill is measured against, so the factor is exactly
/// two only while the output stays centred. The Direct/x2 boundary is not a
/// factor of two at all, for the same reason.)
fn next_range(current: Range, mean_in: i32, pp_in: u32) -> Range {
    let too_hot = !fits(current, mean_in, pp_in, RERANGE_HI_PCT);
    let too_cold = fits(current, mean_in, pp_in, RERANGE_LO_PCT);
    if too_hot || too_cold {
        pick_range(mean_in, pp_in)
    } else {
        current
    }
}

/// Mean, peak-to-peak, and an out-of-range flag for one probe frame, all in
/// raw ADC LSB at the input pin (no gain applied, so they describe the signal
/// itself).
///
/// The flag matters because the probe is the one place the firmware looks at
/// the outside world with no gain to fall back on: an input that leaves
/// 0..VDDA is simply not visible: it reads as a flat top or bottom, which
/// makes pp too small and the mean wrong in the same direction. Acting on
/// those numbers would pick too high a gain and then clip the frame that
/// carries the marks. No gain setting can fix an input outside the supply --
/// that needs external conditioning -- so the only honest response is to
/// report it, which is what the caller does with this flag.
fn probe_stats(buf: &[u32]) -> (i32, u32, bool) {
    let mut lo = u32::MAX;
    let mut hi = 0u32;
    let mut sum = 0u64;
    for &x in buf {
        let v = x & ADC_MASK;
        sum += v as u64;
        if v < lo {
            lo = v;
        }
        if v > hi {
            hi = v;
        }
    }
    let railed = lo == 0 || hi >= ADC_MASK;
    ((sum / buf.len() as u64) as i32, hi - lo, railed)
}

/// OPA1 as a non-inverting PGA (TRM 21.2.7.3.2): PSEL=EXTPIN1 (PA18, through
/// the P-MUX into the amplifier's true input -- datasheet SS7.19.1 puts this
/// mux's series resistance at 2.6 kOhm TYP, but the real op-amp input beyond
/// it draws only leakage-level bias current, so that resistance drops
/// negligible voltage and doesn't meaningfully load an external source),
/// NSEL=RTAP (the ladder tap feeds the inverting input), MSEL=DAC12OUT
/// (ladder bottom biased to the DAC instead of ground, so the gain acts on
/// (Vin - Vdac) rather than Vin itself -- same pattern as
/// ~/embed/Single-Phase-Power-Analyzer's OPA1 setup, just a different bias
/// point and gain step). GAIN=the selected step. OUTPIN=1 drives PA16.
///
/// `gain` and `dac_code` are only the *starting* range; both are re-derived
/// from the input every measurement period. See `dac_code_for`.
fn setup_opa1_pga(gain: Gain, dac_code: u16) {
    // The bias reference must be up and stable before the OPA turns on.
    dac::init();
    dac::set(dac_code);

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

/// Select a measurement range: ADC channel, and for the PGA ranges the gain
/// step and bias pivot too.
///
/// The gain write is a live `modify` with the OPA still enabled, which TRM
/// 21.2.8 explicitly permits ("allows customers to dynamically change the OPA
/// gain without disabling OPA functionality") under two conditions, both of
/// which hold here: the other CFG bits are left untouched, and GAIN never
/// takes the value 0x0 -- crossing 0x0 is the one transition that would force
/// the disable/reconfigure/enable sequence and its OPA enable wait. `Gain`
/// cannot represent 0x0, so that case is unreachable by construction, and
/// `Range::Direct` reaches unity by leaving the OPA out of the path rather
/// than by driving GAIN to 0x0.
///
/// `Direct` deliberately leaves the OPA running. Powering it down would save
/// current -- 1(2) scores low-power operation -- but it would owe an OPA
/// enable wait on the way back, and `Direct` is only ever selected when the
/// input's DC makes amplification impossible, which is a fault condition
/// rather than a normal operating point. Not worth the latency on a path that
/// should never be hot.
///
/// Returns whether the analog path actually changed, so the caller can skip
/// the settle delay when it did not.
fn apply_range(range: Range, mean_in: i32) -> bool {
    set_adc_channel(range.channel());
    match range {
        Range::Direct => false,
        Range::Pga(g) => {
            pac::OPA1.cfg().modify(|w| w.set_gain(g as u8));
            dac::set(dac_code_for(g, mean_in));
            true
        }
    }
}

/// Point ADC1's single conversion at a different input pin.
///
/// MEMCTL is rewritten with ENC dropped: the conversion sequence must not be
/// live while its own channel select moves out from under it. Costs two
/// register writes, which is the whole reason the probe reads PA18 directly
/// instead of putting the OPA into buffer mode.
fn set_adc_channel(ch: u8) {
    let regs = pac::ADC1;
    regs.ctl0().modify(|w| w.set_enc(false));
    regs.memctl(0).modify(|w| w.set_chansel(ch));
    regs.ctl0().modify(|w| w.set_enc(true));
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
/// One table per range, rather than one shared table plus a gain factor.
/// The autoranger makes the range part of the measurement, and the ladder's
/// real step ratios are not the nominal 2/4/8/16/32 -- their deviation is the
/// same order as the whole 0.5% budget. A per-range table absorbs that ratio
/// error, the range's own DC behaviour, and the CT curvature above, all in one
/// set of bench readings, with no separate gain-trim constant to measure or
/// to get wrong.
///
/// TO FILL: set the load to each current, read the reference ammeter and the
/// OLED's `rms` line together, and enter the pair *in the table for the gain
/// the OLED is showing*. Denser at the bottom -- that is where both the
/// curvature and the relative sensitivity are worst.
///
/// Only fill the span each step actually sees. With FILL_TARGET_PCT the
/// autoranger picks x32 at 0.1 A and drops toward x4 near 2 A, so most steps
/// only need the few hundred mA around their own band, plus enough overlap
/// past each re-range threshold that neighbouring tables agree where they
/// meet -- a disagreement there shows up as a step in the reading every time
/// the range changes. The step in use at 2 A additionally needs a point past
/// 2 A, so the over-limit alarm region is interpolated rather than
/// extrapolated.
/// `CAL_X1` is the `Direct` range. It only gets selected when the input's own
/// DC leaves no room to amplify, so on a front end that biases near mid-rail
/// it will never be entered and can stay empty; fill it only if the bench
/// actually shows `x1` on the OLED.
static CAL_X1: &[(f32, f32)] = &[];
static CAL_X2: &[(f32, f32)] = &[];
static CAL_X4: &[(f32, f32)] = &[
    // (582.0, 2.000),
    // (640.0, 2.200),
];
static CAL_X8: &[(f32, f32)] = &[];
static CAL_X16: &[(f32, f32)] = &[];
static CAL_X32: &[(f32, f32)] = &[
    // (233.0, 0.100),
    // (349.0, 0.150),
    // ...
];

fn cal_for(range: Range) -> &'static [(f32, f32)] {
    match range {
        Range::Direct => CAL_X1,
        Range::Pga(Gain::X2) => CAL_X2,
        Range::Pga(Gain::X4) => CAL_X4,
        Range::Pga(Gain::X8) => CAL_X8,
        Range::Pga(Gain::X16) => CAL_X16,
        Range::Pga(Gain::X32) => CAL_X32,
    }
}

/// Fallback scale used only while a step's table has fewer than two entries,
/// so an uncalibrated build still reads approximately right on every step.
/// Fitted at one point, at x4; other steps are extrapolated by the nominal
/// ratio, which is precisely the approximation the tables exist to replace.
const CAL_FALLBACK_A_PER_LSB: f32 = 0.003433733774564919;
const CAL_FALLBACK_GAIN: f32 = 4.0;

/// Both columns of a CAL table must increase together. Checked at compile
/// time because the tables are typed in by hand, one pair per bench reading,
/// and a transposed or out-of-order pair does not fail loudly at runtime --
/// it just interpolates on the wrong segment and reads plausibly wrong.
/// NaN also fails this (every comparison against NaN is false), so a
/// mistyped literal cannot slip through either.
const fn cal_strictly_ascending(cal: &[(f32, f32)]) -> bool {
    let mut i = 1;
    while i < cal.len() {
        if !(cal[i].0 > cal[i - 1].0) || !(cal[i].1 > cal[i - 1].1) {
            return false;
        }
        i += 1;
    }
    true
}
const CAL_ORDER_MSG: &str = "CAL entries must be (lsb, amps) pairs, both strictly increasing";
const _: () = assert!(cal_strictly_ascending(CAL_X1), "{}", CAL_ORDER_MSG);
const _: () = assert!(cal_strictly_ascending(CAL_X2), "{}", CAL_ORDER_MSG);
const _: () = assert!(cal_strictly_ascending(CAL_X4), "{}", CAL_ORDER_MSG);
const _: () = assert!(cal_strictly_ascending(CAL_X8), "{}", CAL_ORDER_MSG);
const _: () = assert!(cal_strictly_ascending(CAL_X16), "{}", CAL_ORDER_MSG);
const _: () = assert!(cal_strictly_ascending(CAL_X32), "{}", CAL_ORDER_MSG);

/// Piecewise-linear lookup in the active range's table. Outside the table the
/// end segments are extended rather than clamped -- a clamped reading would
/// silently under-report an over-limit current, and 2(3) needs `> 2 A` to
/// raise the alarm.
fn lsb_to_amps(rms: f32, range: Range) -> f32 {
    let cal = cal_for(range);
    if cal.len() < 2 {
        let scale = CAL_FALLBACK_A_PER_LSB * CAL_FALLBACK_GAIN / range.nominal() as f32;
        return scale * rms;
    }

    // Pick the bracketing segment; fall back to the first/last segment when
    // the reading sits outside the calibrated span.
    let mut seg = cal.len() - 2;
    for i in 0..cal.len() - 1 {
        if rms < cal[i + 1].0 {
            seg = i;
            break;
        }
    }

    let (x0, y0) = cal[seg];
    let (x1, y1) = cal[seg + 1];
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

/// One hardware-timed capture of `dst.len()` samples from whichever channel
/// `set_adc_channel` last selected. TIM+ADC+DMA, zero CPU involvement per
/// sample. Returns false if the transfer never drained, in which case `dst`
/// holds a mix of new and stale samples and must not be interpreted.
///
/// Factored out of the main loop so the probe and the measurement frame run
/// the identical arming sequence -- the pieces that are easy to drop (the
/// timer stop, the DMAEN re-arm) are exactly the ones that fail silently.
fn capture(dma: &mut Channel<'_>, dst: &mut [u32]) -> bool {
    // Read the length before `dst` is handed to the DMA. Deriving the timeout
    // from DMASZ instead would read a counter the hardware has already begun
    // decrementing, shortening the budget by however long the arming took.
    let len = dst.len() as u32;

    // Stop the timer before re-arming so every frame starts aligned on
    // sample 0 (mirrors ~/embed/Single-Phase-Power-Analyzer's
    // Sampler::capture()).
    pac::TIMG6
        .counterregs(0)
        .ctrctl()
        .modify(|w| w.set_en(false));

    // DMAEN auto-clears when DMASZ decrements to zero (TRM 5.2.3.1) -- must
    // be re-armed before every capture, not just the first.
    pac::ADC1.ctl2().modify(|w| w.set_dmaen(true));

    // SAFETY: the transfer is waited on to completion (or paused) before this
    // function returns, so `dst` is not touched concurrently, and MEMRES0 is
    // a static peripheral register valid for the transfer's whole lifetime.
    let mut xfer = unsafe {
        dma.read(
            DMA_TRIG_ADC1,
            pac::ADC1.memres(0).as_ptr() as *mut u32,
            dst,
            TransferOptions::default(),
        )
        .unwrap()
    };

    // First timer zero event starts the first conversion; the DMA then drains
    // one MEMRES0 result per event until every sample is in.
    pac::TIMG6
        .counterregs(0)
        .ctrctl()
        .modify(|w| w.set_en(true));

    // Bounded wait -- same principle as setup_opa1_pga's RDY wait: never
    // block forever on external hardware. Busy-polling keeps the CPU awake
    // (and SWD reachable) even if a capture never completes, rather than
    // `.await`, which would let the executor's idle loop put the CPU in WFE
    // and (on this MCU+probe combo) block SWD entirely.
    //
    // Checks DMASZ directly, NOT `xfer.is_running()` (req() && en()). Per the
    // TRM (5.3, DMACTL[j].DMAREQ): "Software-controlled DMA start. DMAREQ is
    // reset automatically" -- REQ is a one-shot kickoff pulse for
    // hardware-triggered channels, not an in-progress flag, so it self-clears
    // almost immediately after arming while EN and the real
    // hardware-triggered transfer keep going. DMASZ == 0 is the
    // TRM-documented actual completion signal for single transfer mode.
    //
    // Timed in cycles, not poll iterations -- a bare iteration-count loop is
    // uncalibratable (each poll is a volatile MMIO read plus a fence, whose
    // real cost on this core is opaque). The budget is 5x the frame's own
    // duration, so it scales with the frame instead of being a constant that
    // silently becomes a 25x wait for the 40 ms probe.
    let budget_ms = len * 5 / (SAMPLE_HZ / 1000);
    let mut captured = false;
    for _ in 0..budget_ms {
        if pac::DMA.chan(0).sz().read().size() == 0 {
            captured = true;
            break;
        }
        cortex_m::asm::delay(32_000); // ~1 ms at 32 MHz
    }
    if !captured {
        xfer.request_pause();
    }
    captured
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    set_analog(PINCM_PA18);
    set_analog(PINCM_PA16);
    // Starting range only, and deliberately the pessimistic end of the
    // ladder: x2 cannot clip anything a higher step would have handled. The
    // starting DAC code is mid-scale for the same reason -- not because the
    // input is expected there, but because it is the choice that assumes
    // least. Neither survives the first period: the probe reads the input at
    // unity gain, and `next_range`/`dac_code_for` derive both from what it
    // actually measured. Convergence takes one period rather than several,
    // because the probe's numbers are input-referred and so do not depend on
    // which range happens to be selected while it runs.
    setup_opa1_pga(Gain::X2, (DAC_MAX / 2) as u16);
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
    let mut range = Range::Pga(Gain::X2);
    let mut input_bad = false;
    let mut range_over = false;

    // HC-42 needs about 300 ms after power-on before it is ready.
    Timer::after_millis(500).await;

    loop {
        // --- Probe: the raw input at unity gain, straight off PA18 ---
        //
        // The OPA is not involved and does not move, so this costs one MEMCTL
        // write and 40 ms, with no settle. Both numbers it yields are
        // input-referred and therefore independent of the gain currently
        // selected, which is what lets the range converge in a single period
        // rather than hunting toward it.
        set_adc_channel(ADC_CH_RAW_IN);
        // SAFETY: `capture` waits the transfer out (or pauses it) before
        // returning, so nothing else touches PROBE_BUF concurrently.
        let probed = capture(&mut dma, unsafe { &mut *core::ptr::addr_of_mut!(PROBE_BUF) });
        if probed {
            // SAFETY: as above -- the transfer is finished or paused.
            let (mean_in, pp_in, railed) =
                probe_stats(unsafe { &*core::ptr::addr_of!(PROBE_BUF) });
            input_bad = railed;
            if !railed {
                range = next_range(range, mean_in, pp_in);
                range_over = over_range(range, mean_in, pp_in);
                if apply_range(range, mean_in) {
                    // Settle the DAC and the re-tapped ladder before the frame
                    // that carries the marks. Both are microsecond-scale; 1 ms
                    // is slack, and only 0.4% of the measurement period.
                    // Skipped on Direct, which changes nothing analog.
                    Timer::after_millis(1).await;
                }
            }
        }
        // A failed or railed probe leaves the range exactly as it was.
        // Re-ranging on numbers known to be wrong is strictly worse than
        // keeping a setting that was right one period ago -- and in the
        // railed case the reading is flagged rather than silently trusted,
        // because an input outside 0..VDDA is an analog front-end problem
        // that no range inside this chip can fix.

        // --- Measurement frame: whatever the chosen range put the signal on
        // (OPA1_OUT on PA16 for the PGA ranges, PA18 itself for Direct, in
        // which case the channel is already the one the probe just used) ---
        // SAFETY: `capture` waits the transfer out (or pauses it) before
        // returning, so nothing else touches BUF concurrently.
        let framed = capture(&mut dma, unsafe { &mut *core::ptr::addr_of_mut!(BUF) });

        // BUF now holds 800 hardware-timed samples of OPA1_OUT at 4 kHz
        // (TIM+ADC+DMA, zero CPU involvement per sample).
        // SAFETY: as above -- the transfer is finished or paused, so BUF is
        // ours for the rest of this iteration.
        let buf = unsafe { &*core::ptr::addr_of!(BUF) };
        let pivot = coarse_pivot(buf);
        // One capture, one reading -- no cross-frame state. See `Spread`.
        let rms = rms_lsb(buf, pivot, &tables);
        let spread = spread_track.push(rms);
        let hz = estimate_hz(buf, pivot, &tables);
        let amps = lsb_to_amps(rms, range);

        if let Ok(display) = &mut display {
            let _ = display.clear(BinaryColor::Off);
            let style = MonoTextStyle::new(&FONT_9X15, BinaryColor::On);

            // A leading marker means the reading below it is not to be
            // believed, and says which of the two ways it went wrong:
            //   '!' the probe found the input pinned at an ADC end, so the
            //       signal is leaving 0..VDDA and no internal gain or bias
            //       choice can recover it -- that needs external conditioning.
            //   '>' the arithmetic says the signal will not fit even on
            //       Direct, the least demanding range there is, so the frame
            //       is clipping. Since Direct's window is the converter's own
            //       full scale, this is the same physical condition as '!' --
            //       predicted from the probe's mean/pp rather than seen as
            //       pinned samples. Adding Direct to the ladder is what made
            //       it near-unreachable; it is kept as the arithmetic
            //       backstop, and stays correct if the windows are re-measured.
            //   '?' the measurement frame's DMA never drained, so BUF is part
            //       new samples and part stale ones.
            // All three sit on the primary line on purpose: a number this
            // display cannot stand behind should not look like the others.
            let marker = if input_bad {
                "!"
            } else if range_over {
                ">"
            } else if !framed {
                "?"
            } else {
                ""
            };
            let mut line: String<32> = String::new();
            let _ = write!(line, "{}{:.3} A", marker, amps);
            let _ = Text::new(&line, Point::new(4, 20), style).draw(display);

            // Frequency, plus peak-to-peak spread of the last 16 frames in
            // LSB -- read this to find out whether noise is actually a
            // problem on this board. 0.5% at 0.1 A is 0.146 LSB, so a spread
            // much above ~0.3 means a longer aperture is needed.
            let mut line2: String<32> = String::new();
            let _ = write!(line2, "{:.2}Hz p{:.2}", hz, spread);
            let _ = Text::new(&line2, Point::new(4, 40), style).draw(display);

            // Raw single-frame RMS in LSB and the gain it was taken at --
            // together they are one row of one CAL table, and the gain says
            // *which* table, so the calibration cannot be entered against the
            // wrong one. Two decimals: 0.01 LSB is 0.03% at 0.1 A, so the
            // display resolution is not what limits the calibration.
            //
            // Starts at x=0, not x=4 like the others: 14 characters of
            // FONT_9X15 is exactly 126 px, so the margin is the difference
            // between fitting and losing the last digit of the gain.
            let mut line3: String<32> = String::new();
            let _ = write!(line3, "rms {:.2} x{}", rms, range.nominal());
            let _ = Text::new(&line3, Point::new(0, 60), style).draw(display);

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
