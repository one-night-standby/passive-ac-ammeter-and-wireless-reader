//! Bit-banged-I2C SSD1306 `DrawTarget`, ported from
//! ~/embed/prepare-nuedc/drivers/{shared/ssd1306.rs,mspm0g3507-ssd1306} --
//! same fixed pins (PB2 = SCL, PB3 = SDA) as that project's LP-MSPM0G3507
//! board convention. Software I2C avoids the hardware I2C1 FIFO quirks that
//! ~/embed/Single-Phase-Power-Analyzer worked around with a custom chunked
//! writer; simplicity over speed since this display only needs to refresh a
//! few times a second.

use embassy_mspm0::gpio::{Level, OutputOpenDrain};
use embassy_mspm0::peripherals;
use embedded_graphics_core::Pixel;
use embedded_graphics_core::draw_target::DrawTarget;
use embedded_graphics_core::geometry::{OriginDimensions, Size};
use embedded_graphics_core::pixelcolor::BinaryColor;
use embedded_graphics_core::primitives::Rectangle;
use embedded_hal::i2c::{ErrorKind, ErrorType, I2c, NoAcknowledgeSource, Operation};
use ssd1306::mode::{BufferedGraphicsMode, DisplayConfig};
use ssd1306::prelude::{Brightness, DisplayRotation, DisplaySize128x64, I2CInterface};
use ssd1306::{I2CDisplayInterface, Ssd1306};

type OpenDrainPin = OutputOpenDrain<'static>;

struct Ssd1306Pins {
    scl: OpenDrainPin,
    sda: OpenDrainPin,
}

unsafe fn steal_pins() -> Ssd1306Pins {
    Ssd1306Pins {
        scl: OutputOpenDrain::new(unsafe { peripherals::PB2::steal() }, Level::High),
        sda: OutputOpenDrain::new(unsafe { peripherals::PB3::steal() }, Level::High),
    }
}

struct SoftI2c {
    scl: OpenDrainPin,
    sda: OpenDrainPin,
}

/// BUSCLK, which is what `cortex_m::asm::delay` counts against.
const CPU_HZ: u32 = 32_000_000;

/// Round up: a phase that lands under the datasheet minimum is the exact
/// failure these constants exist to prevent.
const fn cycles_for_ns(ns: u32) -> u32 {
    (CPU_HZ / 1_000_000 * ns).div_ceil(1_000)
}

/// SSD1306 fast-mode I2C timing: t_LOW >= 1.3 us, t_HIGH >= 0.6 us, and a
/// cycle of at least 2.5 us (400 kHz). These sit above all three with margin,
/// because the two directions are not symmetric -- being slow costs a few
/// milliseconds per refresh, being fast costs a display that answers
/// intermittently, and now that the ACK is checked an out-of-spec bus does not
/// fail quietly any more, it fails as a NACK the driver believes.
const SCL_LOW_CYCLES: u32 = cycles_for_ns(1_700);
const SCL_HIGH_CYCLES: u32 = cycles_for_ns(1_300);

impl SoftI2c {
    /// SCL held low, and the window in which SDA is allowed to move.
    fn low_phase() {
        cortex_m::asm::delay(SCL_LOW_CYCLES);
    }

    /// SCL held high, during which SDA must be stable -- this is the window
    /// the slave samples data in, and drives the ACK bit in.
    fn high_phase() {
        cortex_m::asm::delay(SCL_HIGH_CYCLES);
    }

    fn start(&mut self) {
        self.sda.set_high();
        self.scl.set_high();
        Self::high_phase();
        self.sda.set_low();
        Self::high_phase();
        self.scl.set_low();
        Self::low_phase();
    }

    fn stop(&mut self) {
        self.sda.set_low();
        Self::low_phase();
        self.scl.set_high();
        Self::high_phase();
        self.sda.set_high();
        // Bus-free time before whatever START comes next.
        Self::high_phase();
    }

    /// Returns whether the slave acknowledged: SDA pulled low by the far end
    /// during the ninth clock. This is the only failure a bit-banged master
    /// can observe at all, so it is the whole error detection this bus has --
    /// an unpowered or absent display NACKs, and without this the driver would
    /// write into the void and report success.
    #[must_use]
    fn write_byte(&mut self, byte: u8) -> bool {
        for bit in (0..8).rev() {
            if byte & (1 << bit) != 0 {
                self.sda.set_high();
            } else {
                self.sda.set_low();
            }
            Self::low_phase();
            self.scl.set_high();
            Self::high_phase();
            self.scl.set_low();
        }
        // Release SDA so the slave can drive the ACK bit, and sample it at the
        // end of the high phase, which is the most time the slave can be given
        // to pull the line down.
        self.sda.set_high();
        Self::low_phase();
        self.scl.set_high();
        Self::high_phase();
        let acked = self.sda.is_low();
        self.scl.set_low();
        acked
    }

    fn read_byte(&mut self, last: bool) -> u8 {
        self.sda.set_high();
        let mut byte = 0;
        for _ in 0..8 {
            Self::low_phase();
            self.scl.set_high();
            Self::high_phase();
            byte = (byte << 1) | u8::from(self.sda.is_high());
            self.scl.set_low();
        }
        if last {
            self.sda.set_high();
        } else {
            self.sda.set_low();
        }
        Self::low_phase();
        self.scl.set_high();
        Self::high_phase();
        self.scl.set_low();
        self.sda.set_high();
        byte
    }
}

/// Nobody drove the ACK bit low. On this bus that means the display is absent,
/// unpowered, or not answering at the address we used.
#[derive(Debug)]
pub struct Nack(NoAcknowledgeSource);

impl embedded_hal::i2c::Error for Nack {
    fn kind(&self) -> ErrorKind {
        ErrorKind::NoAcknowledge(self.0)
    }
}

impl ErrorType for SoftI2c {
    type Error = Nack;
}

impl I2c for SoftI2c {
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut [Operation<'_>],
    ) -> Result<(), Self::Error> {
        for operation in operations {
            self.start();
            let result = match operation {
                Operation::Read(bytes) => {
                    if self.write_byte((address << 1) | 1) {
                        let length = bytes.len();
                        for (index, byte) in bytes.iter_mut().enumerate() {
                            *byte = self.read_byte(index + 1 == length);
                        }
                        Ok(())
                    } else {
                        Err(Nack(NoAcknowledgeSource::Address))
                    }
                }
                Operation::Write(bytes) => {
                    if !self.write_byte(address << 1) {
                        Err(Nack(NoAcknowledgeSource::Address))
                    } else if bytes.iter().copied().all(|byte| self.write_byte(byte)) {
                        Ok(())
                    } else {
                        Err(Nack(NoAcknowledgeSource::Data))
                    }
                }
            };
            // STOP runs on the failing path too. Returning straight out of a
            // NACK would leave SCL low and the bus owned by a master that has
            // stopped clocking it, and the retry would then start from a state
            // no slave can recover from.
            self.stop();
            result?;
        }
        Ok(())
    }
}

type Interface = I2CInterface<SoftI2c>;
type Inner = Ssd1306<Interface, DisplaySize128x64, BufferedGraphicsMode<DisplaySize128x64>>;

/// Concrete 128x64 buffered SSD1306 `DrawTarget`.
pub struct Oled {
    inner: Inner,
}

impl Oled {
    /// Steal the board's fixed SSD1306 pins (PB2/PB3) and initialize the
    /// display.
    ///
    /// # Safety
    ///
    /// No other live driver may use PB2 or PB3. The caller must also
    /// initialize the MCU HAL (`embassy_mspm0::init`) before calling this.
    pub unsafe fn new() -> Result<Self, ()> {
        let mut pins = unsafe { steal_pins() };
        pins.scl.set_high();
        pins.sda.set_high();
        let interface = I2CDisplayInterface::new(SoftI2c {
            scl: pins.scl,
            sda: pins.sda,
        });
        let mut inner = Ssd1306::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
            .into_buffered_graphics_mode();
        inner.init().map_err(|_| ())?;
        // Lower the segment drive current while the panel is on. This sends
        // Set Contrast Control (0x81, 0x0F); temporal duty cycling below is an
        // independent power-saving layer.
        inner
            .set_brightness(Brightness::custom(0x2, 0x0F))
            .map_err(|_| ())?;
        // Keep the panel dark while the first framebuffer is prepared. The
        // measurement loop turns it on only after S2 toggles display mode on.
        inner.set_display_on(false).map_err(|_| ())?;
        Ok(Self { inner })
    }

    pub fn flush(&mut self) -> Result<(), ()> {
        self.inner.flush().map_err(|_| ())
    }

    pub fn set_display_on(&mut self, on: bool) -> Result<(), ()> {
        self.inner.set_display_on(on).map_err(|_| ())
    }
}

impl OriginDimensions for Oled {
    fn size(&self) -> Size {
        Size::new(128, 64)
    }
}

impl DrawTarget for Oled {
    type Color = BinaryColor;
    type Error = ();

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        self.inner.draw_iter(pixels).map_err(|_| ())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        self.inner.fill_contiguous(area, colors).map_err(|_| ())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        self.inner.fill_solid(area, color).map_err(|_| ())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.inner.clear(color).map_err(|_| ())
    }
}
