#![no_std]
#![no_main]

//! "Passive" AC ammeter, circuit A + B: measure the harvested CT signal,
//! show it, and push it out over the HC-42.
//!
//! This file is the wiring and the measurement period; the substance lives in
//! the four modules below.
//!   `sampler` -- TIMG6/ADC1/DMA and the sample buffers
//!   `range`   -- OPA1 PGA, DAC bias pivot, and the autoranger
//!   `dsp`     -- windowing, true RMS, mains frequency, spread
//!   `cal`     -- per-range LSB -> amps tables

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_mspm0::bind_interrupts;
use embassy_mspm0::dma::{self, Channel};
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::peripherals;
use embassy_mspm0::uart::{Config as UartConfig, Uart};
use embassy_time::Timer;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_9X15;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use heapless::String;
use panic_halt as _;

mod cal;
mod dac;
mod dsp;
mod oled;
mod range;
mod sampler;

use cal::lsb_to_amps;
use dsp::{Spread, coarse_pivot, estimate_hz, rms_lsb};
use oled::Oled;
use range::{
    DAC_MAX, Gain, Range, apply_range, next_range, over_range, probe_stats, setup_opa1_pga,
};
use sampler::{
    ADC_CH_RAW_IN, BUF, PINCM_PA16, PINCM_PA18, PROBE_BUF, capture, init_adc1_event, init_timer,
    set_adc_channel, set_analog,
};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

const HC42_BAUD_RATE: u32 = 9_600;

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    set_analog(PINCM_PA18);
    set_analog(PINCM_PA16);
    // Starting range only, and deliberately the pessimistic end of the
    // ladder: x2 cannot clip anything a higher step would have handled. The
    // starting DAC code is mid-scale for the same reason -- not because the
    // input is expected there, but because it is the choice that assumes
    // least. Neither survives the first period: the probe reads the input at
    // unity gain, and `next_range`/`dac_code_for` derive both from what it
    // actually measured. Convergence takes one period rather than several,
    // because the probe's numbers are input-referred and so do not depend on
    // which range happens to be selected while it runs.
    setup_opa1_pga(Gain::X2, (DAC_MAX / 2) as u16);
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

    let mut tx_green_led = Output::new(p.PB27, Level::Low);
    tx_green_led.set_inversion(true);

    // Each switch connects its GPIO to GND when ON. The internal pull-up makes
    // an open switch read as 0 and a closed switch read as 1 after inversion.
    let address_bit0 = Input::new(p.PB0, Pull::Up);
    let address_bit1 = Input::new(p.PB6, Pull::Up);
    let address_bit2 = Input::new(p.PB7, Pull::Up);
    let address_bit3 = Input::new(p.PB8, Pull::Up);

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = HC42_BAUD_RATE;
    let mut hc42 = Uart::new_blocking(p.UART2, p.PB16, p.PB15, uart_config).unwrap();

    let mut spread_track = Spread::new();
    let mut range = Range::Pga(Gain::X2);
    let mut input_bad = false;
    let mut range_over = false;

    // HC-42 needs about 300 ms after power-on before it is ready.
    Timer::after_millis(500).await;

    loop {
        // --- Probe: the raw input at unity gain, straight off PA18 ---
        //
        // The OPA is not involved and does not move, so this costs one MEMCTL
        // write and 40 ms, with no settle. Both numbers it yields are
        // input-referred and therefore independent of the gain currently
        // selected, which is what lets the range converge in a single period
        // rather than hunting toward it.
        set_adc_channel(ADC_CH_RAW_IN);
        // SAFETY: `capture` waits the transfer out (or pauses it) before
        // returning, so nothing else touches PROBE_BUF concurrently.
        let probed = capture(&mut dma, unsafe {
            &mut *core::ptr::addr_of_mut!(PROBE_BUF)
        });
        if probed {
            // SAFETY: as above -- the transfer is finished or paused.
            let (mean_in, pp_in, railed) = probe_stats(unsafe { &*core::ptr::addr_of!(PROBE_BUF) });
            input_bad = railed;
            if !railed {
                range = next_range(range, mean_in, pp_in);
                range_over = over_range(range, mean_in, pp_in);
                if apply_range(range, mean_in) {
                    // Settle the DAC and the re-tapped ladder before the frame
                    // that carries the marks. Both are microsecond-scale; 1 ms
                    // is slack, and only 0.4% of the measurement period.
                    // Skipped on Direct, which changes nothing analog.
                    Timer::after_millis(1).await;
                }
            }
        }
        // A failed or railed probe leaves the range exactly as it was.
        // Re-ranging on numbers known to be wrong is strictly worse than
        // keeping a setting that was right one period ago -- and in the
        // railed case the reading is flagged rather than silently trusted,
        // because an input outside `0..VDDA` is an analog front-end problem
        // that no range inside this chip can fix.

        // --- Measurement frame: whatever the chosen range put the signal on
        // (OPA1_OUT on PA16 for the PGA ranges, PA18 itself for Direct, in
        // which case the channel is already the one the probe just used) ---
        // SAFETY: `capture` waits the transfer out (or pauses it) before
        // returning, so nothing else touches BUF concurrently.
        let framed = capture(&mut dma, unsafe { &mut *core::ptr::addr_of_mut!(BUF) });

        // BUF now holds 800 hardware-timed samples of OPA1_OUT at 4 kHz
        // (TIM+ADC+DMA, zero CPU involvement per sample).
        // SAFETY: as above -- the transfer is finished or paused, so BUF is
        // ours for the rest of this iteration.
        let buf = unsafe { &*core::ptr::addr_of!(BUF) };
        let pivot = coarse_pivot(buf);
        // One capture, one reading -- no cross-frame state. See `Spread`.
        let rms = rms_lsb(buf, pivot);
        let spread = spread_track.push(rms);
        let hz = estimate_hz(buf, pivot);
        let amps = lsb_to_amps(rms, range);

        if let Ok(display) = &mut display {
            let _ = display.clear(BinaryColor::Off);
            let style = MonoTextStyle::new(&FONT_9X15, BinaryColor::On);

            // A leading marker means the reading below it is not to be
            // believed, and says which of the two ways it went wrong:
            //   '!' the probe found the input pinned at an ADC end, so the
            //       signal is leaving `0..VDDA` and no internal gain or bias
            //       choice can recover it -- that needs external conditioning.
            //   '>' the arithmetic says the signal will not fit even on
            //       Direct, the least demanding range there is, so the frame
            //       is clipping. Every range is bounded by the supply and
            //       nothing tighter (see range::OUT_LO -- OPA1's rails and
            //       ADC1's full scale are the same two voltages), which makes
            //       this exactly the same physical condition as '!', predicted
            //       from the probe's mean/pp rather than seen as pinned
            //       samples. Adding Direct to the ladder is what made it
            //       near-unreachable; it is kept as the arithmetic backstop.
            //   '?' the measurement frame's DMA never drained, so BUF is part
            //       new samples and part stale ones.
            // All three sit on the primary line on purpose: a number this
            // display cannot stand behind should not look like the others.
            let marker = if input_bad {
                "!"
            } else if range_over {
                ">"
            } else if !framed {
                "?"
            } else {
                ""
            };
            let mut line: String<32> = String::new();
            let _ = write!(line, "{}{:.3} A", marker, amps);
            let _ = Text::new(&line, Point::new(4, 20), style).draw(display);

            // Frequency, plus peak-to-peak spread of the last 16 frames in
            // LSB -- read this to find out whether noise is actually a
            // problem on this board. 0.5% at 0.1 A is 0.146 LSB, so a spread
            // much above ~0.3 means a longer aperture is needed.
            let mut line2: String<32> = String::new();
            let _ = write!(line2, "{:.2}Hz p{:.2}", hz, spread);
            let _ = Text::new(&line2, Point::new(4, 40), style).draw(display);

            // Raw single-frame RMS in LSB and the gain it was taken at --
            // together they are one row of one CAL table, and the gain says
            // *which* table, so the calibration cannot be entered against the
            // wrong one. Two decimals: 0.01 LSB is 0.03% at 0.1 A, so the
            // display resolution is not what limits the calibration.
            //
            // Starts at x=0, not x=4 like the others: 14 characters of
            // FONT_9X15 is exactly 126 px, so the margin is the difference
            // between fitting and losing the last digit of the gain.
            let mut line3: String<32> = String::new();
            let _ = write!(line3, "rms {:.2} x{}", rms, range.nominal());
            let _ = Text::new(&line3, Point::new(0, 60), style).draw(display);

            let _ = display.flush();
        }

        // Address dip switches, read once per cycle -- the Android app
        // re-derives alarm status from CURRENT_MA itself (see
        // MeterReading::classify), so STATUS here is just a fixed token
        // that satisfies the app's frame parser.
        let address = u8::from(address_bit0.is_low())
            | (u8::from(address_bit1.is_low()) << 1)
            | (u8::from(address_bit2.is_low()) << 2)
            | (u8::from(address_bit3.is_low()) << 3);
        let current_ma = (amps * 1000.0) as u32;

        let mut frame: String<64> = String::new();
        let _ = write!(
            frame,
            "METER_TEST,ADDR={:02},CURRENT_MA={},STATUS=NORMAL\r\n",
            address, current_ma
        );
        let _ = hc42.blocking_write(frame.as_bytes());
        let _ = hc42.blocking_flush();
        tx_green_led.toggle();

        // Heartbeat: one toggle per measurement cycle.
        // led.toggle();
    }
}
