//! Everything that turns one captured frame into numbers: the window and
//! quadrature tables, true RMS, the mains-frequency estimate, and the
//! frame-to-frame spread indicator. Pure arithmetic over a sample buffer --
//! no registers, no knowledge of which range produced the samples.

use libm::{atan2f, sqrtf};

use crate::sampler::{ADC_MASK, N, SAMPLE_HZ};

const TWO_PI: f32 = 2.0 * core::f32::consts::PI;
const PI: f32 = core::f32::consts::PI;

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

const _: () = assert!(
    SAMPLE_HZ.is_multiple_of(MAINS_NOM_HZ),
    "SPP_NOM must be exact"
);

const _: () = assert!(N.is_multiple_of(2));

// The whole phase-difference trick rests on the *nominal* fundamental
// advancing an exact multiple of 2*pi across HALF samples (400 * 2pi/80 =
// 10pi). If HALF stopped being a whole number of nominal cycles, the wrapped
// phase difference would carry a constant bias instead of reading out pure
// frequency deviation.
const _: () = assert!(HALF.is_multiple_of(SPP_NOM));

/// Cosine for const evaluation, argument in radians.
///
/// Exists because `libm::cosf` is not a `const fn`, and the tables below are
/// worth having as compile-time constants: see the note on `HANN`.
///
/// Quadrant-reduced to [0, pi/2] and then a Taylor series to x^14. At the
/// reduced argument's worst case (pi/2) the first dropped term is ~1.6e-9,
/// well under an f32 ULP, and the whole table checks out at 1.4e-7 max
/// absolute error against a f64 reference -- about 1.2 ULP at magnitude 1.
/// A window function needs nothing like that precision (the Hann sidelobe
/// argument in `rms_lsb` would survive 1e-5), so this has margin to spare.
const fn cos_const(x: f32) -> f32 {
    let mut t = x;
    while t < 0.0 {
        t += TWO_PI;
    }
    while t >= TWO_PI {
        t -= TWO_PI;
    }
    // cos(2pi - t) = cos(t), so fold onto [0, pi].
    if t > PI {
        t = TWO_PI - t;
    }
    // cos(pi - t) = -cos(t), so fold onto [0, pi/2] and carry the sign.
    let sign = if t > PI / 2.0 {
        t = PI - t;
        -1.0
    } else {
        1.0
    };

    let x2 = t * t;
    let mut term = 1.0f32;
    let mut sum = 1.0f32;
    let mut k = 1usize;
    while k <= 7 {
        term = -term * x2 / ((2 * k - 1) * (2 * k)) as f32;
        sum += term;
        k += 1;
    }
    sign * sum
}

/// Sine for const evaluation. Same reduction shape as `cos_const`, with its
/// own series rather than `cos(pi/2 - x)` -- the subtraction would move the
/// rounding error into the argument, where it is worth the most for small x.
#[allow(dead_code)] // Retained for the optional bench frequency estimator.
const fn sin_const(x: f32) -> f32 {
    let mut t = x;
    while t < 0.0 {
        t += TWO_PI;
    }
    while t >= TWO_PI {
        t -= TWO_PI;
    }
    // sin(t + pi) = -sin(t), then sin(pi - t) = sin(t).
    let mut sign = 1.0f32;
    if t > PI {
        t -= PI;
        sign = -1.0;
    }
    if t > PI / 2.0 {
        t = PI - t;
    }

    let x2 = t * t;
    let mut term = t;
    let mut sum = t;
    let mut k = 1usize;
    while k <= 7 {
        term = -term * x2 / ((2 * k) * (2 * k + 1)) as f32;
        sum += term;
        k += 1;
    }
    sign * sum
}

/// Periodic (DFT-even) Hann, `w[n] = 0.5*(1 - cos(2*pi*n/N))`. The periodic
/// form is the one that makes `sum(w)` come out to exactly N/2 and puts the
/// window's nulls on the analysis bins, which is the whole point here.
///
/// Computed at compile time, so it costs flash instead of SRAM and nothing
/// at startup. It used to be built by 401 runtime `cosf` calls (~19 ms on a
/// soft-float M0+) into a `static mut`; startup time is a scored item, and
/// that was the single largest avoidable chunk of it outside the HC-42's own
/// 500 ms settle. Being a plain `static` also retires the last `static mut`
/// in this module along with the `unsafe` and the `Tables` handle that
/// existed only to contain it.
///
/// Still mirrored about n = N/2. At const-eval time that is no longer about
/// speed: it makes `HANN[n] == HANN[N - n]` hold bit-for-bit, so the window
/// is *exactly* symmetric and cannot bias the phase estimate in `half_phase`.
static HANN: [f32; N] = {
    let mut w = [0.0f32; N];
    let mut n = 0;
    while n <= N / 2 {
        let v = 0.5 * (1.0 - cos_const(TWO_PI * n as f32 / N as f32));
        w[n] = v;
        if n > 0 && n < N / 2 {
            w[N - n] = v;
        }
        n += 1;
    }
    w
};

/// One nominal-frequency cycle of quadrature, for the `estimate_hz`
/// correlator. Same story as `HANN`: compile-time, flash-resident.
#[allow(dead_code)] // Retained for the optional bench frequency estimator.
static COS_TAB: [f32; SPP_NOM] = {
    let mut t = [0.0f32; SPP_NOM];
    let mut n = 0;
    while n < SPP_NOM {
        t[n] = cos_const(TWO_PI * n as f32 / SPP_NOM as f32);
        n += 1;
    }
    t
};

#[allow(dead_code)] // Retained for the optional bench frequency estimator.
static SIN_TAB: [f32; SPP_NOM] = {
    let mut t = [0.0f32; SPP_NOM];
    let mut n = 0;
    while n < SPP_NOM {
        t[n] = sin_const(TWO_PI * n as f32 / SPP_NOM as f32);
        n += 1;
    }
    t
};

/// How many recent frames the spread indicator remembers.
#[allow(dead_code)] // Retained for bench noise characterization.
pub const SPREAD_N: usize = 16;

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
#[allow(dead_code)] // Retained for bench noise characterization.
pub struct Spread {
    hist: [f32; SPREAD_N],
    idx: usize,
    len: usize,
}

#[allow(dead_code)] // Retained for bench noise characterization.
impl Spread {
    pub const fn new() -> Self {
        Self {
            hist: [0.0; SPREAD_N],
            idx: 0,
            len: 0,
        }
    }

    /// Records one frame and returns the current peak-to-peak spread.
    pub fn push(&mut self, x: f32) -> f32 {
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
pub fn coarse_pivot(buf: &[u32; N]) -> i32 {
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
pub fn rms_lsb(buf: &[u32; N], pivot: i32) -> f32 {
    // Pass 1: weighted mean of the pivot-centred residual. The mean has to
    // be weighted by the *same* window as the sum of squares -- an
    // unweighted mean leaves fundamental residue that Hann cannot then
    // suppress, and re-introduces the error this function exists to remove.
    let mut sum_w = 0.0f32;
    let mut sum_wd = 0.0f32;
    for (&x, &w) in buf.iter().zip(HANN.iter()) {
        sum_w += w;
        sum_wd += w * ((x & ADC_MASK) as i32 - pivot) as f32;
    }
    // True weighted DC, expressed as an offset from the integer pivot.
    let offset = sum_wd / sum_w;

    // Pass 2. Two passes are mandatory, not stylistic: folding this into
    // pass 1 as sum(w*x^2)/sum(w) - mean^2 subtracts two ~4.2e6 quantities
    // to recover ~848, and f32 has no mantissa left for that cancellation.
    let mut sum_wd2 = 0.0f32;
    for (&x, &w) in buf.iter().zip(HANN.iter()) {
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
#[allow(dead_code)] // Retained for the optional bench frequency estimator.
pub fn half_phase(buf: &[u32; N], start: usize, pivot: i32) -> f32 {
    let mut i_acc = 0.0f32;
    let mut q_acc = 0.0f32;
    for m in 0..HALF {
        let d = ((buf[start + m] & ADC_MASK) as i32 - pivot) as f32 * HANN[2 * m];
        let p = m % SPP_NOM;
        i_acc += d * COS_TAB[p];
        q_acc += d * SIN_TAB[p];
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
#[allow(dead_code)] // Retained for the design report and bench diagnostics.
pub fn estimate_hz(buf: &[u32; N], pivot: i32) -> f32 {
    let p1 = half_phase(buf, 0, pivot);
    let p2 = half_phase(buf, HALF, pivot);

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
