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
/// `CAL_X1` is the `Direct` range. It only gets selected when the input's own
/// DC leaves no room to amplify, so on a front end that biases near mid-rail
/// it will never be entered and can stay empty; fill it only if the bench
/// actually shows `x1` on the OLED.
pub static CAL_X1: &[(f32, f32)] = &[];

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
/// Fitted at one point, at x4; other steps are extrapolated by the nominal
/// ratio, which is precisely the approximation the tables exist to replace.
pub const CAL_FALLBACK_A_PER_LSB: f32 = 0.003433733774564919;

pub const CAL_FALLBACK_GAIN: f32 = 4.0;

/// Both columns of a CAL table must increase together. Checked at compile
/// time because the tables are typed in by hand, one pair per bench reading,
/// and a transposed or out-of-order pair does not fail loudly at runtime --
/// it just interpolates on the wrong segment and reads plausibly wrong.
/// NaN also fails this (every comparison against NaN is false), so a
/// mistyped literal cannot slip through either.
pub const fn cal_strictly_ascending(cal: &[(f32, f32)]) -> bool {
    let mut i = 1;
    while i < cal.len() {
        if !(cal[i].0 > cal[i - 1].0) || !(cal[i].1 > cal[i - 1].1) {
            return false;
        }
        i += 1;
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
