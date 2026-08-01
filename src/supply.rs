//! What the rail is doing, read off ADC0.
//!
//! Channel 15 is not a pin. Selecting it disconnects the external input and
//! ties the converter to an internal divider at VDD/3 (datasheet table 8-8, and
//! 7.12.1 note 4), so this needs no pin, no board copper and no divider of its
//! own. ADC0 is otherwise unused here -- the measurement chain is ADC1 -- and
//! with channel 15 selected its mux reaches none of the pins ADC0 shares with
//! the rest of the firmware, PA26 among them.
//!
//! Read against the same 2.5 V reference the measurement chain runs on, which
//! is the only reference this program ever selects. VDD/3 is 1.1 V at 3.3 V, so
//! the answer sits at code 1802 of 4095 -- 44% of the scale, 1.83 mV of supply
//! per code -- and full scale is 7.5 V.
//!
//! The code is a *ratio*, VDD/3 over VREF, so everything this module concludes
//! rests on VREF actually being at 2.5 V when the sample is taken. A reference
//! that is low reads the supply high, in direct proportion, and the one thing
//! that reliably makes it low is not having waited for it: `vref::init` returns
//! on the READY bit, which says the reference core is running, not that the
//! 1 uF CVREF it drives has charged. Hence `VREF_SETTLE_MS` below, and hence
//! `MAX_PLAUSIBLE_CODE` -- above 3.6 V there is no supply the part survives, so
//! a code that high is a statement about the reference and not about the rail.
//!
//! The 2.5 V setting needs VDD >= 2.7 V to be in spec, which is *below* the
//! voltage being judged, so the verdict is taken where the reference is valid.
//! Under 2.7 V the reference sags with the supply it is powered from, and that
//! sag reads high too -- to fake a pass at 3.3 V it would have to collapse to
//! 1.97 V, which is a supply the CPU is not executing at. Out of spec here
//! means imprecise, not wrong-sided.
//!
//! The 1.4 V setting BUFCONFIG also offers would be valid further down, and is
//! still not worth having: a second reference voltage means CVREF has to be
//! pulled from 2.5 V down to 1.4 V, which the buffer cannot do -- TRM 23.2.1
//! wants the module disabled and PA23 driven low as a GPIO for 100 us first.
//! A gate at 3.3 V does not need a valid number at 2 V. It needs a correct
//! yes/no at 3.3 V, and one reference voltage gives it that for nothing.
//!
//! Nothing here is left running. The reference is brought up and put back down
//! around each conversion, so `meter::measure` still starts from an unpowered
//! VREF and brings up its own.

use embassy_mspm0::pac;
use embassy_time::Timer;

use crate::vref;

/// ADC0's internal supply monitor.
const CH_SUPPLY: u8 = 15;

/// Sample window, in the units SCOMP0 counts: sample clocks times the SCLKDIV
/// divide value, so with SYSOSC at 32 MHz and DIV_BY_4 one count is 125 ns.
/// The window has to cover the divider's own 5 us settling (datasheet 7.12.2
/// tSample_SupplyMon) *and* the 5 us ADC wake-up that automatic power-down puts
/// inside every sample window (TRM 18.2.9); 200 counts is 25 us, past both with
/// room to spare. A single conversion per display decision is nowhere near
/// often enough for the time to be worth shaving.
const SAMPLE_CYCLES: u16 = 200;

/// Bounded like the reference's READY wait, and for the same reason: never
/// block the CPU forever on analog state. One conversion is the 25 us sample
/// plus a microsecond of converting, so exhausting this budget means the
/// converter is not running at all rather than that it is running slowly.
const CONVERT_TRIES: u32 = 10_000;

/// The supply that would put channel 15 at full scale: the divider is VDD/3 and
/// the reference is 2.5 V, so 7.5 V spans the 4096 codes.
const FULL_SCALE_MV: u32 = 3 * 2_500;

const CODES: u32 = 4096;

/// The most the part is rated to be powered at (datasheet 7.1), as a code. The
/// converter cannot report a supply above this from a chip that is executing,
/// so a code at or past it did not come from the rail -- it came from a
/// reference that had not reached 2.5 V, which pushes the ratio up without
/// bound and clips at full scale.
///
/// Not the fix for an unsettled reference, only the floor under it: a reference
/// that is merely *part way* up reads high by a factor this cannot see. What it
/// does catch is the collapse -- and a collapsed reference otherwise reports
/// 7.5 V, which passes every threshold anyone would write here.
const MAX_PLAUSIBLE_CODE: u16 = (3_600 * CODES / FULL_SCALE_MV) as u16;

/// How long the reference is given after `vref::init` returns, before the
/// sample that is scaled against it.
///
/// READY is not settling. It reports the reference core running; CVREF is 1 uF
/// hung off a buffer whose whole quiescent budget is 189 uA, and it starts a
/// power cycle discharged. Sample before it has arrived and the ratio reads
/// high -- which on a gate means opening below the threshold, the one failure
/// direction that matters here.
///
/// Same 20 ms `meter::measure` gives it inside AFE_SETTLE_MS. Not a figure this
/// module derived: it is the settle every reading in `cal.csv` was taken after,
/// so it is the one span on this board with evidence behind it.
const VREF_SETTLE_MS: u64 = 20;

/// How often the rail is re-measured while `wait_above` waits on it.
///
/// Long against `VREF_SETTLE_MS` on purpose. A look is now the settle plus a
/// conversion rather than a conversion alone, and the reference is enabled for
/// all of it, so the ratio of the two is the duty cycle VREF runs at across the
/// wait: 10%, or 19 uA of the 189 uA typ it draws enabled. Shortening this does
/// not find the rail sooner in any way that matters -- the rail is climbing
/// over seconds -- it just spends the charge that climb is made of.
const POLL_MS: u64 = 200;

/// Block until the rail is at `min_mv` or above. There is no deadline: a supply
/// that never gets there waits here forever.
///
/// A supply that cannot be measured counts as "not yet" rather than as
/// "unknown, carry on". `millivolts` only comes back `None` for a reference that
/// never settled, which is either a supply under the 2.7 V that setting needs --
/// a rail that may still be climbing, and the whole thing being waited for -- or
/// a missing CVREF, which is a board that has nothing to measure against.
pub async fn wait_above(min_mv: u16) {
    while !millivolts().await.is_some_and(|mv| mv >= min_mv) {
        Timer::after_millis(POLL_MS).await;
    }
}

/// VDD in millivolts, or `None` if it could not be measured.
///
/// `None` covers two cases, and both mean "no number", not "carry on". The
/// reference never reported ready -- a supply below the 2.7 V it needs, or a
/// missing CVREF cap. Or it reported a supply the part cannot be running at,
/// which says the reference was not at 2.5 V when the sample was taken and so
/// says nothing about the rail.
pub async fn millivolts() -> Option<u16> {
    let code = if vref::init() {
        Timer::after_millis(VREF_SETTLE_MS).await;
        convert()
    } else {
        None
    };
    vref::power_down();
    // Rounded, not truncated. One code is 1.83 mV of supply and the converter
    // has already floored once; flooring the scaling too would put an exact
    // 3.3 V rail at 3298 mV, under a threshold someone will reasonably write
    // as 3300.
    code.filter(|&code| code < MAX_PLAUSIBLE_CODE)
        .map(|code| ((u32::from(code) * FULL_SCALE_MV + CODES / 2) / CODES) as u16)
}

/// One software-triggered conversion of channel 15, ADC0 powered down again
/// before returning either way.
///
/// Powering the block down is what disconnects the divider: it draws 10 uA
/// (datasheet 7.12.1 ISupplyMon) for as long as channel 15 is selected on a
/// live ADC, which is the same order as the analog blocks `meter::measure`
/// takes such care to switch off between readings.
fn convert() -> Option<u16> {
    use pac::adc::vals;
    let regs = pac::ADC0;

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

    // One channel, once, on a software trigger: the sample clock this needs is
    // its own sample timer and nothing else, so unlike ADC1 there is no timer,
    // no event channel and no DMA in the path.
    regs.ctl1().modify(|w| {
        w.set_conseq(vals::Conseq::SINGLE);
        w.set_sampmode(vals::Sampmode::AUTO);
        w.set_trigsrc(vals::Trigsrc::SOFTWARE);
    });
    regs.ctl2().modify(|w| {
        w.set_startadd(0);
        w.set_endadd(0);
        w.set_res(vals::Res::BIT_12);
        w.set_df(false);
    });
    regs.scomp0().modify(|w| w.set_val(SAMPLE_CYCLES));

    regs.memctl(0).modify(|w| {
        w.set_chansel(CH_SUPPLY);
        // VRSEL=2: internal reference, VSS as the negative -- the same mode
        // ADC1 uses, against the 2.5 V `millivolts` just brought up.
        w.set_vrsel(vals::Vrsel::from_bits(2));
        w.set_stime(vals::Stime::SEL_SCOMP0);
        w.set_avgen(false);
        w.set_bcsen(false);
        w.set_trig(vals::Trig::AUTO_NEXT);
        w.set_wincomp(false);
    });

    regs.ctl0().modify(|w| w.set_enc(true));
    regs.ctl1().modify(|w| w.set_sc(vals::Sc::START));

    // SINGLE hardware-clears ENC once the result is in MEMRES0, so ENC going
    // low is the conversion finishing.
    let mut done = false;
    for _ in 0..CONVERT_TRIES {
        if !regs.ctl0().read().enc() {
            done = true;
            break;
        }
    }
    let code = regs.memres(0).read().data();

    power_down();
    done.then_some(code)
}

fn power_down() {
    let regs = pac::ADC0;
    regs.ctl0().modify(|w| w.set_enc(false));
    regs.gprcm(0).pwren().write(|w| {
        w.set_key(pac::adc::vals::PwrenKey::KEY);
        w.set_enable(false);
    });
}
