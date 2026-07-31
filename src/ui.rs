//! The local readout: an SSD1306 that is dark except for the second after a
//! reading, that is allowed to be absent, and that is not touched at all until
//! `supply` says the rail has reached `OLED_MIN_MV` -- waiting there without a
//! deadline rather than giving up on it.

use core::fmt::Write as _;

use embassy_time::Timer;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_9X15;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use heapless::String;

use crate::meter::{Quality, Reading};
use crate::oled::Oled;
use crate::supply;

/// How long a reading stays lit before the panel is blanked.
pub const DISPLAY_ON_MS: u64 = 1_000;

/// The rail the SSD1306 is brought up at, in millivolts. Below it the display
/// is left entirely alone -- not addressed, not even clocked -- and the reading
/// that wanted the panel waits here for as long as it takes.
///
/// This is a threshold on the *measured* supply, and the measurement carries
/// the divider's 1.5% and the reference's 1.4% (datasheet 7.12.1, 7.15.1), so a
/// rail regulated to a hair under 3.3 V can read a hair under 3.3 V. Since the
/// wait has no deadline, that is not a dark panel but a meter parked in
/// `show`: this constant is the one to lower.
const OLED_MIN_MV: u16 = 3_300;

/// How often the rail is re-measured while waiting for it. Each look is one
/// reference bring-up and one conversion, so this is also the duty cycle the
/// VREF module runs at across the wait.
const SUPPLY_POLL_MS: u64 = 20;

/// How many times the SSD1306 may be brought up before this power cycle gives
/// up on it. A failed attempt is retried on a later reading rather than being
/// remembered as final: this is a bit-banged bus with no arbitration, and one
/// bad transaction says less than four do. The cap is what keeps a display that
/// is simply absent from costing a bus transaction on every reading for the
/// rest of the power cycle.
///
/// A rail below `OLED_MIN_MV` costs no attempt at all -- an attempt is spent
/// only once the wait for the supply has returned, so every one of the four is
/// made against a rail that was measured good, which is what makes four of them
/// enough to conclude anything from.
const OLED_INIT_ATTEMPTS: u8 = 4;

/// A leading marker means the reading below it is not to be believed, and says
/// which way it went wrong. All three sit on the primary line on purpose: a
/// number this display cannot stand behind should not look like the others.
/// They matter most while calibrating -- a marked frame's `rms` is fiction,
/// and entering it into a CAL table freezes the fiction into the instrument.
fn marker(quality: Quality) -> &'static str {
    match quality {
        Quality::RefBad => "R",
        Quality::InputBad => "!",
        Quality::OverRange => ">",
        Quality::Incomplete => "?",
        Quality::Good => "",
    }
}

/// Wait until the rail reaches `OLED_MIN_MV`. There is no deadline: a supply
/// that never gets there parks the caller here forever.
///
/// That is the point rather than an oversight. A budget that expires has to
/// decide what to do next, and both answers are wrong -- drive an SSD1306 off a
/// rail that was measured too low, or return to the loop and leave the panel
/// dark until somebody presses S2, which is the display waiting on the operator
/// instead of on the supply. Waiting is the only answer that keeps the
/// threshold meaning what it says.
///
/// What it costs is everything downstream of `show`: `main` does not get back
/// to `wait_for_request` while this runs, so the heartbeat stops and `MEAS`
/// goes unanswered for as long as the rail is short. The radio frame for the
/// reading in hand has already gone out before `show` is called, so it is the
/// frames after it that are held, not this one.
///
/// The rail is measured, not slept against: this returns the moment it is high
/// enough, so a supply that was already up costs one conversion and no wait at
/// all.
///
/// A supply that cannot be measured counts as "not yet" rather than as
/// "unknown, carry on": `supply::millivolts` only comes back `None` for a
/// reference that never settled, and on this board the two things that do that
/// are a supply under 1.62 V and a missing CVREF cap. A rail under 1.62 V is
/// one that may still be climbing, which is exactly what there is to wait for.
async fn wait_for_supply() {
    while !supply::millivolts().is_some_and(|mv| mv >= OLED_MIN_MV) {
        Timer::after_millis(SUPPLY_POLL_MS).await;
    }
}

pub struct Panel {
    display: Option<Oled>,
    attempts_left: u8,
}

impl Panel {
    /// PB2/PB3 and the SSD1306 stay untouched until there is a reading to put
    /// on them, so this claims nothing.
    pub const fn new() -> Self {
        Self {
            display: None,
            attempts_left: OLED_INIT_ATTEMPTS,
        }
    }

    /// Bring-up is attempted here rather than at construction because a failed
    /// attempt leaves `display` at `None` and is tried again on a later
    /// reading: the attempt that failed dropped its half-built driver, and with
    /// it the pins it had stolen, so the next attempt starts from the same
    /// state as the first one did.
    ///
    /// The supply is waited on first, and only while there is no display yet:
    /// once one is up it stays up, so this is a gate on starting to drive the
    /// panel and not a condition re-imposed on every frame. A display already
    /// running is not taken away from the operator because the rail dipped
    /// under the bar it was started at.
    ///
    /// Spent attempts are checked before the rail and not after, so a board
    /// that has already proved it has no display never waits on a supply it has
    /// no use for.
    async fn get(&mut self) -> Option<&mut Oled> {
        if self.display.is_none() {
            if self.attempts_left == 0 {
                return None;
            }
            wait_for_supply().await;
            self.attempts_left -= 1;
            // SAFETY: PB2/PB3 are used by nothing else in this firmware, and
            // no live `Oled` exists here -- `display` is `None`.
            self.display = unsafe { Oled::new() }.ok();
        }
        self.display.as_mut()
    }

    pub async fn show(&mut self, reading: &Reading) {
        let mark = marker(reading.quality());
        let Some(display) = self.get().await else {
            return;
        };
        let _ = display.clear(BinaryColor::Off);
        let style = MonoTextStyle::new(&FONT_9X15, BinaryColor::On);

        let mut line: String<32> = String::new();
        let _ = write!(line, "{}{:.3} A", mark, reading.amps);
        let _ = Text::new(&line, Point::new(4, 26), style).draw(display);

        // Raw single-frame RMS in LSB and the gain it was taken at -- together
        // they are one row of one CAL table, and the gain says *which* table,
        // so the calibration cannot be entered against the wrong one. Two
        // decimals: 0.01 LSB is 0.03% at 0.1 A, so the display resolution is
        // not what limits the calibration.
        //
        // Starts at x=0, not x=4 like the line above: 14 characters of
        // FONT_9X15 is exactly 126 px, so the margin is the difference between
        // fitting and losing the last digit of the gain.
        let mut line2: String<32> = String::new();
        let _ = write!(line2, "rms {:.2} x{}", reading.rms, reading.range.nominal());
        let _ = Text::new(&line2, Point::new(0, 52), style).draw(display);

        let _ = display.flush();
        let _ = display.set_display_on(true);
    }

    pub fn blank(&mut self) {
        if let Some(display) = &mut self.display {
            let _ = display.set_display_on(false);
        }
    }
}
