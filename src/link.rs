//! The wireless link: HC-42 on UART2, the address switch that names this
//! meter, the frames that go out and the one command that comes in.
//!
//! Shared by both binaries so there is exactly one wire format. `main.rs`
//! keeps the link up and measures when told to; `bin/stream.rs` keeps it up and
//! measures on a timer. Neither can drift from the other's grammar.

use core::fmt::Write as _;

use embassy_futures::select::{Either, select};
use embassy_mspm0::Peri;
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::interrupt::typelevel::Binding;
use embassy_mspm0::peripherals;
use embassy_mspm0::uart::{BufferedInterruptHandler, BufferedUart, Config as UartConfig, Instance};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::{Read, ReadReady, Write};

use crate::cal::{self, CalError};
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
///
/// Sized by `CALPT`, the longest thing anyone sends: at full width
/// `CALPT,ADDR=15,I=15,X=12345.67,Y=2.34567` is 40 characters.
const CMD_MAX: usize = 64;

/// Longest line this link emits, CRLF included. `METER_CAL` at its widest --
/// every optional field present and every number at full width -- is 72 bytes.
/// The rest is margin for an `rms` the DSP produced outside the converter's
/// range, which is a number worth reporting rather than reshaping.
const LINE_MAX: usize = 128;

/// How long one chunk may sit unsent before the meter gives up on the line.
///
/// A wait only happens when the ring is full, and at `BT_BAUD_RATE` the whole
/// 192-byte ring is 17 ms of wire time, so anything past this is a port that
/// has stopped clocking rather than one that is behind. Sized well clear of a
/// legitimate backlog so it never fires on a link that is merely busy.
const TX_DEADLINE_MS: u64 = 250;

/// How much of a line goes into the module at a time.
///
/// One BLE notification on the default ATT MTU of 23 carries 20 bytes, and the
/// module has one connection event to send it in. Nothing tells the meter when
/// that has happened: the HC-42 has no flow control, so its UART and its radio
/// are two unsynchronised rates with only the module's own buffer between them
/// -- and the meter's side is faster by three orders of magnitude. Handing it a
/// whole 62-byte frame at `BT_BAUD_RATE` delivers it in 5 ms and asks the module
/// to hold the remainder for several connection intervals, which is how a frame
/// comes back to the reader with its tail missing, or not at all. The heartbeat
/// is 15 bytes and never shows it.
const BT_PACKET_MAX: usize = 20;

/// The pause between chunks: one connection interval, so each chunk is picked
/// up before the next arrives.
///
/// Android settles a BLE connection at 30-50 ms and the module publishes no
/// preference, so this is set from the slow end. It costs one interval per
/// chunk on the way out -- about 350 ms for a whole reading -- which is spent
/// inside the reader's reply window and buys the frames actually arriving.
const BT_PACKET_GAP_MS: u64 = 50;

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

/// What a reader can ask for.
///
/// Only `Measure` reaches the caller. The rest are the calibration push, which
/// `wait_command` serves itself and then goes back to waiting -- pushing a
/// table is not a request for a reading, and a meter that measured every time
/// the bench app sent it a point would spend the whole session lighting its
/// panel.
#[derive(Clone, Copy, PartialEq)]
pub enum Command {
    /// Take one reading now. Equivalent to a press of S2, including what
    /// happens to a second one that arrives mid-measurement: nothing.
    Measure,
    /// `CALPT`: one point of a table being pushed, staged but not installed.
    CalPoint { index: usize, lsb: f32, amps: f32 },
    /// `CALEND`: install the staged points, all `count` of them.
    CalCommit { count: usize },
    /// `CALOFF`: back to the built-in table.
    CalClear,
    /// `CALGET`: say which table is in use. The one command that changes
    /// nothing, and the one the app leans on -- it is how a reader that has
    /// just connected finds out whether the meter it is talking to came
    /// through its last power cycle still calibrated.
    CalStatus,
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
    /// Also where the heartbeat lives, rather than in a `select` around this
    /// future in the caller. The reason is cancellation: a caller racing this
    /// against a beat timer drops it wherever it happens to be, and where it
    /// happens to be is often inside the paced write of a reply. `put_line`
    /// waits a connection interval per chunk, so answering one calibration
    /// command is around 100 ms of awaiting against a 2 s beat -- push a
    /// dozen points and losing at least one reply to the meter's own heartbeat
    /// stops being unlikely. Here the beat can only ever interrupt the byte
    /// read, which holds nothing worth keeping.
    ///
    /// The switch is re-read on every beat and every command, so moving the
    /// address still takes effect between two beats.
    pub async fn wait_command(&mut self, address: &Address) -> Command {
        loop {
            let mut byte = [0u8; 1];
            let mut beat = false;
            let received = {
                let read = self.uart.read(&mut byte);
                match select(read, Timer::after_millis(HEARTBEAT_MS)).await {
                    Either::First(outcome) => outcome.map(|read| read > 0).unwrap_or(false),
                    Either::Second(()) => {
                        beat = true;
                        false
                    }
                }
            };
            if beat {
                self.send_alive(address.read()).await;
                continue;
            }
            if !received {
                continue;
            }
            let addr = address.read();
            match self.feed(byte[0], addr) {
                Some(Command::Measure) => return Command::Measure,
                // Served here rather than returned, so the caller's idle loop
                // never learns a calibration push happened. It costs the caller
                // nothing and it means the panel does not light 10 times while
                // a table is being pushed.
                Some(command) => self.serve(addr, command).await,
                None => {}
            }
        }
    }

    /// Carry out one calibration command and answer it.
    ///
    /// Every one of them is answered, including the ones that fail. A push is
    /// a sequence of a dozen lines over a link that drops them occasionally,
    /// and the app's only way to know a point landed is to be told so -- the
    /// meter's readings will not change until the whole table is committed,
    /// by design.
    async fn serve(&mut self, addr: u8, command: Command) {
        match command {
            // Handled by the caller; here only because the match is total.
            Command::Measure => {}
            Command::CalPoint { index, lsb, amps } => {
                let outcome = cal::stage_point(index, lsb, amps);
                self.send_cal_ack(addr, index, outcome.err()).await;
            }
            Command::CalCommit { count } => {
                let outcome = cal::commit_field(count);
                self.send_cal_status(addr, outcome.err()).await;
            }
            Command::CalClear => {
                let outcome = cal::clear_field();
                self.send_cal_status(addr, outcome.err()).await;
            }
            Command::CalStatus => self.send_cal_status(addr, None).await,
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
    /// Bounded, because this task is the whole meter. A port that has stopped
    /// clocking is a fault worth being visible, but an unbounded wait here does
    /// not make it visible -- it parks `main` mid-frame, which stops the
    /// heartbeat and stops the button being waited on, so the meter goes silent
    /// and deaf at once and looks like it lost power. Past `TX_DEADLINE_MS` the
    /// line is abandoned and the meter carries on: one dropped frame the reader
    /// already knows how to survive, against a meter that answers nothing.
    ///
    /// What is already in the ring stays there and goes out whenever the port
    /// comes back, so an abandoned line can surface as one truncated frame. The
    /// parser drops it on the newline it does not fit, which costs the line
    /// after it and nothing further.
    /// Paced at `BT_PACKET_MAX` a chunk so the module is never handed more than
    /// it can put on the air before the next chunk lands. The gap goes in front
    /// of every chunk, this line's first one included: two lines of a reading go
    /// out back to back, and the module cannot tell where one ends.
    async fn put_line(&mut self, line: &str) {
        for chunk in line.as_bytes().chunks(BT_PACKET_MAX) {
            Timer::after_millis(BT_PACKET_GAP_MS).await;
            if with_timeout(
                Duration::from_millis(TX_DEADLINE_MS),
                self.uart.write_all(chunk),
            )
            .await
            .is_err()
            {
                return;
            }
        }
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
    /// Every reading goes out, whatever the meter thinks of it, and the number
    /// on the wire is the number on the panel. The two readouts are one
    /// instrument: a panel showing 0.42 A while the reader shows nothing is not
    /// a meter being careful, it is a meter disagreeing with itself, and the
    /// operator has no way to tell that from a link that dropped the frame.
    ///
    /// STATUS is the alarm classification of the number being sent and nothing
    /// else. It is not a quality mark and does not change with one.
    ///
    /// `METER_CAL` carries the FLAG, so what the meter made of the reading is
    /// still on the wire for the bench -- alongside the reading rather than
    /// instead of it.
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

        if write!(
            line,
            "METER_TEST,ADDR={},CURRENT_MA={},STATUS={}\r\n",
            addr,
            ma,
            status(ma)
        )
        .is_ok()
        {
            self.put_line(&line).await;
        }
        line.clear();

        // `SRC` says which table produced `CURRENT_MA`, and it is on every
        // frame rather than only on request because the thing it guards
        // against is silent: this meter loses power whenever the loop opens,
        // and a field table that failed to come back from flash would
        // otherwise show up as nothing more than readings that are a little
        // worse. The app watches this field and re-pushes when it reads ROM.
        let mut built = write!(
            line,
            "METER_CAL,ADDR={},RMS={:.2},GAIN={},FLAG={},SRC={}",
            addr,
            reading.rms,
            reading.range.nominal(),
            flag(quality),
            source()
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

    /// "Point `index` is staged", or why it is not.
    ///
    /// Answered per point rather than once at the end, because the alternative
    /// is a push that reports `MISSING` and cannot say which one went astray.
    async fn send_cal_ack(&mut self, addr: u8, index: usize, error: Option<CalError>) {
        let mut line: heapless::String<LINE_MAX> = heapless::String::new();
        let mut built = write!(line, "CALACK,ADDR={},I={}", addr, index).is_ok();
        if let Some(error) = error {
            built &= write!(line, ",ERR={}", cal_error(error)).is_ok();
        }
        built &= line.push_str("\r\n").is_ok();
        if built {
            self.put_line(&line).await;
        }
    }

    /// Which table the meter is reading on, and what the last request did to
    /// it. Sent in answer to `CALEND`, `CALOFF` and `CALGET` alike, so the app
    /// has one frame to parse and one meaning to read from it: this is the
    /// state you are now in, whatever you asked for.
    async fn send_cal_status(&mut self, addr: u8, error: Option<CalError>) {
        let points = cal::field_len();
        let mut line: heapless::String<LINE_MAX> = heapless::String::new();
        let mut built = write!(
            line,
            "CALSTAT,ADDR={},SRC={},N={}",
            addr,
            source(),
            points
        )
        .is_ok();
        if let Some(error) = error {
            built &= write!(line, ",ERR={}", cal_error(error)).is_ok();
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
/// The `CAL*` commands have no broadcast form: a calibration table belongs to
/// the meter it was measured on, and a table pushed to "whoever is listening"
/// would install a curve from one front end onto another. They are dropped
/// unless they name this meter.
///
/// Deliberately not `METER_TEST`-shaped: commands travel the opposite
/// direction on the same transparent link, and a grammar a meter could mistake
/// for its own output is one echo away from a meter triggering itself. The
/// answers this link sends -- `CALACK` and `CALSTAT` -- are outside the
/// command grammar for the same reason, and the heads are compared whole so
/// that a shared prefix is not a shared meaning.
pub fn parse_command_for(line: &str, addr: u8) -> Option<Command> {
    let head = line.split(',').next()?;
    let named = field(line, "ADDR");
    match head {
        "MEAS" => match named {
            None => Some(Command::Measure),
            Some(want) => (want.parse::<u8>().ok()? == addr).then_some(Command::Measure),
        },
        "CALPT" | "CALEND" | "CALOFF" | "CALGET" => {
            if named?.parse::<u8>().ok()? != addr {
                return None;
            }
            parse_cal(head, line)
        }
        _ => None,
    }
}

fn parse_cal(head: &str, line: &str) -> Option<Command> {
    match head {
        // A point carries its own index rather than arriving in order, so a
        // line the link dropped can be re-sent on its own instead of restarting
        // the push -- and so the meter can tell a lost point from a re-sent one.
        "CALPT" => Some(Command::CalPoint {
            index: field(line, "I")?.parse().ok()?,
            lsb: field(line, "X")?.parse().ok()?,
            amps: field(line, "Y")?.parse().ok()?,
        }),
        // The count is stated rather than inferred from what arrived. Inferring
        // it would make a push that lost its last point look complete.
        "CALEND" => Some(Command::CalCommit {
            count: field(line, "N")?.parse().ok()?,
        }),
        "CALOFF" => Some(Command::CalClear),
        "CALGET" => Some(Command::CalStatus),
        _ => None,
    }
}

/// One `KEY=value` out of a command line, whichever position it sits in.
///
/// Positional parsing is what this replaces: `ADDR` is last in `MEAS` and first
/// in the `CAL*` commands, and a parser that reads to the end of the line finds
/// `6,I=0,X=13.56` where it expects an address.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(',').skip(1).find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name.trim() == key).then(|| value.trim())
    })
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

/// `FIELD` once a pushed table is installed, `ROM` while the meter is on its
/// built-in one. Two values and no count: the count is a separate field, and a
/// reader that only wants to know "is this meter still calibrated" should not
/// have to know how many points that took.
fn source() -> &'static str {
    if cal::field_len() >= 2 { "FIELD" } else { "ROM" }
}

fn cal_error(error: CalError) -> &'static str {
    match error {
        CalError::Index => "INDEX",
        CalError::Value => "VALUE",
        CalError::Missing => "MISSING",
        CalError::Order => "ORDER",
        CalError::Flash => "FLASH",
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
