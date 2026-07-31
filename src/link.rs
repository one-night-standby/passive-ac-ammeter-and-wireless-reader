//! The wireless link: HC-42 on UART2, the address switch that names this
//! meter, the frames that go out and the one command that comes in.
//!
//! Shared by both binaries so there is exactly one wire format. `main.rs`
//! keeps the link up and measures when told to; `bin/stream.rs` keeps it up and
//! measures on a timer. Neither can drift from the other's grammar.

use core::fmt::Write as _;

use embassy_mspm0::Peri;
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::interrupt::typelevel::Binding;
use embassy_mspm0::peripherals;
use embassy_mspm0::uart::{BufferedInterruptHandler, BufferedUart, Config as UartConfig, Instance};
use embassy_time::Timer;
use embedded_io_async::{Read, ReadReady, Write};

use crate::meter::{Quality, Reading};

/// HC-42 settling after its supply comes up, before UART2 exists. The module
/// must not see the idle-high TX line while unpowered -- current would flow
/// back through its RXD protection diode -- so this delay is also what
/// separates "supply up" from "pin driven".
const BT_SETTLE_MS: u64 = 350;

/// Not the HC-42's factory 9600: at that rate one reading's two frames are 110
/// bytes and 115 ms of pure wire time, which lands directly in how long a
/// reader waits for an answer. 115200 makes the same frames 9.6 ms and costs
/// nothing else -- the module supports up to 230400 (HC42.pdf 5.3.5).
///
/// **The module must be set to match**, with `AT+UART=115200`; `bin/btcfg.rs`
/// does it and reads the value back. A module left at 9600 is not slow, it is
/// silent, which is the failure this constant exists to make visible: change it
/// here and the meter goes mute until the module is changed too.
pub const BT_BAUD_RATE: u32 = 115_200;

/// Alarm thresholds from 2(3), in mA. The phone classifies again from its own
/// preferences; sending our own classification means a meter read by anything
/// else still says what it thinks.
const LOW_LIMIT_MA: u32 = 200;

const HIGH_LIMIT_MA: u32 = 2_000;

/// `METER_TEST` allows 1-7 digits. A reading that would overflow that is
/// nonsense anyway, and a malformed frame is worse than a saturated one: the
/// parser drops it whole, so the fault would look like silence.
const MAX_FRAME_MA: u32 = 9_999_999;

/// Longest command line accepted. Anything longer is a reader that has lost
/// the plot or line noise that happens to lack newlines; either way the buffer
/// resets rather than growing.
const CMD_MAX: usize = 32;

/// Longest line this link emits, CRLF included. `METER_CAL` at its widest --
/// every optional field present and every number at full width -- is 72 bytes.
/// The rest is margin for an `rms` the DSP produced outside the converter's
/// range, which is a number worth reporting rather than reshaping.
const LINE_MAX: usize = 128;

/// How often the meter announces itself while idle. Cheap enough to ignore:
/// one 17-byte line is 18 ms of 9600-baud UART and no analog blocks at all,
/// against the 1.22 mA the HC-42 already draws holding a full-speed connection
/// (HC42.pdf 1.3).
///
/// It is what makes the reader's view of "who is out there" track the coded
/// switch: one meter stands in for 16 addresses, and when the switch moves, the
/// old address simply stops announcing and the new one starts. It also makes a
/// disconnected load visible within a few seconds -- the meter loses power with
/// it, so the announcements just stop -- which is exactly the offline condition
/// of 2(3), and far quicker than waiting for a poll to time out.
pub const HEARTBEAT_MS: u64 = 2_000;

/// What a reader can ask for. One variant today, and the parser is written so
/// that stays cheap to extend.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Take one reading now. Equivalent to a press of S2, including what
    /// happens to a second one that arrives mid-measurement: nothing.
    Measure,
}

/// The four-position coded switch, ON to ground against internal pull-ups, so
/// a closed switch reads low and contributes its weight.
///
/// Read per frame rather than latched at boot: the address is meant to be set
/// on the bench with the meter running, and a value that only takes effect
/// after a power cycle is a value that gets set wrong once and stays wrong.
pub struct Address {
    bit0: Input<'static>,
    bit1: Input<'static>,
    bit2: Input<'static>,
    bit3: Input<'static>,
}

impl Address {
    pub fn new(
        pb0: Peri<'static, peripherals::PB0>,
        pb6: Peri<'static, peripherals::PB6>,
        pb7: Peri<'static, peripherals::PB7>,
        pb8: Peri<'static, peripherals::PB8>,
    ) -> Self {
        Self {
            bit0: Input::new(pb0, Pull::Up),
            bit1: Input::new(pb6, Pull::Up),
            bit2: Input::new(pb7, Pull::Up),
            bit3: Input::new(pb8, Pull::Up),
        }
    }

    pub fn read(&self) -> u8 {
        (self.bit0.is_low() as u8)
            | (self.bit1.is_low() as u8) << 1
            | (self.bit2.is_low() as u8) << 2
            | (self.bit3.is_low() as u8) << 3
    }
}

pub struct Link {
    uart: BufferedUart<'static>,
    /// Partial command line, carried across reads because a 9600-baud line
    /// arrives a few bytes at a time.
    pending: heapless::String<CMD_MAX>,
    _power: Output<'static>,
}

impl Link {
    /// Power the radio and open the port, in that order and with the settle in
    /// between (board notes 6.1). Stays up from here: a module that has lost
    /// power cannot be woken over the air, so the only thing that could turn it
    /// back on is the local button -- which defeats the point of accepting
    /// commands at all.
    ///
    /// PB17 needs an external pull-down for the window before this runs, or the
    /// radio powers up during flashing and reset.
    pub async fn power_up<T: Instance>(
        pb17: Peri<'static, peripherals::PB17>,
        uart: Peri<'static, T>,
        tx: Peri<'static, impl embassy_mspm0::uart::TxPin<T>>,
        rx: Peri<'static, impl embassy_mspm0::uart::RxPin<T>>,
        irq: impl Binding<T::Interrupt, BufferedInterruptHandler<T>>,
        tx_buf: &'static mut [u8],
        rx_buf: &'static mut [u8],
    ) -> Option<Self> {
        let power = Output::new(pb17, Level::High);
        Timer::after_millis(BT_SETTLE_MS).await;

        let mut config = UartConfig::default();
        config.baudrate = BT_BAUD_RATE;
        let uart = BufferedUart::new(uart, tx, rx, irq, tx_buf, rx_buf, config).ok()?;
        Some(Self {
            uart,
            pending: heapless::String::new(),
            _power: power,
        })
    }

    /// Wait for a command. Bytes that arrive while nobody is waiting are held
    /// by the buffered UART, so a command sent a moment early is not lost --
    /// only one sent *during* a measurement is, and that is deliberate: see
    /// `discard_pending`.
    pub async fn wait_command(&mut self, addr: u8) -> Command {
        let mut byte = [0u8; 1];
        loop {
            if self.uart.read(&mut byte).await.is_err() {
                continue;
            }
            if let Some(command) = self.feed(byte[0], addr) {
                return command;
            }
        }
    }

    /// Throw away everything buffered, partial line included.
    ///
    /// Called after a measurement so a command that arrived during one is
    /// dropped rather than queued -- the same rule the button follows, where
    /// `wait_for_falling_edge` clears pending edges before it arms. A reading
    /// answers the request that started it; a request made while the meter was
    /// busy would otherwise be answered by a reading it did not ask for, and
    /// the reader has no way to tell the two apart.
    pub async fn discard_pending(&mut self) {
        self.pending.clear();
        let mut sink = [0u8; 32];
        // `read_ready` is what keeps the await from parking: it only runs when
        // there is something buffered, so this drains and returns rather than
        // waiting for a byte that may never come.
        while self.uart.read_ready().unwrap_or(false) {
            if self.uart.read(&mut sink).await.is_err() {
                break;
            }
        }
    }

    /// One byte into the line assembler. Returns a command on the newline that
    /// completes one.
    fn feed(&mut self, byte: u8, addr: u8) -> Option<Command> {
        if byte == b'\n' || byte == b'\r' {
            let command = parse_command_for(self.pending.trim(), addr);
            self.pending.clear();
            return command;
        }
        // A line that outgrows the buffer is dropped rather than truncated:
        // truncation can turn an unknown command into a known prefix of one.
        if self.pending.push(byte as char).is_err() {
            self.pending.clear();
        }
        None
    }

    /// One line onto the wire.
    ///
    /// Awaited, not spun on. `blocking_write` looks like the cheaper choice for
    /// something this short, but a full TX ring puts it in a loop with no exit
    /// -- it returns only from inside a branch that requires room, and has no
    /// error path at all -- so the caller spins with the CPU never yielding and
    /// takes every other task down with it. Awaiting turns the same condition
    /// into this task waiting while the rest of the executor keeps running, and
    /// the indicator keeps blinking to say so.
    ///
    /// No deadline on the wait. At 115200 the whole 192-byte ring drains in
    /// 17 ms, so a wait longer than that means the port has stopped clocking,
    /// and that is a fault worth being visible rather than one to paper over
    /// by dropping frames on a timer.
    async fn put_line(&mut self, line: &str) {
        let _ = self.uart.write_all(line.as_bytes()).await;
    }

    /// "I am here, and I am meter n." Carries no reading on purpose: this is a
    /// presence beat, not a measurement, and nothing analog wakes up for it.
    pub async fn send_alive(&mut self, addr: u8) {
        let mut line: heapless::String<LINE_MAX> = heapless::String::new();
        if write!(line, "IMHERE,ADDR={}\r\n", addr).is_ok() {
            self.put_line(&line).await;
        }
    }

    /// One reading, on the wire.
    ///
    /// `METER_TEST` is withheld when the reading is one the meter cannot stand
    /// behind, because that frame has nowhere to say so -- its STATUS field is
    /// the alarm classification, and sending a fiction as NORMAL is worse than
    /// sending nothing: a reader that stops hearing from a meter shows it
    /// offline, which is true, while a confident wrong number is not.
    ///
    /// `OverRange` is the exception. It means the signal will not fit even at
    /// unity gain, which on this front end is a current past the top of scale
    /// -- the number is understated but the direction is known, and that is
    /// exactly the case 2(3) wants alarmed. It goes out as HIGH.
    ///
    /// `METER_CAL` always goes out, including for the withheld cases. A frame
    /// the bench cannot use is still evidence, and it carries its own FLAG.
    ///
    /// Each line is built whole before any of it is sent, and a line that
    /// overran `LINE_MAX` is dropped rather than sent short. A truncated frame
    /// is worse than a missing one: the reader's parser discards it either way,
    /// but silence reads as a meter that is offline, which is a state the
    /// reader already knows how to show.
    pub async fn send(&mut self, addr: u8, reading: &Reading) {
        let quality = reading.quality();
        let ma = milliamps(reading);
        let mut line: heapless::String<LINE_MAX> = heapless::String::new();

        let test_status = match quality {
            Quality::Good => Some(status(ma)),
            Quality::OverRange => Some("HIGH"),
            Quality::RefBad | Quality::InputBad | Quality::Incomplete => None,
        };
        if let Some(test_status) = test_status {
            if write!(
                line,
                "METER_TEST,ADDR={},CURRENT_MA={},STATUS={}\r\n",
                addr, ma, test_status
            )
            .is_ok()
            {
                self.put_line(&line).await;
            }
            line.clear();
        }

        let mut built = write!(
            line,
            "METER_CAL,ADDR={},RMS={:.2},GAIN={},FLAG={}",
            addr,
            reading.rms,
            reading.range.nominal(),
            flag(quality)
        )
        .is_ok();
        // Omitted rather than zeroed when the probe frame never drained: a mean
        // of 0 is a legal reading, and a bench that cannot tell it from "no
        // probe" would chase the wrong fault.
        if let Some((mean_in, pp_in)) = reading.probe {
            built &= write!(line, ",MEAN={},PP={}", mean_in, pp_in).is_ok();
        }
        built &= line.push_str("\r\n").is_ok();
        if built {
            self.put_line(&line).await;
        }
    }
}

/// `MEAS` alone is a broadcast; `MEAS,ADDR=n` is ignored unless `n` is this
/// meter's switch setting, so a reader can talk to one of several in range.
///
/// Deliberately not `METER_TEST`-shaped: commands travel the opposite
/// direction on the same transparent link, and a grammar a meter could mistake
/// for its own output is one echo away from a meter triggering itself.
pub fn parse_command_for(line: &str, addr: u8) -> Option<Command> {
    let command = parse_command(line)?;
    match line.split_once(",ADDR=") {
        None => Some(command),
        Some((_, want)) => (want.trim().parse::<u8>().ok()? == addr).then_some(command),
    }
}

fn parse_command(line: &str) -> Option<Command> {
    let head = line.split(',').next()?;
    match head {
        "MEAS" => Some(Command::Measure),
        _ => None,
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
