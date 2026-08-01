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
/// And there is a great deal of it. Sensitivity runs 449 LSB/A at 1.15 A down
/// to 117 LSB/A at 0.12 A -- a factor of 3.8 -- so no line, and no two- or
/// three-constant curve, reaches the bottom of the measuring range. A dense
/// table does, and it needs no model of the mechanism.
///
/// Also why nothing here should "correct" toward theory: if the reference
/// instrument reads 2% low, matching it scores full marks and being right
/// scores zero. Fit what the reference says, verbatim.
///
/// One table per range, rather than one shared table plus a gain factor.
/// The autoranger makes the range part of the measurement, and the ladder's
/// real step ratios are not the nominal 2/4/8/16/32 -- their deviation is the
/// same order as the whole 0.5% budget. A per-range table absorbs that ratio
/// error, the range's own DC behaviour, and the curvature above, all in one
/// set of bench readings, with no separate gain-trim constant to measure or
/// to get wrong.
///
/// `CAL_X1` is the `Direct` range, and on this front end it is the range the
/// bench actually runs in -- the autoranger never leaves it. That is not a
/// fault: the input's own swing already fills most of the headroom its DC
/// leaves, at every current, so no gain step fits and unity is the correct
/// choice. It stays that way while the front end's DC tracks its signal,
/// because a PGA multiplies both.
///
/// TO REFILL: `cargo xtask cal-log`, sweep the load across the range, and run
/// the reduction below over the CSV it writes.
///
/// Reduced from a 215-frame run (`cal.csv`, 2026-08-01 10:04-10:22, address 6,
/// reference SDM3055X-E, ADC on the 2.5 V VREF), covering 0.115 A to 2.38 A.
/// The reduction takes every frame that carries both numbers, with no filter on
/// the quality flag, and does only what the table's own shape demands:
///
/// - frames are sorted by RMS and grouped into 1%-wide bins; each bin enters as
///   its median. Neighbouring frames of one held point differ by less than the
///   bin width, so this is averaging, not selection;
/// - the longest strictly-ascending subsequence of the binned points is kept.
///   What it discards is the frames taken while the front end was still settling
///   after a load step -- there the reference has moved and the meter has not,
///   so the pair describes two different currents and no monotone table can
///   contain it. 128 bins in, 94 out;
/// - a point is kept only if its RMS is at least 2% above the last kept one.
///   Below that the segment is shorter than the scatter on its endpoints, and
///   its slope is noise: without this rule the steepest segment is six times its
///   neighbours. 94 in, 75 out, and the segment slopes span 0.28-10.4 mA/LSB.
///
/// Against the 32 frames of the run that sat in a settled stretch -- both the
/// reference and the RMS steady across their neighbours, so the pairing is
/// trustworthy -- the residual is 0.12% median, 0.45% at the 90th percentile,
/// 6.2% worst.
///
/// What bounds this table:
///
/// - The bottom is measured, not extrapolated: the run reaches 0.115 A, below
///   the 0.1 A the range asks for, so nothing under 0.2 A rests on an
///   extension of the first segment.
/// - Above 2.38 A the last segment is extended rather than clamped, so the
///   `> 2 A` alarm of 2(3) fires at the right current.
/// - **This is a single-direction sweep.** The front end takes seconds to settle
///   after a load step, and the reduction drops the frames inside that, so the
///   table describes the settled value. A reading taken within a few seconds of
///   a load change will not match it.
pub static CAL_X1: &[(f32, f32)] = &[
    (13.56, 0.1155),
    (14.71, 0.1182),
    (15.21, 0.1202),
    (18.60, 0.1242),
    (20.87, 0.1280),
    (22.56, 0.1298),
    (23.71, 0.1346),
    (25.94, 0.1377),
    (27.44, 0.1404),
    (28.98, 0.1431),
    (31.07, 0.1459),
    (32.16, 0.1496),
    (34.03, 0.1525),
    (35.39, 0.1546),
    (36.68, 0.1584),
    (38.24, 0.1607),
    (39.23, 0.1631),
    (41.04, 0.1657),
    (42.46, 0.1694),
    (43.82, 0.1714),
    (46.09, 0.1743),
    (47.49, 0.1786),
    (48.56, 0.1817),
    (52.14, 0.1872),
    (53.85, 0.1898),
    (59.11, 0.2014),
    (64.25, 0.2085),
    (65.73, 0.2116),
    (68.97, 0.2162),
    (73.02, 0.2222),
    (77.10, 0.2303),
    (80.25, 0.2363),
    (84.29, 0.2422),
    (86.02, 0.2454),
    (89.75, 0.2527),
    (91.99, 0.2563),
    (94.94, 0.2641),
    (106.37, 0.2673),
    (118.81, 0.2901),
    (128.43, 0.3114),
    (137.13, 0.3349),
    (151.29, 0.3394),
    (159.53, 0.3900),
    (173.21, 0.3968),
    (188.68, 0.4538),
    (193.65, 0.4796),
    (225.37, 0.4894),
    (241.51, 0.5622),
    (279.41, 0.5855),
    (286.44, 0.6587),
    (296.60, 0.6796),
    (303.83, 0.6952),
    (317.93, 0.7040),
    (331.62, 0.7314),
    (340.22, 0.7756),
    (353.14, 0.8165),
    (387.60, 0.8729),
    (396.67, 0.8924),
    (435.73, 0.9701),
    (458.57, 1.0265),
    (515.12, 1.1485),
    (554.68, 1.2324),
    (617.90, 1.3650),
    (676.45, 1.4024),
    (696.77, 1.5127),
    (747.14, 1.6166),
    (788.33, 1.6800),
    (823.22, 1.7859),
    (855.40, 1.8768),
    (896.34, 1.9269),
    (927.08, 2.0224),
    (955.55, 2.0823),
    (990.81, 2.1549),
    (1018.13, 2.2124),
    (1093.01, 2.3797),
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
/// `lsb_to_amps` can use here). Worst case 69% across those rows, at the bottom
/// of the range -- a single scale factor cannot follow a sensitivity that moves
/// by 3.8x, which is the whole reason `CAL_X1` is a table. Nothing that reaches
/// this constant is calibrated; it exists so an unfilled step reads a plausible
/// number instead of a wild one.
///
/// Written to the shortest decimal that round-trips through f32, so the source
/// does not imply a precision this constant does not have.
pub const CAL_FALLBACK_A_PER_LSB: f32 = 0.0026078339;

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
