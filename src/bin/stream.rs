#![no_std]
#![no_main]
// The shared modules carry items this binary does not use -- it has no panel,
// no button and no bench report -- and every one of them would otherwise be a
// dead_code warning here.
#![allow(dead_code)]

//! Free-running meter: measure, transmit, sleep the analog, repeat.
//!
//! Same measurement as `main.rs` -- literally the same `meter::measure`, so a
//! CAL table filled from one binary is valid in the other -- but triggered by a
//! timer instead of S2 and reported out UART2 to the HC-42 instead of onto the
//! OLED. Between readings the analog blocks are powered down exactly as they
//! are there; what stays up is the radio, because a module that has lost power
//! cannot be woken over the air.
//!
//! Two lines go out per reading. `METER_TEST` is the frame the Android reader
//! parses (`MeterFrameParser.FRAME_PATTERN`), and its regex anchors at the end
//! of the line, so nothing may be appended to it. `METER_CAL` carries what a
//! calibration run needs and the phone silently ignores: the raw RMS, the gain
//! it was taken at, and the probe's view of the input. `tools/cal_log.py`
//! parses both.

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_mspm0::dma::{self, Channel};
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::mode::Blocking;
use embassy_mspm0::uart::{Config as UartConfig, UartTx};
use embassy_mspm0::{bind_interrupts, peripherals};
use embassy_time::Timer;
use panic_halt as _;

#[path = "../cal.rs"]
mod cal;
#[path = "../dac.rs"]
mod dac;
#[path = "../dsp.rs"]
mod dsp;
#[path = "../led.rs"]
mod led;
#[path = "../meter.rs"]
mod meter;
#[path = "../range.rs"]
mod range;
#[path = "../sampler.rs"]
mod sampler;
#[path = "../vref.rs"]
mod vref;

use meter::{Meter, Quality, Reading};

bind_interrupts!(struct Irqs {
    DMA => dma::InterruptHandler<peripherals::DMA_CH0>;
});

/// Idle between the end of one reading and the start of the next. The analog
/// blocks are down for all of it, so this is the duty cycle: at 250 ms of
/// measurement per reading, one second gives the front end roughly three
/// quarters of its life powered off, and the phone an update rate that still
/// looks live.
const PERIOD_MS: u64 = 1_000;

/// HC-42 settling after its supply comes up, before UART2 exists. The module
/// must not see the idle-high TX line while unpowered -- current would flow
/// back through its RXD protection diode -- so this delay is also what
/// separates "supply up" from "pin driven".
const BT_SETTLE_MS: u64 = 350;

/// HC-42 factory default (`AT+UART`, HC42.pdf 5.3.5). Changing it means
/// changing the module too, and a module that disagrees is silent rather than
/// wrong, which is the failure this constant exists to avoid.
const BT_BAUD_RATE: u32 = 9_600;

/// Alarm thresholds from 2(3), in mA. The phone classifies again from its own
/// preferences; sending our own classification means a meter read by anything
/// else still says what it thinks.
const LOW_LIMIT_MA: u32 = 200;

const HIGH_LIMIT_MA: u32 = 2_000;

/// `METER_TEST` allows 1-7 digits. A reading that would overflow that is
/// nonsense anyway, and a malformed frame is worse than a saturated one: the
/// parser drops it whole, so the fault would look like silence.
const MAX_FRAME_MA: u32 = 9_999_999;

/// `core::fmt` sink on the HC-42's UART, so frames are written with plain
/// `write!` instead of formatting into a `heapless::String` a line at a time.
struct Radio<'d>(UartTx<'d, Blocking>);

impl core::fmt::Write for Radio<'_> {
    /// Translates bare LF to CRLF. `MeterFrameParser` strips the CR itself, but
    /// every serial terminal used to look at this link mid-bench does not.
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for part in s.split_inclusive('\n') {
            match part.strip_suffix('\n') {
                Some(head) => {
                    let _ = self.0.blocking_write(head.as_bytes());
                    let _ = self.0.blocking_write(b"\r\n");
                }
                None => {
                    let _ = self.0.blocking_write(part.as_bytes());
                }
            }
        }
        Ok(())
    }
}

/// The four-position coded switch, ON to ground against internal pull-ups, so
/// a closed switch reads low and contributes its weight.
///
/// Read per frame rather than latched at boot: the address is meant to be set
/// on the bench with the meter running, and a value that only takes effect
/// after a power cycle is a value that gets set wrong once and stays wrong.
struct Address {
    bit0: Input<'static>,
    bit1: Input<'static>,
    bit2: Input<'static>,
    bit3: Input<'static>,
}

impl Address {
    fn read(&self) -> u8 {
        (self.bit0.is_low() as u8)
            | (self.bit1.is_low() as u8) << 1
            | (self.bit2.is_low() as u8) << 2
            | (self.bit3.is_low() as u8) << 3
    }
}

fn milliamps(reading: &Reading) -> u32 {
    // f32 -> u32 saturates at both ends in Rust, so a negative RMS (which the
    // CAL extrapolation can produce below the table's first point) lands on 0
    // rather than wrapping to something enormous.
    ((reading.amps * 1000.0) as u32).min(MAX_FRAME_MA)
}

fn status(milliamps: u32) -> &'static str {
    if milliamps < LOW_LIMIT_MA {
        "LOW"
    } else if milliamps > HIGH_LIMIT_MA {
        "HIGH"
    } else {
        "NORMAL"
    }
}

fn flag(quality: Quality) -> &'static str {
    match quality {
        Quality::RefBad => "REF_BAD",
        Quality::InputBad => "BAD_INPUT",
        Quality::OverRange => "OVER",
        Quality::Incomplete => "PARTIAL",
        Quality::Good => "OK",
    }
}

/// One reading, on the wire.
///
/// `METER_TEST` is withheld when the reading is one the meter cannot stand
/// behind, because that frame has nowhere to say so -- its STATUS field is the
/// alarm classification, and sending a fiction as NORMAL is worse than sending
/// nothing: a reader that stops hearing from a meter shows it offline, which is
/// true, while a confident wrong number is not.
///
/// `OverRange` is the exception. It means the signal will not fit even at unity
/// gain, which on this front end is a current past the top of scale -- the
/// number is understated but the direction is known, and that is exactly the
/// case 2(3) wants alarmed. It goes out as HIGH.
///
/// `METER_CAL` always goes out, including for the withheld cases. A frame the
/// bench cannot use is still evidence, and it carries its own FLAG.
fn transmit(radio: &mut Radio<'_>, addr: u8, reading: &Reading) {
    let quality = reading.quality();
    let ma = milliamps(reading);

    let test_status = match quality {
        Quality::Good => Some(status(ma)),
        Quality::OverRange => Some("HIGH"),
        Quality::RefBad | Quality::InputBad | Quality::Incomplete => None,
    };
    if let Some(test_status) = test_status {
        let _ = writeln!(
            radio,
            "METER_TEST,ADDR={},CURRENT_MA={},STATUS={}",
            addr, ma, test_status
        );
    }

    let _ = write!(
        radio,
        "METER_CAL,ADDR={},RMS={:.2},GAIN={},FLAG={}",
        addr,
        reading.rms,
        reading.range.nominal(),
        flag(quality)
    );
    // Omitted rather than zeroed when the probe frame never drained: a mean of
    // 0 is a legal reading, and a bench that cannot tell it from "no probe"
    // would chase the wrong fault.
    if let Some((mean_in, pp_in)) = reading.probe {
        let _ = write!(radio, ",MEAN={},PP={}", mean_in, pp_in);
    }
    let _ = writeln!(radio);
}

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    if let Ok(token) = led::blink(Output::new(p.PA0, Level::Low)) {
        spawner.spawn(token);
    }

    // Active-high enable for the HC-42's load switch, held down through boot.
    // It needs an external pull-down for the window before this line, or the
    // radio powers up during flashing and reset (see the board notes, 6.1).
    let mut bt_power = Output::new(p.PB17, Level::Low);

    let address = Address {
        bit0: Input::new(p.PB0, Pull::Up),
        bit1: Input::new(p.PB6, Pull::Up),
        bit2: Input::new(p.PB7, Pull::Up),
        bit3: Input::new(p.PB8, Pull::Up),
    };

    let mut meter = Meter::new(p.PA8, p.PA26);
    let mut dma = Channel::new(p.DMA_CH0, Irqs);

    // The first reading is taken with the radio still dark. Its start-up
    // current is the largest single draw in this firmware and it lands on a
    // harvested rail that has just come up; putting the measurement first
    // means the two never contend, and by the time the module powers on there
    // is already something to say.
    let mut reading = meter.measure(&mut dma).await;

    bt_power.set_high();
    Timer::after_millis(BT_SETTLE_MS).await;

    // TX only: this meter reports and takes no commands, so PB16 stays free.
    let mut uart_config = UartConfig::default();
    uart_config.baudrate = BT_BAUD_RATE;
    let mut radio = Radio(UartTx::new_blocking(p.UART2, p.PB15, uart_config).unwrap());

    loop {
        transmit(&mut radio, address.read(), &reading);
        Timer::after_millis(PERIOD_MS).await;
        reading = meter.measure(&mut dma).await;
    }
}
