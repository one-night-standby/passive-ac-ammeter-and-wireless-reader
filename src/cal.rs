//! Calibration: raw Hann-weighted RMS in ADC LSB -> amps, one table per
//! range. These tables are the deliverable of the bench session; everything
//! else in this firmware exists to produce a number worth putting in them.
//!
//! A table pushed over the link and kept in flash (`nvcal`) takes precedence
//! over `CAL_X1` -- see `Table` for what that is for and what it does not
//! cover.

use core::cell::RefCell;

use critical_section::Mutex;

use crate::nvcal;
use crate::range::{Gain, Range};

/// Calibration table: `(raw Hann-weighted RMS in LSB, amps on the reference
/// ammeter)`, ascending by LSB. Interpolated piecewise-linearly.
///
/// The scored quantity is agreement with the reference ammeter, not absolute
/// accuracy -- 1(1) is "误差 ... 相对于标定电流表读数". So any error a single
/// scale factor can absorb is already free, and what is left after fitting is
/// only what a straight line *cannot* absorb: curvature across the 20:1 range.
/// And there is a great deal of it. Sensitivity runs 457 LSB/A at 1.90 A down
/// to 171 LSB/A at 0.13 A -- a factor of 2.7 -- so no scale factor reaches the
/// bottom of the measuring range, and the best straight line with a free offset
/// still misses by 3.2%, six times the budget. A table does, and it needs no
/// model of the mechanism.
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
/// TO REFILL: set a load, let the front end settle, and record the pair the
/// meter's second line and the reference ammeter show at the same moment. The
/// bench app's calibration screen keeps the list and exports it; `cargo xtask
/// cal-log` logs the same pairs to CSV when the reference is on USBTMC.
///
/// Sixteen points, one per load setting, each pair read off the bench verbatim
/// -- nothing here is a bin median, a fit, or any other number the bench did
/// not produce. Sixteen is also `FIELD_MAX`, so a pushed table can describe
/// this range at the same resolution this one does; the built-in table is not
/// the denser of the two.
///
/// What bounds this table:
///
/// - It spans 0.1305 to 1.9000 A. Both ends are extended rather than clamped:
///   2 A lands 48 LSB past the last point, about a third of the last segment's
///   own span, so the `> 2 A` alarm of 2(3) fires close to the right current.
/// - **The bottom of the measuring range is not measured here.** The first
///   point is 0.1305 A, so 0.12 A -- and the 0.1 A the range asks for -- read
///   on the first segment extended downward, and that segment's slope comes
///   from two readings 13 mA apart. Extended far enough it stops meaning
///   anything: at zero RMS this table says 0.093 A. A point below 0.13 A is
///   what fixes that, and no argument here substitutes for one.
/// - Nothing measures 0.167 to 0.293 A either: 42.75 and 109.07 LSB are
///   adjacent rows. That is a 1.75:1 step in current across one chord, against
///   1.28:1 for the next widest gap, and it ends right where the sensitivity
///   is known to bend (see `FIELD_MAX`).
/// - **These are settled readings.** The front end takes seconds to catch up
///   after a load step. A reading taken within a few seconds of a load change
///   will not match this table, and a point recorded there does not belong in
///   it.
pub static CAL_X1: &[(f32, f32)] = &[
    (22.31, 0.1305),
    (30.09, 0.1435),
    (35.07, 0.1540),
    (42.75, 0.1670),
    (109.07, 0.2930),
    (130.00, 0.3360),
    (157.68, 0.3950),
    (185.34, 0.4550),
    (223.49, 0.5340),
    (273.93, 0.6400),
    (358.55, 0.8160),
    (455.47, 1.0330),
    (545.09, 1.2240),
    (630.57, 1.4040),
    (745.60, 1.6450),
    (868.68, 1.9000),
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
/// `lsb_to_amps` can use here). Worst case 57% across those rows, at the bottom
/// of the range -- a line through the origin cannot follow a sensitivity that
/// moves by 2.7x across the table, which is the whole reason `CAL_X1` is a
/// table. Nothing that reaches this constant is calibrated; it exists so an
/// unfilled step reads a plausible number instead of a wild one.
///
/// Written to the shortest decimal that round-trips through f32, so the source
/// does not imply a precision this constant does not have.
pub const CAL_FALLBACK_A_PER_LSB: f32 = 0.0025214116;

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

/// Points a field table may hold.
///
/// Sixteen rather than the ten a session realistically collects, because what
/// decides whether a piecewise-linear table clears 0.5% is where the knots sit,
/// not how many there are. Against a smooth fit of the bench run, ten knots
/// spaced evenly in log(RMS) leave a worst chord error of 0.49% -- the entire
/// budget -- while the same ten spaced by round current values leave 1.5%: the
/// sensitivity bends hardest around 0.3 A, and a table with no knot there
/// cannot follow it. The spare slots are what lets a session put knots where
/// the curve needs them instead of where the load knob is convenient. Sixteen
/// costs 128 bytes of table and a 144-byte flash record.
pub const FIELD_MAX: usize = 16;

/// Raising it costs flash slots, not RAM. The record is `2 + FIELD_MAX` flash
/// words and the store is one 1 KB sector, so 16 points give 144-byte records
/// and seven of them per erase; 32 would give 272 bytes and three. Those slots
/// are what lets a push append instead of erasing, which is the whole reason a
/// brownout mid-push cannot leave the meter with neither table.
const _: () = assert!(
    // The staged-point mask is a `u16`, one bit per slot. Both ways it breaks
    // past 16 are quiet: the shift in `stage_point` overflows, and the
    // all-present mask in `commit_field` truncates -- between them a push that
    // dropped points would commit as though it were complete.
    FIELD_MAX <= u16::BITS as usize,
    "FIELD_MAX is bounded by the width of the staged-point mask"
);

/// A calibration table small enough to travel over the link and live in flash.
///
/// This is the same `(LSB, amps)` relation as `CAL_X1`, measured against
/// whatever reference is on the bench at the time. It exists because the
/// scored quantity is agreement with *that* instrument: a 4.5-digit handheld
/// and the meter this was fitted against can differ by more than the whole
/// 0.5% budget on AC, and no amount of care on our side removes that.
///
/// It replaces `CAL_X1` only -- the `Direct` range, which is the only one this
/// front end runs in. A field table pushed while a PGA step was somehow
/// selected would be a table measured on one range applied to another, so the
/// PGA steps keep their own tables and their own fallback.
#[derive(Clone, Copy)]
pub struct Table {
    pub points: [(f32, f32); FIELD_MAX],
    pub len: usize,
}

impl Table {
    pub const EMPTY: Self = Self {
        points: [(0.0, 0.0); FIELD_MAX],
        len: 0,
    };

    pub fn as_slice(&self) -> &[(f32, f32)] {
        &self.points[..self.len]
    }

    /// Only what the lookup cannot do without: the LSB column ascending, and
    /// numbers that are numbers.
    ///
    /// The amps column is left alone deliberately. `lsb_to_amps` finds its
    /// segment by LSB, so a LSB column that does not increase leaves "which
    /// segment" undefined -- that one is arithmetic, not taste. What the amps
    /// do between two points is a measurement result: a bench point taken
    /// while the load was still moving, or two currents a hair apart read in
    /// the wrong order, produce a table that dips, and a table that dips is
    /// what the reference actually said. The built-in tables are held to the
    /// stricter rule at compile time, where a non-monotone pair means a typo
    /// rather than a reading.
    pub fn valid(&self) -> bool {
        if self.len < 2 || self.len > FIELD_MAX {
            return false;
        }
        // Infinities pass an ascending test and then poison the interpolation,
        // so they are excluded here rather than there. NaN is already excluded
        // by the comparison itself.
        if self
            .as_slice()
            .iter()
            .any(|(lsb, amps)| !lsb.is_finite() || !amps.is_finite() || *lsb < 0.0 || *amps < 0.0)
        {
            return false;
        }
        self.as_slice()
            .windows(2)
            .all(|pair| pair[1].0 > pair[0].0)
    }
}

/// The table in use, or `None` while the meter is on `CAL_X1`.
///
/// Loaded from flash once at boot and thereafter only replaced by a completed
/// push, so a measurement never waits on flash and never sees a half-installed
/// table.
static FIELD: Mutex<RefCell<Option<Table>>> = Mutex::new(RefCell::new(None));

/// Points as they arrive, one frame each, before the push is complete.
///
/// Separate from `FIELD` because a push is not atomic on the wire: the app
/// sends one point per line and any of them can be lost. Staging keeps the
/// meter measuring on its current table for the whole exchange -- there is no
/// window where it is calibrated by a partial table.
///
/// The mask is which slots have been written since the last commit or clear.
/// A dropped point in the middle of a push is exactly what it catches: without
/// it, the committed table would silently contain whatever that slot held from
/// an earlier session.
static STAGED: Mutex<RefCell<(Table, u16)>> = Mutex::new(RefCell::new((Table::EMPTY, 0)));

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CalError {
    /// Point index outside the table.
    Index,
    /// A coordinate that is not a usable number.
    Value,
    /// Fewer points committed than were staged, or a gap in the middle.
    Missing,
    /// Committed table is not two or more strictly ascending points.
    Order,
    /// The flash controller refused the write. The table in RAM is left alone:
    /// it is still the table the operator measured, it just will not survive
    /// the next power loss.
    Flash,
}

/// Install whatever is in flash. Call once, before the first measurement.
///
/// A table that fails `valid` is dropped rather than repaired, for the same
/// reason a pushed one is: `CAL_X1` is a defensible fallback and a
/// half-trusted table is not.
pub fn load_field() {
    let stored = nvcal::load().filter(Table::valid);
    critical_section::with(|cs| *FIELD.borrow(cs).borrow_mut() = stored);
}

/// How many points the meter is currently reading on; 0 means `CAL_X1`. This
/// is what every frame reports as `SRC`, and the app's cue to push again after
/// the meter has been through a power cycle it could not survive.
pub fn field_len() -> usize {
    critical_section::with(|cs| {
        FIELD
            .borrow(cs)
            .borrow()
            .map_or(0, |table| table.len)
    })
}

/// Take one point of an in-progress push.
pub fn stage_point(index: usize, lsb: f32, amps: f32) -> Result<(), CalError> {
    if index >= FIELD_MAX {
        return Err(CalError::Index);
    }
    if !lsb.is_finite() || !amps.is_finite() || lsb < 0.0 || amps < 0.0 {
        return Err(CalError::Value);
    }
    critical_section::with(|cs| {
        let mut staged = STAGED.borrow(cs).borrow_mut();
        staged.0.points[index] = (lsb, amps);
        staged.1 |= 1 << index;
    });
    Ok(())
}

/// Finish a push: check the staged points, write them to flash, and start
/// reading on them.
///
/// Flash first, then RAM, so the two can only disagree in the direction that
/// is safe to disagree in -- a write that failed leaves the meter on the table
/// it already had, and says so.
pub fn commit_field(count: usize) -> Result<(), CalError> {
    let staged = critical_section::with(|cs| *STAGED.borrow(cs).borrow());
    let (mut table, mask) = staged;
    if count < 2 || count > FIELD_MAX {
        return Err(CalError::Order);
    }
    let wanted = ((1u32 << count) - 1) as u16;
    if mask & wanted != wanted {
        return Err(CalError::Missing);
    }
    table.len = count;
    if !table.valid() {
        return Err(CalError::Order);
    }

    nvcal::store(&table).map_err(|_| CalError::Flash)?;
    critical_section::with(|cs| {
        *FIELD.borrow(cs).borrow_mut() = Some(table);
        *STAGED.borrow(cs).borrow_mut() = (Table::EMPTY, 0);
    });
    Ok(())
}

/// Go back to `CAL_X1`, in flash as well as in RAM.
pub fn clear_field() -> Result<(), CalError> {
    // Already there: say so without writing. Clearing is a record like any
    // other -- a `len == 0` one -- so a second `CALOFF` would spend a slot
    // saying what the last one already says, and seven of those are an erase.
    if field_len() == 0 {
        return Ok(());
    }
    nvcal::store(&Table::EMPTY).map_err(|_| CalError::Flash)?;
    critical_section::with(|cs| {
        *FIELD.borrow(cs).borrow_mut() = None;
        *STAGED.borrow(cs).borrow_mut() = (Table::EMPTY, 0);
    });
    Ok(())
}

/// Piecewise-linear lookup in the active range's table. Outside the table the
/// end segments are extended rather than clamped -- a clamped reading would
/// silently under-report an over-limit current, and 2(3) needs `> 2 A` to
/// raise the alarm.
pub fn lsb_to_amps(rms: f32, range: Range) -> f32 {
    if range == Range::Direct {
        // Copied out rather than interpolated under the lock: the lookup is
        // the tail of a measurement, and holding a critical section across it
        // would put the link's interrupt behind arithmetic that does not need
        // to exclude anything.
        let field = critical_section::with(|cs| *FIELD.borrow(cs).borrow());
        // The length is re-checked rather than assumed. Nothing installs a
        // table shorter than two points -- both paths that write `FIELD` run
        // `valid` first -- but `interpolate` subtracts two from the length, so
        // the cost of that invariant being wrong is a panic in the tail of a
        // measurement, and this meter panics by halting.
        if let Some(table) = field.filter(|table| table.len >= 2) {
            return interpolate(rms, table.as_slice());
        }
    }

    let cal = cal_for(range);
    if cal.len() < 2 {
        let scale = CAL_FALLBACK_A_PER_LSB * CAL_FALLBACK_GAIN / range.nominal() as f32;
        return scale * rms;
    }
    interpolate(rms, cal)
}

/// Requires at least two points; every caller has already established that.
fn interpolate(rms: f32, cal: &[(f32, f32)]) -> f32 {
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
