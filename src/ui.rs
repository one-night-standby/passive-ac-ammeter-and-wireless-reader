//! The local readout: an SSD1306 that is dark except for the second after a
//! reading, and that is allowed to stop answering.
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

use crate::meter::Reading;
use crate::oled::Oled;

/// How long a reading stays lit before the panel is blanked.
pub const DISPLAY_ON_MS: u64 = 1_000;

pub struct Panel {
    display: Option<Oled>,
}

impl Panel {
    /// PB2/PB3 and the SSD1306 stay untouched until there is a reading to put
    /// on them, so this claims nothing.
    pub const fn new() -> Self {
        Self { display: None }
    }

    /// Bring-up is attempted here rather than at construction because a failed
    /// attempt leaves `display` at `None` and is tried again on the next
    /// reading: the attempt that failed dropped its half-built driver, and with
    /// it the pins it had stolen, so the next attempt starts from the same
    /// state as the first one did.
    ///
    /// Nothing limits how many attempts a power cycle may make. The panel is
    /// soldered to the board, so a NACK never means it is gone -- it means the
    /// rail sagged under it or the bus glitched, and both of those come back.
    /// There is no state a retry could be wrong about, and the attempt that
    /// finds the panel still silent ends on the address byte of `init`'s first
    /// command, inside 30 us, once per reading.
    fn get(&mut self) -> Option<&mut Oled> {
        if self.display.is_none() {
            // SAFETY: PB2/PB3 are used by nothing else in this firmware, and
            // no live `Oled` exists here -- `display` is `None`.
            self.display = unsafe { Oled::new() }.ok();
        }
        self.display.as_mut()
    }

    /// Drop the driver so the next reading brings the panel up from scratch.
    ///
    /// A panel that stops acknowledging has lost its rail or lost the bus, and
    /// it comes back from either one reset: its GDDRAM, its addressing mode and
    /// its charge pump are no longer what this driver assumes, and only a fresh
    /// `init` puts them back. Keeping the driver would keep writing frames that
    /// land nowhere.
    fn lost(&mut self) {
        self.display = None;
    }

    pub fn show(&mut self, reading: &Reading) {
        let Some(display) = self.get() else {
            return;
        };
        if Self::draw(display, reading).is_err() {
            self.lost();
        }
    }

    /// The first failure ends the frame. Past a NACK the panel is not listening,
    /// so the rest of it would be written into the void and `set_display_on`
    /// would light nothing.
    fn draw(display: &mut Oled, reading: &Reading) -> Result<(), ()> {
        display.clear(BinaryColor::Off)?;
        let style = MonoTextStyle::new(&FONT_9X15, BinaryColor::On);

        let mut line: String<32> = String::new();
        let _ = write!(line, "{:.3} A", reading.amps);
        Text::new(&line, Point::new(4, 26), style).draw(display)?;

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
        Text::new(&line2, Point::new(0, 52), style).draw(display)?;

        display.flush()?;
        display.set_display_on(true)
    }

    pub fn blank(&mut self) {
        if let Some(display) = &mut self.display
            && display.set_display_on(false).is_err()
        {
            self.lost();
        }
    }
}
