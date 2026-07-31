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
/// the range changes. The step in use at 2 A wants a point past 2 A as well,
/// so the over-limit alarm region is interpolated rather than extrapolated;
/// `CAL_X1` does not have one, and says below what that costs.
///
/// `CAL_X1` is the `Direct` range, and on this front end it is the range the
/// bench actually runs in -- the autoranger never leaves it. That is not a
/// fault: the input's own swing already fills 76-89% of the headroom its DC
/// leaves, at every current, so no gain step fits and unity is the correct
/// choice. It stays that way while the front end's DC tracks its signal,
/// because a PGA multiplies both.
///
/// Reduced from an 84-frame run (`cal.csv`, 2026-08-01 05:12-05:15, address 6,
/// reference SDM3055X-E, ADC on the 2.5 V VREF). The reduction walks the run in
/// time order rather than sorting it, because what makes a bench point is that
/// the load was *held*, and only the time axis knows that:
///
/// - consecutive frames whose reference current is within 1% of the running
///   median are one bench point, and enter as the median of that run;
/// - a group of fewer than three frames is discarded. Those are the frames
///   caught while the load was being turned, where the meter's 260 ms window
///   and the reference's are looking at different currents. Their apparent
///   A/LSB scatters over 1.72e-3 to 2.94e-3 against 2.08-2.14e-3 for every
///   settled group, so they are not weak points, they are wrong ones;
/// - a group whose own RMS spans more than 5% is discarded for the same reason
///   -- it is not one bench point however many frames it has;
/// - a row is kept only if its RMS is at least 3% above the last kept one.
///
/// Twelve settled points survive out of twenty-three groups. Eleven remain
/// after the one above 2 A, and nine after the 3% step.
///
/// What bounds this table is not the table:
///
/// - Nothing below 0.55 A or above 1.54 A. `< 0.2 A` is reached by extending
///   the first segment, and everything past the last row is extrapolated along
///   the final segment. That extrapolation was checked against the two frames
///   this run did take above 2 A: it lands within 0.4% at 2.13 A and 4.8% low
///   at 2.37 A. The `> 2 A` alarm of 2(3) therefore still fires at the right
///   current, but the number it fires with is understated at the top.
/// - **This is an ascending-ramp table.** Two settled points fall on the way
///   back down, at 0.70 A and 0.87 A, and both sit about 5% below the
///   ascending curve at the same reference current -- they agree with each
///   other, so it is a direction offset and not scatter. Ramped down, this
///   table therefore reads about 5% high. Two points cannot say whether that
///   is core remanence in the CT, which would be the ordinary explanation and
///   would make it a permanent property of the instrument, so nothing here
///   corrects for it. Measuring it is a bench run of its own: sweep down as
///   slowly as up, holding every point.
/// - Residual against the settled points it was built from is zero by
///   construction; the honest scatter figure is the 1.08% median relative
///   residual of a straight line through the same points, which is what the
///   piecewise segments are absorbing.
///
/// Hold each bench point for five seconds or more when re-running this: at the
/// 1.4 s the request-response loop takes per reading, that is the three frames
/// a group needs to survive the filter above.
pub static CAL_X1: &[(f32, f32)] = &[
    (262.26, 0.5501),
    (289.65, 0.6037),
    (312.01, 0.6511),
    (337.66, 0.7077),
    (370.96, 0.7777),
    (419.19, 0.8793),
    (483.46, 1.0167),
    (576.36, 1.2173),
    (719.86, 1.5373),
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
/// Fitted to the same rows as `CAL_X1`, by least squares on *relative* error (a
/// reading is judged as a percentage) and through the origin (the only shape
/// `lsb_to_amps` can use here). Worst case 1.63% across those rows -- the
/// rebiased front end is nearly linear, so one straight line goes a long way
/// here. Still a fallback and not a calibration: it says nothing about the
/// five PGA steps it actually serves, which it reaches by the nominal ladder
/// ratio from a run that never left x1.
///
/// Written to the shortest decimal that round-trips through f32, so the source
/// does not imply a precision this constant does not have.
pub const CAL_FALLBACK_A_PER_LSB: f32 = 0.0021008217;

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
