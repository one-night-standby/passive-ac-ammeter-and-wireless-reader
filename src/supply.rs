//! What the rail is doing, read off ADC0.
//!
//! Channel 15 is not a pin. Selecting it disconnects the external input and
//! ties the converter to an internal divider at VDD/3 (datasheet table 8-8, and
//! 7.12.1 note 4), so this needs no pin, no board copper and no divider of its
//! own. ADC0 is otherwise unused here -- the measurement chain is ADC1 -- and
//! with channel 15 selected its mux reaches none of the pins ADC0 shares with
//! the rest of the firmware, PA26 among them.
//!
//! Read against the 1.4 V reference rather than the 2.5 V one the meter runs
//! on. VDD/3 is 1.1 V at 3.3 V and 1.2 V at the 3.6 V the MCU is rated to, so
//! 1.4 V full scale holds the whole operating range without clipping, and it is
//! itself in spec down to VDD = 1.62 V. The 2.5 V setting stops being valid at
//! 2.7 V, which is inside the range this exists to watch: it would go out of
//! spec exactly where the answer starts to matter, and `READY` would still read
//! 1 while it did.
//!
//! Nothing here is shared with a reading. The reference is brought up and put
//! back down around the one conversion, so `meter::measure` still starts from
//! an unpowered VREF and configures it for its own 2.5 V.

use embassy_mspm0::pac;

use crate::vref::{self, Output};

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
/// the reference is 1.4 V, so 4.2 V spans the 4096 codes. That is above the
/// 3.6 V maximum the MCU is rated for, which is the point -- no supply the part
/// survives can push the code off the top of the scale and come back looking
/// like a healthy rail.
const FULL_SCALE_MV: u32 = 3 * 1_400;

const CODES: u32 = 4096;

/// VDD in millivolts, or `None` if it could not be measured.
///
/// `None` means the reference never reported ready, which is either a supply
/// below the 1.62 V that setting needs or a missing CVREF cap -- and neither
/// leaves a number worth returning.
pub fn millivolts() -> Option<u16> {
    let code = if vref::init(Output::V1_4) {
        convert()
    } else {
        None
    };
    vref::power_down();
    // Rounded, not truncated. One code is 1.03 mV of supply and the converter
    // has already floored once; flooring the scaling too would put an exact
    // 3.3 V rail at 3299 mV, a millivolt under a threshold someone will
    // reasonably write as 3300.
    code.map(|code| ((u32::from(code) * FULL_SCALE_MV + CODES / 2) / CODES) as u16)
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
        // ADC1 uses, at the 1.4 V setting `millivolts` just brought up.
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
