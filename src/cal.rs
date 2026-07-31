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
/// bench actually runs in -- the autoranger never leaves it, because the input
/// sits well below mid-rail and its swing already fills the headroom that DC
/// leaves. Every row below came off a frame reporting `x1`.
///
/// Reduced from a 91-frame run (`cal.csv`, 2026-07-31, reference SDM3055X-E):
/// readings within 1% of each other in current are one bench point and enter
/// as their median, and a row is kept only if its RMS is at least 6 LSB above
/// the last kept one. That floor is twice the single-reading standard
/// deviation measured in the run, and it is what stops the table from
/// interpolating between two numbers whose difference is noise.
///
/// Three things this table does not cover, all of them the run's limits rather
/// than the format's:
///
/// - Nothing below 0.59 A or above 2.79 A. The `< 0.2 A` and `> 2 A` alarm
///   regions of 2(3) are therefore reached by extending the end segments, not
///   by interpolation.
/// - 1.42 A to 1.75 A is one long segment. The front end plateaus there --
///   19% more current moved the RMS by less than the reading noise -- so the
///   run cannot resolve that band at all and the segment bridges it. Readings
///   landing inside it are worth about 10%, and no table can fix that; the
///   front end has to.
/// - Fit residual against the run's own 88 usable points is 0.11% median,
///   0.83% at the 90th percentile outside that plateau. In-sample, so it
///   bounds how well the table represents this run, not how well it will read
///   the next one.
///
/// One frame was discarded: 3.498 A reading 471.33 LSB, below the 2.79 A
/// cluster in both RMS and probe mean. Both columns must ascend, so it could
/// not be entered even if it were believed, and its shape is a mis-paired
/// sample taken while the load was moving rather than a saturating front end.
///
/// STALE: these rows were taken with ADC1 converting against VDDA. The
/// converter now runs on the 2.5 V VREF, so the same input produces a code
/// larger by roughly VDDA/2.5, and the front end was rebiased for that window
/// besides. Every number here is wrong until the bench is run again; it is kept
/// only as the record of the configuration it was measured in.
pub static CAL_X1: &[(f32, f32)] = &[
    (193.74, 0.5879),
    (200.27, 0.6044),
    (206.56, 0.6252),
    (216.92, 0.6493),
    (226.51, 0.6817),
    (234.84, 0.7110),
    (242.51, 0.7409),
    (251.36, 0.7762),
    (260.19, 0.8166),
    (269.10, 0.8731),
    (277.95, 0.9146),
    (284.57, 0.9597),
    (293.35, 1.0017),
    (304.34, 1.0632),
    (314.81, 1.1358),
    (322.48, 1.1782),
    (333.16, 1.2782),
    (339.47, 1.3126),
    (347.10, 1.3744),
    (356.18, 1.4494),
    (369.78, 1.7468),
    (379.03, 1.8276),
    (386.26, 1.9019),
    (393.48, 1.9550),
    (421.72, 2.1182),
    (458.96, 2.3075),
    (467.09, 2.5345),
    (500.50, 2.7848),
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
/// `lsb_to_amps` can use here). Worst case 38% across that run's span, which is
/// what a single straight line is worth against a curve whose local slope
/// moves by 2.5x. A fallback, not a calibration.
///
/// Written to the shortest decimal that round-trips through f32, so the source
/// does not imply a precision this constant does not have.
pub const CAL_FALLBACK_A_PER_LSB: f32 = 0.0034627696;

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
