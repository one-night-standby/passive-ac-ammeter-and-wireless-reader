//! Hardware sampling chain: TIMG6 -> ADC1 -> DMA, and the buffers it
//! fills. Knows nothing about what the samples mean or which range produced
//! them -- see `range` for the analog front end and `dsp` for the maths.

use embassy_mspm0::dma::{Channel, TransferOptions};
use embassy_mspm0::pac;

/// Event fabric channel TIMG6 publishes its zero event on and ADC1
/// subscribes to. Free pick -- only one producer/consumer pair exists in
/// this firmware so far. (This is a *different* namespace from the DMA
/// trigger ID below: this one is the generic 16-channel event crossbar,
/// the other is ADC1's own hardwired "result loaded" line into the DMA.)
pub const EVT_CH_ADC1: u8 = 1;

/// ADC1's dedicated hardwired DMA request line (G350x datasheet:
/// DMA_ADC1_EVT_GEN_BD_TRIG). Confirmed against
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/sampler.rs, which
/// runs this exact TIM+ADC+DMA chain on the same silicon.
pub const DMA_TRIG_ADC1: u8 = 24;

pub const TIMER_CLK_HZ: u32 = 32_000_000; // BUSCLK
/// STRUCTURE.md: 4k samples/sec, 800 points (10 cycles of 50 Hz mains).
pub const SAMPLE_HZ: u32 = 4_000;

pub const LOAD: u32 = TIMER_CLK_HZ / SAMPLE_HZ - 1;

pub const PINCM_PA18: usize = 40; // OPA1_IN1+ (signal in)
pub const PINCM_PA16: usize = 38; // OPA1_OUT (also ADC1 channel 1, signal out)

/// ADC1 channel numbers for the two pins above, taken from the PAC's own pin
/// table (`impl_adc_pin!(ADC1, PA16, 1u8)`, `impl_adc_pin!(ADC1, PA18, 3u8)`).
///
/// PA18 being an ADC channel *in its own right* is what makes the unity-gain
/// probe free: the raw input can be sampled without touching the OPA at all,
/// so the probe costs one MEMCTL write instead of an OPA disable/re-enable.
/// (True x1 is not reachable through the PGA ladder -- TRM Table 21-6 lists
/// non-inverting GAIN=0x0 as "Not valid", the ladder starts at 2x. The only
/// other x1 path is buffer mode, NSEL=RTOP with GAIN=0x0, and TRM 21.2.8
/// requires the full clear-ENABLE/rewrite-CFG/set-ENABLE dance plus an OPA
/// enable wait every time GAIN crosses 0x0.)
pub const ADC_CH_OPA_OUT: u8 = 1; // PA16
pub const ADC_CH_RAW_IN: u8 = 3; // PA18

pub fn set_analog(pincm: usize) {
    pac::IOMUX.pincm(pincm).modify(|w| {
        w.set_pf(0);
        w.set_pipu(false);
        w.set_pipd(false);
    });
}

pub const N: usize = 800;

pub static mut BUF: [u32; N] = [0; N];

/// Unity-gain probe frame: 160 samples = 40 ms = two nominal mains cycles.
/// Only mean and peak-to-peak come out of it, and both are settled after one
/// cycle, so a second cycle is already margin. Kept short because it is pure
/// overhead on every measurement period, and startup time is a scored item.
pub const PROBE_N: usize = 160;

/// Separate storage rather than the head of `BUF`. Sharing would be 640 bytes
/// cheaper but creates a real failure mode: if the *main* capture times out,
/// the first 160 words of BUF would still hold unamplified probe samples
/// spliced onto the previous frame's tail, and the DSP would report a
/// confident, entirely fictional number instead of a stale one.
pub static mut PROBE_BUF: [u32; PROBE_N] = [0; PROBE_N];

/// ADC1 result width. MEMRES is 12-bit right-aligned; masking is defensive,
/// but it has to be applied *consistently* -- the previous `rms_lsb` summed
/// unmasked words for its mean and then subtracted that from masked ones.
pub const ADC_MASK: u32 = 0xFFF;

/// Point ADC1's single conversion at a different input pin.
///
/// MEMCTL is rewritten with ENC dropped: the conversion sequence must not be
/// live while its own channel select moves out from under it. Costs two
/// register writes, which is the whole reason the probe reads PA18 directly
/// instead of putting the OPA into buffer mode.
pub fn set_adc_channel(ch: u8) {
    let regs = pac::ADC1;
    regs.ctl0().modify(|w| w.set_enc(false));
    regs.memctl(0).modify(|w| w.set_chansel(ch));
    regs.ctl0().modify(|w| w.set_enc(true));
}

/// ADC1 channel 1 (PA16 = OPA1_OUT), event-triggered single conversions:
/// each TIMG6 zero-event (via the shared EVT_CH_ADC1 fabric channel) starts
/// one conversion into MEMRES0, which the DMA then drains. Register-for-
/// register match of
/// ~/embed/Single-Phase-Power-Analyzer/src/bin/analyzer/sampler.rs's
/// `init_adc` (confirmed working on this same silicon), minus the second
/// ADC and the hardware averaging it doesn't need here.
pub fn init_adc1_event() {
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
pub fn init_timer() {
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

/// One hardware-timed capture of `dst.len()` samples from whichever channel
/// `set_adc_channel` last selected. TIM+ADC+DMA, zero CPU involvement per
/// sample. Returns false if the transfer never drained, in which case `dst`
/// holds a mix of new and stale samples and must not be interpreted.
///
/// Factored out of the main loop so the probe and the measurement frame run
/// the identical arming sequence -- the pieces that are easy to drop (the
/// timer stop, the DMAEN re-arm) are exactly the ones that fail silently.
pub fn capture(dma: &mut Channel<'_>, dst: &mut [u32]) -> bool {
    // Read the length before `dst` is handed to the DMA. Deriving the timeout
    // from DMASZ instead would read a counter the hardware has already begun
    // decrementing, shortening the budget by however long the arming took.
    let len = dst.len() as u32;

    // Stop the timer before re-arming so every frame starts aligned on
    // sample 0 (mirrors ~/embed/Single-Phase-Power-Analyzer's
    // Sampler::capture()).
    pac::TIMG6
        .counterregs(0)
        .ctrctl()
        .modify(|w| w.set_en(false));

    // DMAEN auto-clears when DMASZ decrements to zero (TRM 5.2.3.1) -- must
    // be re-armed before every capture, not just the first.
    pac::ADC1.ctl2().modify(|w| w.set_dmaen(true));

    // SAFETY: the transfer is waited on to completion (or paused) before this
    // function returns, so `dst` is not touched concurrently, and MEMRES0 is
    // a static peripheral register valid for the transfer's whole lifetime.
    let mut xfer = unsafe {
        dma.read(
            DMA_TRIG_ADC1,
            pac::ADC1.memres(0).as_ptr() as *mut u32,
            dst,
            TransferOptions::default(),
        )
        .unwrap()
    };

    // First timer zero event starts the first conversion; the DMA then drains
    // one MEMRES0 result per event until every sample is in.
    pac::TIMG6
        .counterregs(0)
        .ctrctl()
        .modify(|w| w.set_en(true));

    // Bounded wait -- same principle as setup_opa1_pga's RDY wait: never
    // block forever on external hardware. Busy-polling keeps the CPU awake
    // (and SWD reachable) even if a capture never completes, rather than
    // `.await`, which would let the executor's idle loop put the CPU in WFE
    // and (on this MCU+probe combo) block SWD entirely.
    //
    // Checks DMASZ directly, NOT `xfer.is_running()` (req() && en()). Per the
    // TRM (5.3, DMACTL[j].DMAREQ): "Software-controlled DMA start. DMAREQ is
    // reset automatically" -- REQ is a one-shot kickoff pulse for
    // hardware-triggered channels, not an in-progress flag, so it self-clears
    // almost immediately after arming while EN and the real
    // hardware-triggered transfer keep going. DMASZ == 0 is the
    // TRM-documented actual completion signal for single transfer mode.
    //
    // Timed in cycles, not poll iterations -- a bare iteration-count loop is
    // uncalibratable (each poll is a volatile MMIO read plus a fence, whose
    // real cost on this core is opaque). The budget is 5x the frame's own
    // duration, so it scales with the frame instead of being a constant that
    // silently becomes a 25x wait for the 40 ms probe.
    let budget_ms = len * 5 / (SAMPLE_HZ / 1000);
    let mut captured = false;
    for _ in 0..budget_ms {
        if pac::DMA.chan(0).sz().read().size() == 0 {
            captured = true;
            break;
        }
        cortex_m::asm::delay(32_000); // ~1 ms at 32 MHz
    }
    if !captured {
        xfer.request_pause();
    }
    captured
}
