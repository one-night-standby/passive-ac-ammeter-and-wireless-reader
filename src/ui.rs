//! The local readout: an SSD1306 that is dark except for the second after a
//! reading, and that is allowed to be absent.
//!
//! Nothing here looks at the supply. `main` does not reach its first reading
//! until the rail has come up, so by the time a panel exists to bring up it is
//! already being driven off a rail that was measured good.

use core::fmt::Write as _;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_9X15;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use heapless::String;

use crate::meter::{Quality, Reading};
use crate::oled::Oled;

/// How long a reading stays lit before the panel is blanked.
pub const DISPLAY_ON_MS: u64 = 1_000;

/// How many times the SSD1306 may be brought up before this power cycle gives
/// up on it. A failed attempt is retried on a later reading rather than being
/// remembered as final: this is a bit-banged bus with no arbitration, and one
/// bad transaction says less than four do. The cap is what keeps a display that
/// is simply absent from costing a bus transaction on every reading for the
/// rest of the power cycle.
///
/// Four is enough to conclude something from because every one of them is made
/// on a supply that was measured good before the firmware got this far. A weak
/// rail cannot spend them.
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
    fn get(&mut self) -> Option<&mut Oled> {
        if self.display.is_none() && self.attempts_left > 0 {
            self.attempts_left -= 1;
            // SAFETY: PB2/PB3 are used by nothing else in this firmware, and
            // no live `Oled` exists here -- `display` is `None`.
            self.display = unsafe { Oled::new() }.ok();
        }
        self.display.as_mut()
    }

    pub fn show(&mut self, reading: &Reading) {
        let mark = marker(reading.quality());
        let Some(display) = self.get() else {
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
