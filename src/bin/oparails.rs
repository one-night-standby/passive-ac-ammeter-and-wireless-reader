#![no_std]
#![no_main]
// The firmware's modules are pulled in by path rather than through a lib
// target, so most of what they export is unused here. One crate-level allow
// beats an attribute per item, and this bin is not the place to restructure
// the crate.
#![allow(dead_code)]

//! Bench bin: measure where OPA1's output actually clips, and print the answer
//! on the LaunchPad's virtual COM port.
//!
//! This is where `range::OUT_LO`/`OUT_HI` come from. Every
//! `headroom`/`fits`/`over_range` decision in the autoranger is sized against
//! that window, so it is worth being able to re-derive rather than believe: run
//! this after a supply change, on a different board, or on any doubt about the
//! front end. Nothing at run time depends on it.
//!
//! # Output
//!
//! UART0 on PA10, 115200 8N1, through the board's XDS110 virtual COM port (J21
//! in its default XDS_UART position -- see `docs/LP-MSPM0G3507_开发板信息.md`).
//! Deliberately *not* UART2/PB15-PB16, which is the HC-42's.
//!
//! TX only. This bin reports and takes no commands, so PA11 is left alone.
//!
//! Every line is either a `#` comment or `tag key=value`/`tag,csv` data, so a
//! capture can be grepped by tag: `rail_lo`/`rail_hi` for the per-gain rails,
//! `window` for the result, `full`/`low`/`high` for the raw sweeps. The last
//! line of a pass is the literal `range.rs` constants, ready to paste.
//!
//! # Each rail needs the topology that can actually saturate it
//!
//! The amplifier is driven through its own muxes, so the result does not depend
//! on what is wired to the input. No single mux setting saturates *both* ends,
//! so each end gets the one that can.
//!
//! **High rail** -- PSEL=DAC12OUT, NSEL=OANRTAP, MSEL=VSS: a plain non-inverting
//! amplifier on the DAC, `Vout = G*Vdac`, with PA18 out of the path. At
//! `Vdac = full scale` this commands `G*VDDA`, which is at least twice what the
//! output stage can deliver, at every gain. Genuinely saturated.
//!
//! **Low rail** -- PSEL=EXTPIN1, NSEL=OANRTAP, MSEL=DAC12OUT: the firmware's own
//! topology, `Vout = G*Vin + (1-G)*Vdac`. The `(1-G)*Vdac` term is what makes a
//! *negative* command possible: at `Vdac = full scale` it asks for
//! `G*Vin - (G-1)*VDDA`, thousands of LSB below ground for any input that is not
//! sitting near the top rail. Genuinely saturated too.
//!
//! Every `rail_*` line therefore carries `cmd`, the commanded output in ADC LSB.
//! **If `cmd` is not far outside 0..4095, that line is not a rail** -- it is the
//! amplifier working linearly, and the number is whatever it was fed times the
//! gain. A flat-looking reading is not evidence of clipping on its own: command
//! an output of 0 V and the DAC's own floor plus the OPA's input offset leave a
//! few LSB at the input, which comes back as `G * Vos` and looks like a plausible
//! low rail at every gain. `cmd` is what distinguishes the two, and the `warn`
//! line checks the same thing from the readings alone.
//!
//! CFGBASE is left exactly as the firmware sets it (GBW=HIGHGAIN, RRI on), since
//! those bits configure the amplifier whose swing is being measured. What this
//! does assume is that the output swing is a property of the output stage and not
//! of which input mux or ladder-bottom source is selected -- the usual meaning of
//! an output swing spec, and what lets one end's answer come from a topology the
//! other end's did not use.
//!
//! # Resolution: coarse to find the shoulder, 1 LSB to resolve it
//!
//! At x2 the output moves 2 LSB per DAC LSB in the high-rail topology and 1 in
//! the low-rail one, so a 128-point sweep of the whole DAC range steps in tens of
//! LSB -- fine for the straight middle of the curve, useless for pinning a rail
//! that may sit within a few LSB of the supply. So `full` only *locates* the high
//! shoulder, and `high` then walks 128 codes at 1 DAC LSB across it. The low
//! shoulder needs no search: in the firmware topology the output crosses zero at
//! `Vdac = G*Vin/(G-1)`, which the probe's measured DC gives directly.
//!
//! # Reading the result
//!
//! `rdy_hi=0` or `rdy_lo=0` means OPA1 never came up in that topology and its
//! numbers mean nothing. `probe` is the untouched input; the low-rail topology
//! needs it, and the firmware needs it far more (see the note at the end).
//!
//! The `rail_lo` lines should agree across all five gains, and so should the
//! `rail_hi` ones -- the output stage does not know what the ladder is set to. A
//! progression that tracks the gain instead means that end was not saturated, and
//! `warn` says so.
//!
//! `window` is the tightest end across gains. `verdict` says, per end, only
//! whether the number is *resolvable*: an end sitting on 0 or 4095 has run out of
//! ADC codes, so the amplifier may stop exactly there or a hair beyond and this
//! measurement cannot tell which. `adc` therefore means "at the supply, to within
//! one LSB", not "a converter limit the amplifier would have beaten" -- ADC1 is
//! referenced to the same VDDA/VSSA that powers OPA1, so there is no version of
//! this where the two disagree by more than an LSB.
//!
//! Plot `low` and `high` to see *how* it clips: a sharp corner into a flat
//! plateau means a hard limit and a single min/max pair is the right model, while
//! a rounded knee means the amplifier compresses gradually before it rails and
//! `fits` wants a margin rather than a bound.

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_mspm0::dma::{self, Channel};
use embassy_mspm0::mode::Blocking;
use embassy_mspm0::uart::{Config as UartConfig, UartTx};
use embassy_mspm0::{bind_interrupts, pac, peripherals};
use embassy_time::Timer;
use panic_halt as _;

#[path = "../dac.rs"]
mod dac;
#[path = "../range.rs"]
mod range;
#[path = "../sampler.rs"]
mod sampler;

use range::{DAC_MAX, Gain, g_nominal, probe_stats};
use sampler::{
    ADC_CH_OPA_OUT, ADC_CH_RAW_IN, ADC_MASK, PINCM_PA16, PINCM_PA18, PROBE_BUF, capture,
    init_adc1_event, init_timer, set_adc_channel, set_analog,
};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

/// XDS110 virtual COM port. 8N1 comes from `UartConfig::default()`; only the
/// rate is stated here, because it is the one thing the terminal has to match.
const CONSOLE_BAUD_RATE: u32 = 115_200;

/// Points in the full-range sweep. Enough to see the shape and to bracket the
/// high shoulder to within `COARSE_STEP`; the 1 LSB detail comes from the zooms.
const FULL_N: usize = 128;

/// DAC codes per full-sweep point. 128 * 32 = 4096, so the sweep covers the whole
/// stimulus range and `code = i * COARSE_STEP` stays exact -- which is what lets
/// the shoulder search hand a code straight to a zoom sweep.
const COARSE_STEP: u16 = 32;

/// Codes per shoulder zoom, one per point. Four times the coarse step, so a
/// window centred on the bracketed transition contains it with room to spare on
/// both sides -- the plateau either side of the corner is what shows whether the
/// corner is sharp.
const ZOOM_N: usize = 128;

/// Conversions averaged per point. At 4 kHz this is 4 ms, deliberately not a
/// whole mains cycle: the high-rail topology has the input out of the path
/// entirely, and the low-rail one is saturated wherever it matters, so this is
/// purely noise averaging.
const AVG_N: usize = 16;

const GAINS: [Gain; 5] = [Gain::X2, Gain::X4, Gain::X8, Gain::X16, Gain::X32];

/// Gain used for the swept sections. The lowest step on the ladder, because the
/// output moves the fewest LSB per DAC LSB there and the shallowest slope
/// resolves a corner best. The `rail_*` lines check the rails do not move with
/// gain.
const CURVE_GAIN: Gain = Gain::X2;

/// How close two neighbouring points count as the same plateau, when locating the
/// high shoulder in the coarse sweep. That sweep steps the output 64 LSB per
/// point, so this cannot mistake slope for plateau.
const FLAT_LSB: i32 = 3;

/// How far outside 0..4095 a commanded output has to be before the reading is
/// accepted as a rail rather than as linear operation.
///
/// Sized against the thing it has to rule out, not against full scale. The only
/// way a reading can look like a rail without being one is the amplifier working
/// linearly on a small residual -- the DAC's output floor plus the OPA's input
/// offset -- and that residual measured about 6 LSB at the input, so it can
/// account for at most `32 * 6 = 203` LSB of output at the top of the ladder.
/// 512 is comfortably past that while still admitting the low rail at x2, where
/// the deepest command the DAC can ask for is only `2*Vin - VDDA`.
const CMD_OVERDRIVE: i32 = 512;

static mut AVG_BUF: [u32; AVG_N] = [0; AVG_N];

/// One end's reading at one gain, with the output that was commanded to produce
/// it. The command is what decides whether the reading is a rail at all; see the
/// module docs.
#[derive(Clone, Copy)]
struct Rail {
    out: u16,
    /// `None` when the probe failed, so the low-rail topology's command -- which
    /// depends on the input's DC -- is unknown.
    cmd: Option<i32>,
}

impl Rail {
    /// Whether the amplifier was demonstrably driven past what it can deliver.
    /// Unknown commands count as not proven, which keeps them out of `window`.
    fn saturated(&self) -> bool {
        match self.cmd {
            Some(c) => c < -CMD_OVERDRIVE || c > ADC_MASK as i32 + CMD_OVERDRIVE,
            None => false,
        }
    }
}

/// `core::fmt` sink on UART0, so the report is written with plain `writeln!`
/// instead of formatting into a `heapless::String` a line at a time.
struct Serial<'d>(UartTx<'d, Blocking>);

impl Serial<'_> {
    fn put(&mut self, bytes: &[u8]) {
        let _ = self.0.blocking_write(bytes);
    }
}

impl core::fmt::Write for Serial<'_> {
    /// Translates bare LF to CRLF. `writeln!` emits `\n` alone, and a raw
    /// terminal on the VCOM stair-steps without the CR.
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for part in s.split_inclusive('\n') {
            match part.strip_suffix('\n') {
                Some(head) => {
                    self.put(head.as_bytes());
                    self.put(b"\r\n");
                }
                None => self.put(part.as_bytes()),
            }
        }
        Ok(())
    }
}

/// Mean of `AVG_N` conversions on whichever channel is currently selected.
/// `None` means the transfer never drained, in which case the buffer holds a mix
/// of new and stale samples and must not be averaged.
fn read_mean(dma: &mut Channel<'_>) -> Option<u16> {
    // SAFETY: `capture` waits the transfer out (or pauses it) before it
    // returns, so nothing else touches AVG_BUF concurrently.
    if !capture(dma, unsafe { &mut *core::ptr::addr_of_mut!(AVG_BUF) }) {
        return None;
    }
    // SAFETY: as above -- the transfer is finished or paused.
    let buf = unsafe { &*core::ptr::addr_of!(AVG_BUF) };
    let sum: u32 = buf.iter().map(|&x| x & ADC_MASK).sum();
    Some((sum / AVG_N as u32) as u16)
}

/// Park the DAC at `code`, let the amplifier get there, and read the output.
///
/// The DAC settles in microseconds; the slow half is the OPA recovering from
/// saturation, which is the state it is in at the far end of every sweep here.
async fn read_at(dma: &mut Channel<'_>, code: u16) -> Option<u16> {
    dac::set(code.min(DAC_MAX as u16));
    Timer::after_millis(1).await;
    read_mean(dma)
}

/// `out.len()` points starting at DAC code `start`, `step` codes apart.
/// False means a capture never drained and `out` must not be interpreted.
async fn sweep(dma: &mut Channel<'_>, start: u16, step: u16, out: &mut [u16]) -> bool {
    for (i, slot) in out.iter_mut().enumerate() {
        match read_at(dma, start + i as u16 * step).await {
            Some(v) => *slot = v,
            // A capture that never drained means the sampling chain is broken,
            // not that this one point is noisy. A window built partly from stale
            // samples would look perfectly reasonable, which is the failure
            // worth avoiding here.
            None => return false,
        }
    }
    true
}

/// DAC code where the coarse sweep reaches the high rail, bracketed to within
/// `COARSE_STEP`.
///
/// Searched from the top down: the sweep's last point is saturated by
/// construction (`Vdac = full scale` commands `G*VDDA`), so walking back while
/// points still agree with it finds where the plateau starts, without needing to
/// know in advance how wide it is.
fn top_shoulder(full: &[u16; FULL_N]) -> u16 {
    let last = full[FULL_N - 1] as i32;
    let mut i = FULL_N - 1;
    while i > 0 && (full[i - 1] as i32 - last).abs() <= FLAT_LSB {
        i -= 1;
    }
    i as u16 * COARSE_STEP
}

/// Commanded output in ADC LSB for the high-rail topology, `Vout = G*Vdac`.
fn cmd_hi(gain: Gain, code: u16) -> i32 {
    g_nominal(gain) * code as i32
}

/// Commanded output in ADC LSB for the low-rail topology,
/// `Vout = G*Vin + (1-G)*Vdac`. Needs the input's measured DC, so it is only
/// known when the probe succeeded.
fn cmd_lo(gain: Gain, mean_in: i32, code: u16) -> i32 {
    let g = g_nominal(gain);
    g * mean_in + (1 - g) * code as i32
}

/// Everything one pass measured, so the reporting does no measuring and the
/// measuring does no reporting.
struct Measurement {
    /// Output at `Vdac = full scale` in the low-rail topology, per gain.
    rail_lo: [Option<Rail>; GAINS.len()],
    /// Output at `Vdac = full scale` in the high-rail topology, per gain.
    rail_hi: [Option<Rail>; GAINS.len()],
    /// `Vout = G*Vdac` across the whole DAC range, at `CURVE_GAIN`.
    full: [u16; FULL_N],
    full_ok: bool,
    /// The low shoulder, low-rail topology, one DAC code per point.
    low: [u16; ZOOM_N],
    low_start: u16,
    low_ok: bool,
    /// The high shoulder, high-rail topology, one DAC code per point.
    high: [u16; ZOOM_N],
    high_start: u16,
    high_ok: bool,
    /// OPA1's ready bit in each topology.
    rdy_hi: bool,
    rdy_lo: bool,
    /// The untouched input. The low-rail topology needs it; so does the firmware.
    probe: Option<(i32, u32)>,
}

impl Measurement {
    /// The window that holds on every gain: the tightest end observed, counting
    /// only readings whose command proves them saturated. `None` for an end where
    /// nothing qualified.
    ///
    /// The filter is the point. An unsaturated reading at the low end sits
    /// *above* the true rail, because the amplifier is still following its input
    /// rather than pinned; letting one into a `max` would raise the window's floor
    /// to a number that has nothing to do with clipping.
    fn window(&self) -> (Option<u16>, Option<u16>) {
        let lo = self
            .rail_lo
            .iter()
            .flatten()
            .filter(|r| r.saturated())
            .map(|r| r.out);
        let hi = self
            .rail_hi
            .iter()
            .flatten()
            .filter(|r| r.saturated())
            .map(|r| r.out);
        (lo.max(), hi.min())
    }
}

/// OPA1 through its own muxes: `psel` picks what drives the non-inverting input,
/// `msel` what sits at the ladder bottom, NSEL always the ladder tap.
///
/// Full reset/configure/enable rather than a live `modify`: TRM 21.2.8 only
/// permits changing CFG live for GAIN, and switching topology moves PSEL and
/// MSEL. Returns OPA1's ready bit -- reported rather than waited on forever, same
/// principle as `range::setup_opa1_pga`.
fn setup_opa1(
    psel: pac::opa::vals::Psel,
    msel: pac::opa::vals::Msel,
    gain: Gain,
    dac_code: u16,
) -> bool {
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

    // Identical to the firmware's: these bits configure the amplifier whose
    // swing is being measured, so changing them would measure a different one.
    opa.cfgbase().write(|w| {
        w.set_gbw(pac::opa::vals::Gbw::HIGHGAIN);
        w.set_rri(true);
    });

    opa.cfg().write(|w| {
        w.set_psel(psel);
        w.set_nsel(pac::opa::vals::Nsel::OANRTAP);
        w.set_msel(msel);
        w.set_gain(gain as u8);
        w.set_outpin(true);
    });

    opa.ctl().write(|w| w.set_enable(true));
    for _ in 0..100_000u32 {
        if opa.stat().read().rdy() {
            return true;
        }
    }
    false
}

/// `Vout = G*Vdac`, PA18 out of the path. Saturates the high rail.
fn setup_high_rail(gain: Gain) -> bool {
    setup_opa1(
        pac::opa::vals::Psel::DAC12OUT,
        pac::opa::vals::Msel::VSS,
        gain,
        0,
    )
}

/// `Vout = G*Vin + (1-G)*Vdac` -- the firmware's topology. Saturates the low
/// rail, because the DAC term is what makes a negative command possible.
fn setup_low_rail(gain: Gain) -> bool {
    setup_opa1(
        pac::opa::vals::Psel::EXTPIN1,
        pac::opa::vals::Msel::DAC12OUT,
        gain,
        0,
    )
}

/// The endpoint reading at each gain, in whichever topology is already set up,
/// paired with what `cmd` says was asked of the amplifier to get it.
///
/// Live GAIN writes are permitted by TRM 21.2.8 because the other CFG bits are
/// untouched and GAIN never takes 0x0 -- the same two conditions
/// `range::apply_range` relies on.
async fn rails_each_gain(
    dma: &mut Channel<'_>,
    into: &mut [Option<Rail>; GAINS.len()],
    cmd: impl Fn(Gain) -> Option<i32>,
) {
    for (slot, gain) in into.iter_mut().zip(GAINS) {
        pac::OPA1.cfg().modify(|w| w.set_gain(gain as u8));
        // Longer than a sweep step: this is a full-rail swing plus a gain change,
        // not one more code along a ramp.
        Timer::after_millis(2).await;
        *slot = read_at(dma, DAC_MAX as u16).await.map(|out| Rail {
            out,
            cmd: cmd(gain),
        });
    }
    pac::OPA1.cfg().modify(|w| w.set_gain(CURVE_GAIN as u8));
    Timer::after_millis(2).await;
}

async fn measure(dma: &mut Channel<'_>, s: &mut Serial<'_>) -> Measurement {
    // The input first: the low-rail topology's commanded output depends on it, so
    // it is a prerequisite and not just a report.
    set_adc_channel(ADC_CH_RAW_IN);
    // SAFETY: `capture` waits the transfer out (or pauses it) before returning,
    // so nothing else touches PROBE_BUF concurrently.
    let probed = capture(dma, unsafe { &mut *core::ptr::addr_of_mut!(PROBE_BUF) });
    let probe = probed.then(|| {
        // SAFETY: as above -- the transfer is finished or paused.
        let (mean, pp, _) = probe_stats(unsafe { &*core::ptr::addr_of!(PROBE_BUF) });
        (mean, pp)
    });

    let mut m = Measurement {
        rail_lo: [None; GAINS.len()],
        rail_hi: [None; GAINS.len()],
        full: [0; FULL_N],
        full_ok: false,
        low: [0; ZOOM_N],
        low_start: 0,
        low_ok: false,
        high: [0; ZOOM_N],
        high_start: 0,
        high_ok: false,
        rdy_hi: false,
        rdy_lo: false,
        probe,
    };

    // --- High rail: Vout = G*Vdac ---
    m.rdy_hi = setup_high_rail(CURVE_GAIN);
    set_adc_channel(ADC_CH_OPA_OUT);

    let _ = writeln!(s, "# high rails, each gain");
    rails_each_gain(dma, &mut m.rail_hi, |g| Some(cmd_hi(g, DAC_MAX as u16))).await;

    let _ = writeln!(s, "# full sweep");
    m.full_ok = sweep(dma, 0, COARSE_STEP, &mut m.full).await;

    if m.full_ok {
        // Centre the window on the transition the coarse sweep bracketed.
        // Saturating, because a shoulder inside the first coarse step would put
        // the start below zero.
        m.high_start = top_shoulder(&m.full).saturating_sub(ZOOM_N as u16 / 2);
        let _ = writeln!(s, "# high shoulder from {}", m.high_start);
        m.high_ok = sweep(dma, m.high_start, 1, &mut m.high).await;
    }

    // --- Low rail: Vout = G*Vin + (1-G)*Vdac, the firmware's topology ---
    m.rdy_lo = setup_low_rail(CURVE_GAIN);
    set_adc_channel(ADC_CH_OPA_OUT);

    let _ = writeln!(s, "# low rails, each gain");
    let probe_mean = m.probe.map(|(mean, _)| mean);
    rails_each_gain(dma, &mut m.rail_lo, |g| {
        probe_mean.map(|mean| cmd_lo(g, mean, DAC_MAX as u16))
    })
    .await;

    // No search needed for this shoulder: the output crosses zero at
    // Vdac = G*Vin/(G-1), which the probe already measured. Clamped so the
    // window stays inside the DAC's range instead of piling points onto the
    // endpoint that `read_at` clamps to.
    if let Some((mean_in, _)) = m.probe {
        let g = g_nominal(CURVE_GAIN);
        let cross = (g * mean_in / (g - 1)).clamp(0, DAC_MAX) as u16;
        m.low_start = cross
            .saturating_sub(ZOOM_N as u16 / 2)
            .min(DAC_MAX as u16 + 1 - ZOOM_N as u16);
        let _ = writeln!(s, "# low shoulder from {}", m.low_start);
        m.low_ok = sweep(dma, m.low_start, 1, &mut m.low).await;
    }

    m
}

/// One swept section as `tag,dac,out` rows, or a single comment if it failed.
fn report_sweep(s: &mut Serial<'_>, tag: &str, out: &[u16], start: u16, step: u16, ok: bool) {
    if !ok {
        let _ = writeln!(s, "# {} skipped", tag);
        return;
    }
    for (i, v) in out.iter().enumerate() {
        let _ = writeln!(s, "{},{},{}", tag, start + i as u16 * step, v);
    }
}

/// One end's per-gain rails, each with the output that was commanded to produce
/// it -- see the module docs on why `cmd` is on every line.
fn report_rails(s: &mut Serial<'_>, tag: &str, reads: &[Option<Rail>; GAINS.len()]) {
    for (gain, read) in GAINS.iter().zip(reads) {
        let n = g_nominal(*gain);
        match read {
            Some(r) => match r.cmd {
                Some(c) => {
                    let _ = writeln!(
                        s,
                        "{} x{} out={} cmd={} saturated={}",
                        tag,
                        n,
                        r.out,
                        c,
                        u8::from(r.saturated())
                    );
                }
                None => {
                    let _ = writeln!(s, "{} x{} out={} cmd=? saturated=?", tag, n, r.out);
                }
            },
            None => {
                let _ = writeln!(s, "# {} x{} skipped", tag, n);
            }
        }
    }
}

/// True when the readings track the gain instead of agreeing with each other --
/// the signature of an unsaturated end, where the number is `G` times whatever
/// the amplifier was fed rather than a rail.
///
/// Independent of `Rail::saturated`, deliberately: that one trusts the commanded
/// output, this one only looks at the readings. They would be fooled by different
/// things, so both are cheap insurance against reporting linear operation as a
/// rail.
fn tracks_gain(reads: &[Option<Rail>; GAINS.len()]) -> bool {
    let lo = reads.iter().flatten().map(|r| r.out).min();
    let hi = reads.iter().flatten().map(|r| r.out).max();
    match (lo, hi) {
        // The ladder spans 16x from end to end, so a genuine rail cannot vary
        // anywhere near that much; 2x is well clear of noise either way.
        (Some(lo), Some(hi)) => hi as i32 > 2 * lo.max(1) as i32,
        _ => false,
    }
}

fn report(s: &mut Serial<'_>, pass: u32, m: &Measurement) {
    let _ = writeln!(s, "# --- pass {} ---", pass);
    let _ = writeln!(
        s,
        "rdy_hi={} rdy_lo={}",
        u8::from(m.rdy_hi),
        u8::from(m.rdy_lo)
    );
    match m.probe {
        Some((mean, pp)) => {
            let _ = writeln!(s, "probe vin={} pp={}", mean, pp);
        }
        None => {
            let _ = writeln!(s, "# probe skipped, so the low rail could not be driven");
        }
    }

    report_rails(s, "rail_hi", &m.rail_hi);
    report_rails(s, "rail_lo", &m.rail_lo);

    if tracks_gain(&m.rail_hi) {
        let _ = writeln!(s, "warn rail_hi tracks gain: not saturated, G*Vin not VOH");
    }
    if tracks_gain(&m.rail_lo) {
        let _ = writeln!(s, "warn rail_lo tracks gain: not saturated, G*Vos not VOL");
    }

    let (lo, hi) = m.window();
    match (lo, hi) {
        (Some(lo), Some(hi)) => {
            let _ = writeln!(s, "window lo={} hi={}", lo, hi);
            // Whether the number is resolvable, not which of two limits won. An
            // end on 0 or 4095 has run out of ADC codes, so the amplifier may
            // stop exactly there or a hair beyond and this cannot tell which.
            // One LSB of slack either way: the last count is not worth arguing
            // over.
            let _ = writeln!(
                s,
                "verdict lo={} hi={}",
                if lo <= 1 { "adc" } else { "opa" },
                if hi as u32 >= ADC_MASK - 1 {
                    "adc"
                } else {
                    "opa"
                },
            );
        }
        _ => {
            let _ = writeln!(s, "# window unknown: an end produced no reading");
        }
    }

    let _ = writeln!(s, "# section,dac,out");
    report_sweep(s, "full", &m.full, 0, COARSE_STEP, m.full_ok);
    report_sweep(s, "low", &m.low, m.low_start, 1, m.low_ok);
    report_sweep(s, "high", &m.high, m.high_start, 1, m.high_ok);

    if let (Some(lo), Some(hi)) = (lo, hi) {
        let _ = writeln!(s, "# range.rs: OUT_LO: i32 = {}; OUT_HI: i32 = {};", lo, hi);
    }
    let _ = writeln!(s, "# --- end pass {} ---", pass);
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    set_analog(PINCM_PA18);
    set_analog(PINCM_PA16);
    init_adc1_event();
    init_timer();

    let mut dma = Channel::new(p.DMA_CH0, Irqs);

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = CONSOLE_BAUD_RATE;
    // TX only: this bin reports and takes no commands, so PA11 stays free.
    let mut s = Serial(UartTx::new_blocking(p.UART0, p.PA10, uart_config).unwrap());

    let _ = writeln!(s, "\n# oparails: OPA1 output clip window, in ADC LSB");
    let _ = writeln!(
        s,
        "# rail_hi,full,high: PSEL=DAC12OUT MSEL=VSS -> Vout = G*Vdac"
    );
    let _ = writeln!(
        s,
        "# rail_lo,low: PSEL=EXTPIN1 MSEL=DAC12OUT -> Vout = G*Vin+(1-G)*Vdac"
    );
    let _ = writeln!(s, "# cfgbase GBW=HIGHGAIN RRI=1, same as the firmware");
    let _ = writeln!(
        s,
        "# adc ADC1 ch1 = PA16 = OPA1_OUT, 12-bit, VDDA/VSSA ref, {} averaged",
        AVG_N
    );
    let _ = writeln!(
        s,
        "# a rail line with saturated=0 is linear operation, not a rail"
    );

    let mut pass = 1u32;
    loop {
        let m = measure(&mut dma, &mut s).await;
        report(&mut s, pass, &m);
        pass += 1;
        // Long enough that a capture is easy to grab one pass at a time, and that
        // changing something on the bench between passes is practical.
        Timer::after_millis(5000).await;
    }
}
