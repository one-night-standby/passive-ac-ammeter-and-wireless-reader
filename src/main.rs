#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_mspm0::gpio::{Level, Output};
use embassy_mspm0::pac;
use embassy_mspm0::peripherals;
use embassy_time::Timer;
use panic_halt as _;

const PINCM_PA18: usize = 40; // OPA1_IN1+ (signal in)
const PINCM_PA16: usize = 38; // OPA1_OUT (also ADC1 channel 1, signal out)

fn set_analog(pincm: usize) {
    pac::IOMUX.pincm(pincm).modify(|w| {
        w.set_pf(0);
        w.set_pipu(false);
        w.set_pipd(false);
    });
}

const N: usize = 800;
static mut BUF: [u32; N] = [0; N];

/// Non-inverting PGA gain steps. The on-chip resistor ladder only has these
/// five fixed taps -- no continuous adjustment, and no step below 2x (unity
/// is a separate topology, plain buffer mode, not part of this ladder).
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum Gain {
    X2 = 1,
    X4 = 2,
    X8 = 3,
    X16 = 4,
    X32 = 5,
}

/// OPA1 as a non-inverting PGA (TRM 21.2.7.3.2): PSEL=EXTPIN1 (PA18, direct
/// to the amplifier's real high-impedance input), NSEL=RTAP (the ladder
/// tap feeds the inverting input), MSEL=VSS (ladder bottom grounded --
/// no bias, pure gain), GAIN=the selected step. OUTPIN=1 drives PA16.
fn setup_opa1_pga(gain: Gain) {
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

    opa.cfgbase().write(|w| {
        w.set_gbw(pac::opa::vals::Gbw::HIGHGAIN);
        w.set_rri(true);
    });

    opa.cfg().write(|w| {
        w.set_psel(pac::opa::vals::Psel::EXTPIN1);
        w.set_nsel(pac::opa::vals::Nsel::OANRTAP);
        w.set_msel(pac::opa::vals::Msel::VSS);
        w.set_gain(gain as u8);
        w.set_outpin(true);
    });

    opa.ctl().write(|w| w.set_enable(true));
    while !opa.stat().read().rdy() {}
}

/// ADC1 channel 1 (PA16 = OPA1_OUT), software-triggered single conversions.
/// Same pattern that just proved out clean on ADC0/PA24 -- no timer, no
/// event fabric, no DMA, so there's no risk of hitting that unrelated
/// event-subscriber bug again.
fn init_adc1() {
    use pac::adc::vals;
    let regs = pac::ADC1;

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

    regs.ctl1().modify(|w| {
        w.set_conseq(vals::Conseq::REPEATSINGLE);
        w.set_sampmode(vals::Sampmode::AUTO);
        w.set_trigsrc(vals::Trigsrc::SOFTWARE);
    });
    regs.ctl2().modify(|w| {
        w.set_startadd(0);
        w.set_endadd(0);
        w.set_res(vals::Res::BIT_12);
        w.set_df(false);
        w.set_sampcnt(vals::Sampcnt::from_bits(1));
    });
    regs.scomp0().modify(|w| w.set_val(25));

    regs.memctl(0).modify(|w| {
        w.set_chansel(1); // PA16
        w.set_vrsel(vals::Vrsel::from_bits(0)); // VDDA/VSSA
        w.set_stime(vals::Stime::SEL_SCOMP0);
        w.set_avgen(false);
        w.set_bcsen(false);
        w.set_trig(vals::Trig::TRIGGER_NEXT);
        w.set_wincomp(false);
    });

    regs.ctl0().modify(|w| w.set_enc(true));
}

/// One software-triggered conversion on ADC1, polled to completion.
fn read_once() -> u32 {
    use pac::adc::vals;
    let regs = pac::ADC1;
    regs.ctl1().modify(|w| w.set_sc(vals::Sc::START));
    cortex_m::asm::delay(32 * 20);
    regs.memres(0).read().data() as u32
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let _ = embassy_mspm0::init(Default::default());

    set_analog(PINCM_PA18);
    set_analog(PINCM_PA16);
    setup_opa1_pga(Gain::X32);
    init_adc1();

    // Busy-poll 800 samples of OPA1's output via ADC1.
    for i in 0..N {
        // SAFETY: BUF is only touched in this single-threaded loop before
        // anything else can read it.
        unsafe {
            (*core::ptr::addr_of_mut!(BUF))[i] = read_once();
        }
        cortex_m::asm::delay(32 * 30);
    }

    // Capture done; BUF now holds 800 polled samples of OPA1_OUT. Fast-
    // blink the LED to signal completion, then idle -- the buffer stays
    // put in SRAM for readback over SWD.
    let mut led = Output::new(unsafe { peripherals::PA0::steal() }, Level::Low);
    led.set_inversion(true);
    loop {
        led.toggle();
        Timer::after_millis(100).await;
    }
}
