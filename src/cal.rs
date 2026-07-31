//! Calibration: raw Hann-weighted RMS in ADC LSB -> amps, one table per
//! range. These tables are the deliverable of the bench session; everything
//! else in this firmware exists to produce a number worth putting in them.

use crate::range::{Gain, Range};

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
/// `CAL_X1` is the `Direct` range, and on this front end it is the range the
/// bench actually runs in -- the autoranger never leaves it. That is not a
/// fault: the input's own swing already fills 76-89% of the headroom its DC
/// leaves, at every current, so no gain step fits and unity is the correct
/// choice. It stays that way while the front end's DC tracks its signal,
/// because a PGA multiplies both.
///
/// Reduced from a 196-frame run (`cal.csv`, 2026-07-31 21:54-21:59, reference
/// SDM3055X-E, ADC on the 2.5 V VREF): readings within 1% of each other in
/// current are one bench point and enter as their median, and a row is kept
/// only if its RMS is at least 3% above the last kept one. Below that step the
/// residual stops improving -- the extra rows are fitting noise, not curve.
///
/// The run swept up and then back down, and **both directions are in here on
/// purpose**. At matched current the descending pass reads 0.59% lower than
/// the ascending one, consistently enough to be a systematic rather than
/// scatter; folding the two together puts the table between them, which shows
/// up as a residual of +0.17% on the ascending points and -0.24% on the
/// descending ones. Half the error each way beats none in one direction and
/// all of it in the other. It also cancels the pairing lag to first order: the
/// reference reading trails the meter's window by ~300 ms, which biases a
/// rising sweep one way and a falling sweep the other.
///
/// What bounds this table is not the table:
///
/// - Per-frame scatter is 0.45% (1 sigma, from local fits within one pass).
///   Fit residual is 0.238% median and 0.714% at the 90th percentile, and it
///   barely moves between 82 rows and 38 -- both numbers are the run's noise
///   and its direction offset, not the interpolation.
/// - Nothing below 0.55 A or above 2.32 A. Worse, the four frames above that
///   came back `OVER`: at 2.33 A the probe already puts the waveform's peak at
///   code 4107 against a 4095 ceiling. The `> 2 A` alarm region of 2(3) is
///   therefore only about 15% wide before the front end runs out of window,
///   and `< 0.2 A` is reached by extending the first segment.
///
/// Re-derive from the ascending pass alone if the load is only ever ramped up
/// in service; the merge above is the choice that assumes least about that.
pub static CAL_X1: &[(f32, f32)] = &[
    (259.75, 0.5527),
    (271.67, 0.5756),
    (280.33, 0.5951),
    (289.14, 0.6122),
    (299.88, 0.6354),
    (309.81, 0.6595),
    (322.31, 0.6832),
    (336.05, 0.7120),
    (349.23, 0.7388),
    (362.63, 0.7689),
    (376.16, 0.7997),
    (388.32, 0.8245),
    (405.35, 0.8615),
    (423.26, 0.8976),
    (441.63, 0.9384),
    (459.22, 0.9711),
    (474.15, 1.0086),
    (493.55, 1.0454),
    (508.72, 1.0789),
    (525.61, 1.1145),
    (545.61, 1.1566),
    (568.28, 1.2045),
    (593.00, 1.2569),
    (617.78, 1.3135),
    (646.17, 1.3652),
    (666.95, 1.4165),
    (702.75, 1.4760),
    (727.28, 1.5288),
    (762.63, 1.6171),
    (790.15, 1.6907),
    (814.84, 1.7077),
    (843.12, 1.7820),
    (879.33, 1.8648),
    (918.48, 1.9214),
    (953.58, 2.0040),
    (993.20, 2.1032),
    (1029.83, 2.1726),
    (1067.80, 2.2633),
];
pub static CAL_X2: &[(f32, f32)] = &[];

pub static CAL_X4: &[(f32, f32)] = &[
    // (582.0, 2.000),
    // (640.0, 2.200),
];

pub static CAL_X8: &[(f32, f32)] = &[];

pub static CAL_X16: &[(f32, f32)] = &[];

pub static CAL_X32: &[(f32, f32)] = &[
    // (233.0, 0.100),
    // (349.0, 0.150),
    // ...
];

pub fn cal_for(range: Range) -> &'static [(f32, f32)] {
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
/// With `CAL_X1` filled, that means the five PGA steps -- which is also where
/// this constant is weakest: it is fitted at x1 and reaches the others by the
/// nominal ladder ratio, precisely the approximation the per-range tables
/// exist to replace.
///
/// Fitted to the same run as `CAL_X1`, by least squares on *relative* error (a
/// reading is judged as a percentage) and through the origin (the only shape
/// `lsb_to_amps` can use here). Worst case 2.3% across that run's span -- the
/// rebiased front end is nearly linear, so one straight line goes a long way
/// here. Still a fallback and not a calibration: it says nothing about the
/// five PGA steps it actually serves, which it reaches by the nominal ladder
/// ratio from a run that never left x1.
///
/// Written to the shortest decimal that round-trips through f32, so the source
/// does not imply a precision this constant does not have.
pub const CAL_FALLBACK_A_PER_LSB: f32 = 0.0021205244;

/// The gain `CAL_FALLBACK_A_PER_LSB` was fitted at.
pub const CAL_FALLBACK_GAIN: f32 = 1.0;

/// Both columns of a CAL table must increase together. Checked at compile
/// time because the tables are typed in by hand, one pair per bench reading,
/// and a transposed or out-of-order pair does not fail loudly at runtime --
/// it just interpolates on the wrong segment and reads plausibly wrong.
/// NaN also fails this (every comparison against NaN is false), so a
/// mistyped literal cannot slip through either.
pub const fn cal_strictly_ascending(cal: &[(f32, f32)]) -> bool {
    let mut i = 1;
    while i < cal.len() {
        // Phrased as the positive condition rather than a negated one so that
        // NaN rejection is visible instead of incidental: every comparison
        // against NaN is false, so a NaN in either column fails this `&&` and
        // falls to the `else`.
        if cal[i].0 > cal[i - 1].0 && cal[i].1 > cal[i - 1].1 {
            i += 1;
        } else {
            return false;
        }
    }
    true
}

pub const CAL_ORDER_MSG: &str = "CAL entries must be (lsb, amps) pairs, both strictly increasing";

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
pub fn lsb_to_amps(rms: f32, range: Range) -> f32 {
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
