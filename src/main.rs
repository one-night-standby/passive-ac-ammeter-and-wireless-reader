#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_mspm0::bind_interrupts;
use embassy_mspm0::dma::{self, Channel, TransferOptions};
use embassy_mspm0::gpio::{Level, Output};
use embassy_mspm0::pac;
use embassy_mspm0::peripherals;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_9X15;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use heapless::String;
use libm::sqrtf;
use panic_halt as _;

mod dac;
mod oled;
use oled::Oled;

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

/// Event fabric channel TIMG6 publishes its zero event on and ADC1
/// subscribes to. Free pick -- only one producer/consumer pair exists in
/// this firmware so far. (This is a *different* namespace from the DMA
/// trigger ID below: this one is the generic 16-channel event crossbar,
/// the other is ADC1's own hardwired "result loaded" line into the DMA.)
const EVT_CH_ADC1: u8 = 1;
/// ADC1's dedicated hardwired DMA request line (G350x datasheet:
/// DMA_ADC1_EVT_GEN_BD_TRIG). Confirmed against
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/sampler.rs, which
/// runs this exact TIM+ADC+DMA chain on the same silicon.
const DMA_TRIG_ADC1: u8 = 24;

const TIMER_CLK_HZ: u32 = 32_000_000; // BUSCLK
/// STRUCTURE.md: 4k samples/sec, 800 points (10 cycles of 50 Hz mains).
const SAMPLE_HZ: u32 = 4_000;
const LOAD: u32 = TIMER_CLK_HZ / SAMPLE_HZ - 1;

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

/// DAC12 output code for a 1.7 V bias reference (VDDA = 3.3 V assumed):
/// 1.7 / 3.3 * 4096 ~= 2110.
const DAC_CODE_1V7: u16 = 2110;

/// OPA1 as a non-inverting PGA (TRM 21.2.7.3.2): PSEL=EXTPIN1 (PA18, through
/// the P-MUX into the amplifier's true input -- datasheet SS7.19.1 puts this
/// mux's series resistance at 2.6 kOhm TYP, but the real op-amp input beyond
/// it draws only leakage-level bias current, so that resistance drops
/// negligible voltage and doesn't meaningfully load an external source),
/// NSEL=RTAP (the ladder tap feeds the inverting input), MSEL=DAC12OUT
/// (ladder bottom biased to
/// 1.7 V instead of ground, so the gain acts on (Vin - 1.7 V) rather than
/// Vin itself -- same pattern as
/// ~/embed/Single-Phase-Power-Analyzer's OPA1 setup, just a different bias
/// point and gain step). GAIN=the selected step. OUTPIN=1 drives PA16.
fn setup_opa1_pga(gain: Gain) {
    // The bias reference must be up and stable before the OPA turns on.
    dac::init();
    dac::set(DAC_CODE_1V7);

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
        w.set_msel(pac::opa::vals::Msel::DAC12OUT);
        w.set_gain(gain as u8);
        w.set_outpin(true);
    });

    opa.ctl().write(|w| w.set_enable(true));
    // Bounded wait -- never block the CPU forever on external hardware
    // state. If RDY never asserts (bad register sequencing, an invalid
    // config, or a real hardware limit on this gain/topology combo), give
    // up and move on rather than hanging the whole MCU (which is exactly
    // what made the last build unflashable without forcing BSL).
    for _ in 0..100_000u32 {
        if opa.stat().read().rdy() {
            break;
        }
    }
}

/// ADC1 channel 1 (PA16 = OPA1_OUT), event-triggered single conversions:
/// each TIMG6 zero-event (via the shared EVT_CH_ADC1 fabric channel) starts
/// one conversion into MEMRES0, which the DMA then drains. Register-for-
/// register match of
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/sampler.rs's
/// `init_adc` (confirmed working on this same silicon), minus the second
/// ADC and the hardware averaging it doesn't need here.
fn init_adc1_event() {
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

    // REPEATSINGLE keeps ENC set across conversions (plain SINGLE hardware-
    // clears ENC after the first result); TRIGGER_NEXT below makes each
    // repeat wait for its own event instead of free-running.
    regs.ctl1().modify(|w| {
        w.set_conseq(vals::Conseq::REPEATSINGLE);
        w.set_sampmode(vals::Sampmode::AUTO);
        w.set_trigsrc(vals::Trigsrc::EVENT);
    });
    regs.ctl2().modify(|w| {
        w.set_startadd(0);
        w.set_endadd(0);
        w.set_res(vals::Res::BIT_12);
        w.set_df(false);
        // One converted sample per DMA trigger (0 = no trigger at all).
        w.set_sampcnt(vals::Sampcnt::from_bits(1));
        // Master DMA-request enable for this ADC (TI's DL_ADC12_enableDMA).
        // Separate from the DMA_TRIG.IMASK bit below -- that one only
        // selects *which* MEM-result-loaded event feeds the trigger; this
        // one is the switch that lets the ADC assert its DMA request line
        // at all. Missing this was exactly why conversions completed fine
        // (MEMRES0 held a real value) but the DMA channel never saw a
        // single trigger (SZ never moved off 800).
        w.set_dmaen(true);
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

    // Subscribe the trigger to the timer's event channel.
    regs.fsub().write(|w| w.set_chanid(EVT_CH_ADC1));
    // MEMRES0 loaded -> DMA trigger.
    regs.dma_trig(0)
        .imask()
        .modify(|w| w.set_memresifg(0, true));

    // Enable conversions (they only run when events arrive).
    regs.ctl0().modify(|w| w.set_enc(true));
}

/// TIMG6: 32 MHz down-counter publishing its zero event on EVT_CH_ADC1
/// every 1/4000 s. Same register sequence as
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/sampler.rs's
/// `init_timer` (confirmed working on this same silicon), just one
/// publisher channel instead of two.
fn init_timer() {
    use pac::tim::vals;
    let tim = pac::TIMG6;

    tim.gprcm(0).rstctl().write(|w| {
        w.set_resetstkyclr(true);
        w.set_resetassert(true);
        w.set_key(vals::ResetKey::KEY);
    });
    tim.gprcm(0).pwren().write(|w| {
        w.set_enable(true);
        w.set_key(vals::PwrenKey::KEY);
    });
    cortex_m::asm::delay(16);

    tim.clksel().modify(|w| w.set_busclk_sel(true));
    tim.clkdiv().modify(|w| w.set_ratio(0));
    tim.commonregs(0).cclkctl().modify(|w| w.set_clken(true));

    tim.counterregs(0).load().write_value(LOAD);
    tim.counterregs(0).ctrctl().modify(|w| {
        w.set_cm(vals::Cm::DOWN);
        w.set_repeat(vals::Repeat::REPEAT_1);
        w.set_cvae(vals::Cvae::LDVAL);
        // CZC/CAC/CLC reset to a *reserved* 0x7 on some timers, which stops
        // the counter from advancing at all. Must be set to CCTL0 (0).
        w.set_czc(vals::CxC::CCTL0);
        w.set_cac(vals::CxC::CCTL0);
        w.set_clc(vals::CxC::CCTL0);
        w.set_en(false);
    });

    tim.evt_mode()
        .modify(|w| w.set_evt_cfg(1, vals::EvtCfg::HARDWARE));
    tim.gen_event(0).imask().modify(|w| w.set_z(true));
    tim.fpub(0).write(|w| w.set_chanid(EVT_CH_ADC1));
}

/// TODO: calibrate against a reference ammeter -- see
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/dsp.rs's `Calib`
/// for the calibration procedure/pattern this should follow. Placeholder
/// (1 LSB RMS == 1 A) until real reference readings exist.
const CAL_A_PER_LSB: f32 = 0.003433733774564919;

/// RMS over one captured frame, in raw ADC-LSB units (DC bias removed
/// dynamically -- the OPA stage biases the signal to ~1.7 V, not 0 V, so
/// the raw counts are never centered on zero). u64 sum-of-squares
/// accumulator avoids float rounding error across 800 additions, matching
/// STRUCTURE.md's "RMS u64" box. Ported from
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/dsp.rs::compute.
fn rms_lsb(buf: &[u32; N]) -> f32 {
    let mean = (buf.iter().map(|&x| x as u64).sum::<u64>() / N as u64) as i32;
    let mut sum_sq: u64 = 0;
    for &x in buf.iter() {
        let d = (x & 0xFFF) as i32 - mean;
        sum_sq += (d * d) as u64;
    }
    sqrtf(sum_sq as f32 / N as f32)
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    set_analog(PINCM_PA18);
    set_analog(PINCM_PA16);
    setup_opa1_pga(Gain::X4);
    init_adc1_event();
    init_timer();

    let mut dma = Channel::new(p.DMA_CH0, Irqs);
    // SAFETY: sole owner of the OLED's fixed pins (PB2/PB3) -- nothing
    // else in this firmware uses them. Created once, outside the loop --
    // re-running the full SSD1306 init sequence every measurement would
    // flicker the screen and waste time on the bit-banged I2C bus.
    let mut display = unsafe { Oled::new() };

    let mut led = Output::new(unsafe { peripherals::PA0::steal() }, Level::Low);
    led.set_inversion(true);

    loop {
        // Stop the timer before re-arming so every frame starts aligned
        // on sample 0 (mirrors
        // ~/embed/Single-Phase-Power-Analyzer's Sampler::capture()).
        pac::TIMG6
            .counterregs(0)
            .ctrctl()
            .modify(|w| w.set_en(false));

        // DMAEN auto-clears when DMASZ decrements to zero (TRM 5.2.3.1)
        // -- must be re-armed before every capture, not just the first.
        pac::ADC1.ctl2().modify(|w| w.set_dmaen(true));

        // SAFETY: BUF outlives this transfer (waited on to completion
        // right below, before anything else touches it), and MEMRES0 is
        // a static peripheral register valid for the transfer's whole
        // lifetime.
        let mut xfer = unsafe {
            dma.read(
                DMA_TRIG_ADC1,
                pac::ADC1.memres(0).as_ptr() as *mut u32,
                &mut *core::ptr::addr_of_mut!(BUF),
                TransferOptions::default(),
            )
            .unwrap()
        };

        // First timer zero event starts the first conversion; the DMA
        // then drains one MEMRES0 result per event until all 800 are in.
        pac::TIMG6
            .counterregs(0)
            .ctrctl()
            .modify(|w| w.set_en(true));

        // Bounded wait -- same principle as setup_opa1_pga's RDY wait:
        // never block forever on external hardware. Busy-polling keeps
        // the CPU awake (and SWD reachable) even if a capture never
        // completes, rather than `.await`, which would let the
        // executor's idle loop put the CPU in WFE and (on this MCU+probe
        // combo) block SWD entirely.
        //
        // Checks DMASZ directly, NOT `xfer.is_running()` (req() &&
        // en()). Per the TRM (5.3, DMACTL[j].DMAREQ): "Software-
        // controlled DMA start. DMAREQ is reset automatically" -- REQ is
        // a one-shot kickoff pulse for hardware-triggered channels, not
        // an in-progress flag, so it self-clears almost immediately
        // after arming while EN and the real hardware-triggered transfer
        // keep going. DMASZ == 0 is the TRM-documented actual completion
        // signal for single transfer mode.
        //
        // Timed in cycles, not poll iterations -- a bare iteration-count
        // loop is uncalibratable (each poll is a volatile MMIO read plus
        // a fence, whose real cost on this core is opaque). 800 samples
        // at 4 kHz only need 200 ms if the chain works, so 1000 checks x
        // ~1 ms apart gives 5x margin and a predictable ~1 s cap.
        let mut captured = false;
        for _ in 0..1000u32 {
            if pac::DMA.chan(0).sz().read().size() == 0 {
                captured = true;
                break;
            }
            cortex_m::asm::delay(32_000); // ~1 ms at 32 MHz
        }
        if !captured {
            xfer.request_pause();
        }

        // Capture done; BUF now holds 800 hardware-timed samples of
        // OPA1_OUT at 4 kHz (TIM+ADC+DMA, zero CPU involvement per
        // sample).
        // SAFETY: the DMA transfer above either completed or was
        // paused, so nothing else touches BUF concurrently with this
        // read.
        let original_rms = unsafe { rms_lsb(&*core::ptr::addr_of!(BUF)) };
        let amps = original_rms * CAL_A_PER_LSB;

        if let Ok(display) = &mut display {
            let _ = display.clear(BinaryColor::Off);
            let style = MonoTextStyle::new(&FONT_9X15, BinaryColor::On);
            let mut line: String<32> = String::new();
            let _ = write!(line, "{:.3} A", amps);
            let _ = Text::new(&line, Point::new(4, 30), style).draw(display);

            let mut line2: String<32> = String::new();
            let _ = write!(line2, "orig{:.3} A", original_rms);
            let _ = Text::new(&line2, Point::new(4, 60), style).draw(display);
            let _ = display.flush();
        }

        // Heartbeat: one toggle per measurement cycle.
        // led.toggle();
    }
}
