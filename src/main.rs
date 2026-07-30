#![no_std]
#![no_main]

//! "Passive" AC ammeter, circuit A + B: measure the harvested CT signal,
//! show it on demand, and push it out over the HC-42. The OLED stays inactive
//! until S2 toggles it on, while the HC-42 is kept off during the first
//! measurement so the harvesting supply only has to start the analog and
//! compute path.
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
use dsp::{coarse_pivot, rms_lsb};
use oled::Oled;
use range::{DAC_MAX, Gain, Range, apply_range, next_range, probe_stats, setup_opa1_pga};
use sampler::{
    ADC_CH_RAW_IN, BUF, PINCM_PA16, PINCM_PA18, PROBE_BUF, capture_with_poll, init_adc1_event,
    init_timer, set_adc_channel, set_analog,
};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

const HC42_BAUD_RATE: u32 = 9_600;
/// HC-42 manual: allow at least 350 ms after power-on before using its UART.
const HC42_POWER_UP_MS: u64 = 350;
const DISPLAY_BUTTON_RELEASE_DEBOUNCE_MS: u8 = 20;

struct ToggleButton {
    armed: bool,
    released_ms: u8,
    toggle_pending: bool,
}

impl ToggleButton {
    const fn new() -> Self {
        Self {
            armed: true,
            released_ms: DISPLAY_BUTTON_RELEASE_DEBOUNCE_MS,
            toggle_pending: false,
        }
    }

    fn poll(&mut self, pressed: bool) {
        if pressed {
            self.released_ms = 0;
            if self.armed {
                // Keep parity so two complete presses before the measurement
                // frame ends still produce two state transitions.
                self.toggle_pending = !self.toggle_pending;
                self.armed = false;
            }
        } else if !self.armed {
            self.released_ms = self
                .released_ms
                .saturating_add(1)
                .min(DISPLAY_BUTTON_RELEASE_DEBOUNCE_MS);
            if self.released_ms == DISPLAY_BUTTON_RELEASE_DEBOUNCE_MS {
                self.armed = true;
            }
        }
    }

    fn take_toggle(&mut self) -> bool {
        let pending = self.toggle_pending;
        self.toggle_pending = false;
        pending
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    // PB17 drives the active-high EN input of an external 3.3 V load switch;
    // it must never drive the HC-42 VCC pin directly. A physical pull-down on
    // EN keeps the module off before this firmware takes control of PB17.
    //
    // Keep UART2 uninitialized while power is off as well. Otherwise its TX
    // idle-high level could feed the unpowered module through RXD's protection
    // diode and defeat the power gate.
    let mut hc42_power_enable = Output::new(p.PB17, Level::Low);
    let mut hc42_peripherals = Some((p.UART2, p.PB16, p.PB15));
    let mut hc42 = None;

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
    // `None` deliberately leaves PB2/PB3 in their reset state during the
    // first probe, capture and calculation. `Some(Err(_))` records a failed
    // first initialization so fixed pins are never stolen a second time.
    let mut display: Option<Result<Oled, ()>> = None;

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
    // LaunchPad S2 is the independent user button on PB21. It pulls the pin
    // low when pressed; unlike S1/PA18, it does not disturb the ADC input.
    let display_button = Input::new(p.PB21, Pull::Up);

    let mut range = Range::Pga(Gain::X2);
    let mut display_enabled = false;
    let mut display_toggle_button = ToggleButton::new();

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
        let probed = capture_with_poll(
            &mut dma,
            unsafe { &mut *core::ptr::addr_of_mut!(PROBE_BUF) },
            || display_toggle_button.poll(display_button.is_low()),
        );
        if probed {
            // SAFETY: as above -- the transfer is finished or paused.
            let (mean_in, pp_in, railed) = probe_stats(unsafe { &*core::ptr::addr_of!(PROBE_BUF) });
            if !railed {
                range = next_range(range, mean_in, pp_in);
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
        let _ = capture_with_poll(
            &mut dma,
            unsafe { &mut *core::ptr::addr_of_mut!(BUF) },
            || display_toggle_button.poll(display_button.is_low()),
        );

        // BUF now holds 800 hardware-timed samples of OPA1_OUT at 4 kHz
        // (TIM+ADC+DMA, zero CPU involvement per sample).
        // SAFETY: as above -- the transfer is finished or paused, so BUF is
        // ours for the rest of this iteration.
        let buf = unsafe { &*core::ptr::addr_of!(BUF) };
        let pivot = coarse_pivot(buf);
        let rms = rms_lsb(buf, pivot);
        let amps = lsb_to_amps(rms, range);

        if display_toggle_button.take_toggle() {
            display_enabled = !display_enabled;
            if !display_enabled && let Some(Ok(display)) = &mut display {
                let _ = display.set_display_on(false);
            }
        }

        if display_enabled {
            // PB2/PB3 and the SSD1306 remain completely untouched until S2
            // is first pressed after a complete measurement is available.
            if display.is_none() {
                // SAFETY: PB2/PB3 have not been used above and this branch
                // executes once because both Ok and Err are retained.
                display = Some(unsafe { Oled::new() });
            }

            if let Some(Ok(display)) = &mut display {
                let _ = display.clear(BinaryColor::Off);
                let style = MonoTextStyle::new(&FONT_9X15, BinaryColor::On);
                let mut line: String<32> = String::new();
                let _ = write!(line, "{:.3} A", amps);
                let _ = Text::new(&line, Point::new(32, 38), style).draw(display);
                let _ = display.flush();
                let _ = display.set_display_on(true);
            }
        }

        // Power the HC-42 only after the first measurement. OLED operation is
        // independently gated by S2 and does not delay wireless availability.
        // The module stays on afterwards because a BLE peripheral must remain
        // discoverable for the reader to decide when it is needed.
        if hc42.is_none() {
            hc42_power_enable.set_high();
            Timer::after_millis(HC42_POWER_UP_MS).await;

            let (uart2, rx, tx) = hc42_peripherals
                .take()
                .expect("HC-42 UART peripherals can only be initialized once");
            let mut uart_config = UartConfig::default();
            uart_config.baudrate = HC42_BAUD_RATE;
            hc42 = Some(Uart::new_blocking(uart2, rx, tx, uart_config).unwrap());
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
        if let Some(hc42) = &mut hc42 {
            let _ = hc42.blocking_write(frame.as_bytes());
            let _ = hc42.blocking_flush();
            tx_green_led.toggle();
        }

        // Heartbeat: one toggle per measurement cycle.
        // led.toggle();
    }
}
