//! The analog front end and the autoranger that drives it: OPA1 as a
//! non-inverting PGA, the DAC12 bias pivot, and the arithmetic that picks a
//! range from what the unity-gain probe measured.
//!
//! The range planning is deliberately free of register access (`fits`,
//! `headroom`, `pick_range`, `next_range`, `dac_code_for` are pure integer
//! functions of the probe's mean/pp), so it can be reasoned about -- and
//! simulated -- without hardware in the loop. `apply_range` is the one place
//! that touches silicon.

use embassy_mspm0::pac;

use crate::dac;
use crate::sampler::{ADC_CH_OPA_OUT, ADC_CH_RAW_IN, ADC_MASK, set_adc_channel};

/// Non-inverting PGA gain steps. The on-chip resistor ladder only has these
/// five fixed taps -- no continuous adjustment, and no step below 2x (unity
/// is a separate topology, plain buffer mode, not part of this ladder).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Gain {
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
/// it the right floor to fall back to. Every range is bounded by the same
/// window (see `OUT_LO`), and against `Pga(X2)` this one needs half the swing
/// inside it -- multiplier 1 versus 2. So wherever `Pga(X2)` fits, `Direct`
/// fits too.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Range {
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
    /// step, so `1..=5` maps to `2..=32` by a shift; `Direct` is 1 by
    /// definition, and exactly 1 -- it is a wire, not a trimmed resistor ratio.
    ///
    /// Used *only* for range arithmetic and the DAC pivot, never in the
    /// reading itself. That distinction matters: the on-chip ladder's real
    /// ratios deviate from these nominal values by roughly the same order as
    /// the 0.5% accuracy budget, so a reading derived from `nominal()` would
    /// blow the spec on its own. Each range instead carries its own CAL table,
    /// which absorbs its true ratio along with everything else.
    pub const fn nominal(self) -> i32 {
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
}

/// Window samples must stay inside, in ADC LSB. One window for every range,
/// because there is only one limit here and everything shares it: the ADC's
/// reference.
///
/// ADC1 converts against the 2.5 V VREF (`sampler::init_adc1_event` sets
/// VRSEL=2), so full scale `0..4095` is `0..2.5 V`. OPA1 still runs off
/// VDDA/VSSA and can swing above that, which makes the converter the binding
/// limit rather than the amplifier -- and a strictly cleaner one: past 2.5 V
/// the code saturates at 4095 exactly, with no soft shoulder to model, while
/// the amplifier is still in its linear region and not adding compression of
/// its own. So a hard bound is the right model and `fits` owes no compression
/// margin on top of `FILL_TARGET_PCT`.
///
/// The consequence worth stating plainly: an input driven past full scale is
/// invisible to the samples as anything but a flat top. That is what
/// `probe_stats`'s rail flag exists to catch, and why it is checked before the
/// range arithmetic rather than after.
///
/// Full scale is the honest pair of numbers rather than something a few LSB
/// inside it -- a tighter bound would be guarding a difference smaller than the
/// uncertainty on measuring it.
///
/// Re-measure with `src/bin/oparails.rs`, which drives OPA1 through its own muxes
/// so the answer does not depend on what is wired to the input. It saturates each
/// end with the topology that can actually reach it (`Vout = G*Vdac` for the top,
/// this file's `Vout = G*Vin + (1-G)*Vdac` for the bottom, whose DAC term is what
/// makes a negative command possible) and prints the commanded output next to
/// every reading, so "the amplifier was pushed past what it can deliver" is
/// checked rather than assumed. Worth re-running after a supply change or on a
/// different board; nothing at run time depends on it.
pub const OUT_LO: i32 = 0;

pub const OUT_HI: i32 = ADC_MASK as i32;

/// Mid-supply, which is where `dac_code_for` pivots the amplified DC -- the point
/// furthest from both rails, so it is the placement that leaves the most swing.
pub const OUT_CENTER: i32 = OUT_HI / 2;

/// DAC12 full-scale code. The pivot solution below is not always reachable
/// -- see `dac_code_for` -- so this bound is part of the range arithmetic,
/// not just a defensive clamp.
pub const DAC_MAX: i32 = 4095;

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
pub const FILL_TARGET_PCT: i32 = 65;

/// Re-range thresholds, in percent of available headroom, applied to the
/// projected fill at the *current* range. Deliberately wider than
/// FILL_TARGET_PCT on both sides -- see `next_range`.
pub const RERANGE_HI_PCT: i32 = 75;

pub const RERANGE_LO_PCT: i32 = 25;

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
/// and both referenced to the same 2.5 V VREF, so their codes share one scale
/// and the reference cancels out of the expression entirely -- the placement
/// does not care what the reference actually is, only that one number serves
/// both converters. Move either one onto a different reference and every DAC
/// code this function returns is wrong by their ratio, silently.
pub fn dac_code_for(gain: Gain, mean_in: i32) -> u16 {
    let g = g_nominal(gain);
    ((g * mean_in - OUT_CENTER) / (g - 1)).clamp(0, DAC_MAX) as u16
}

/// Where this range's DC actually lands. For `Direct` that is the input's own
/// DC, untouched -- there is nothing in the path to move it. For the PGA it is
/// `OUT_CENTER`, except where `dac_code_for` had to clamp.
pub fn dc_for(range: Range, mean_in: i32) -> i32 {
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
pub const fn g_nominal(g: Gain) -> i32 {
    Range::Pga(g).nominal()
}

/// Usable half-swing on this range: the distance from its DC to the nearer end
/// of the window. Zero (or negative, reported as zero) means the DC is already
/// outside the window and no signal fits at all.
///
/// Only the DC placement still depends on the range -- the bounds no longer do.
pub fn headroom(range: Range, mean_in: i32) -> i32 {
    let dc = dc_for(range, mean_in);
    (dc - OUT_LO).min(OUT_HI - dc).max(0)
}

/// Does this range's signal fit inside `fill_pct` of the headroom it actually
/// has? Both constraints -- signal size and DC placement -- are enforced here,
/// so a range that cannot be centred is rejected for the right reason instead
/// of quietly clipping.
pub fn fits(range: Range, mean_in: i32, pp_in: u32, fill_pct: i32) -> bool {
    let half = range.nominal() * pp_in.max(1) as i32 / 2;
    half * 100 <= headroom(range, mean_in) * fill_pct
}

/// Largest range that fits, falling back to `Direct` when none does.
///
/// The fallback is not a guess: `Direct` is the least demanding range there
/// is (see `Range`), so if it does not fit, nothing would have. Reaching it
/// means the input's own excursion leaves `0..VDDA` -- an analog front-end
/// limit that no choice inside this chip can widen, which is why the caller
/// flags that case rather than the planner hiding it.
pub fn pick_range(mean_in: i32, pp_in: u32) -> Range {
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
pub fn over_range(range: Range, mean_in: i32, pp_in: u32) -> bool {
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
pub fn next_range(current: Range, mean_in: i32, pp_in: u32) -> Range {
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
/// `0..VDDA` is simply not visible: it reads as a flat top or bottom, which
/// makes pp too small and the mean wrong in the same direction. Acting on
/// those numbers would pick too high a gain and then clip the frame that
/// carries the marks. No gain setting can fix an input outside the supply --
/// that needs external conditioning -- so the only honest response is to
/// report it, which is what the caller does with this flag.
pub fn probe_stats(buf: &[u32]) -> (i32, u32, bool) {
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
pub fn setup_opa1_pga(gain: Gain, dac_code: u16) {
    // The bias reference must be up and stable before the OPA turns on, and
    // the DAC's own reference before that -- `vref::init`, which the caller
    // runs first because its result is a fact about the whole reading.
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

/// Turn the amplifier off and take its bias reference with it, consumer before
/// source: the ladder bottom is tied to DAC12OUT, so the OPA stops before the
/// voltage it is referred to goes away.
///
/// This is what actually unloads PA18 between measurements. The IOMUX has the
/// pin high-Z the whole time (`sampler::set_hiz`), but that says nothing about
/// the analog side: an enabled OPA1 with PSEL=EXTPIN1 connects the pin to the
/// amplifier's input through the P-MUX and draws input bias current there.
/// Clearing PWREN is what breaks that connection.
///
/// Clearing PWREN also resets the block, so `setup_opa1_pga` has to run again
/// before the next frame -- it writes every field it depends on, starting from
/// its own reset assert.
pub fn power_down_opa1() {
    let opa = pac::OPA1;
    opa.ctl().write(|w| w.set_enable(false));
    opa.gprcm().pwren().write(|w| {
        w.set_key(pac::opa::vals::PwrenKey::KEY);
        w.set_enable(false);
    });

    dac::power_down();
    crate::vref::power_down();
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
/// `Direct` deliberately leaves the OPA running for the rest of the cycle.
/// Powering it down here would save a little current -- 1(2) scores low-power
/// operation -- but it would owe an OPA enable wait if the measurement frame
/// or a later probe wanted the amplifier back, and `Direct` is only ever
/// selected when the input's DC makes amplification impossible, which is a
/// fault condition rather than a normal operating point. Not worth the latency
/// on a path that should never be hot. The idle current it would have saved is
/// taken by `power_down_opa1` at the end of the cycle regardless.
///
/// Returns whether the analog path actually changed, so the caller can skip
/// the settle delay when it did not.
pub fn apply_range(range: Range, mean_in: i32) -> bool {
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
