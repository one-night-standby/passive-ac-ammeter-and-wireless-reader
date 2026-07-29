//! Bit-banged-I2C SSD1306 `DrawTarget`, ported from
//! ~/embed/prepare-nuedc/drivers/{shared/ssd1306.rs,mspm0g3507-ssd1306} --
//! same fixed pins (PB2 = SCL, PB3 = SDA) as that project's LP-MSPM0G3507
//! board convention. Software I2C avoids the hardware I2C1 FIFO quirks that
//! ~/embed/Single-Phase-Power-Analyzer worked around with a custom chunked
//! writer; simplicity over speed since this display only needs to refresh a
//! few times a second.

use core::convert::Infallible;

use embassy_mspm0::gpio::{Level, OutputOpenDrain};
use embassy_mspm0::peripherals;
use embedded_graphics_core::draw_target::DrawTarget;
use embedded_graphics_core::geometry::{OriginDimensions, Size};
use embedded_graphics_core::pixelcolor::BinaryColor;
use embedded_graphics_core::primitives::Rectangle;
use embedded_graphics_core::Pixel;
use embedded_hal::i2c::{ErrorType, I2c, Operation};
use ssd1306::mode::{BufferedGraphicsMode, DisplayConfig};
use ssd1306::prelude::{DisplayRotation, DisplaySize128x64, I2CInterface};
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

impl SoftI2c {
    fn tick() {
        for _ in 0..16 {
            cortex_m::asm::nop();
        }
    }

    fn start(&mut self) {
        self.sda.set_high();
        self.scl.set_high();
        Self::tick();
        self.sda.set_low();
        Self::tick();
        self.scl.set_low();
    }

    fn stop(&mut self) {
        self.sda.set_low();
        self.scl.set_high();
        Self::tick();
        self.sda.set_high();
        Self::tick();
    }

    fn write_byte(&mut self, byte: u8) {
        for bit in (0..8).rev() {
            if byte & (1 << bit) != 0 {
                self.sda.set_high();
            } else {
                self.sda.set_low();
            }
            self.scl.set_high();
            Self::tick();
            self.scl.set_low();
        }
        // Release SDA for ACK. The display's ACK is sampled for bus timing,
        // while this infallible GPIO backend deliberately does not surface it.
        self.sda.set_high();
        self.scl.set_high();
        Self::tick();
        let _ack = self.sda.is_low();
        self.scl.set_low();
    }

    fn read_byte(&mut self, last: bool) -> u8 {
        self.sda.set_high();
        let mut byte = 0;
        for _ in 0..8 {
            self.scl.set_high();
            Self::tick();
            byte = (byte << 1) | u8::from(self.sda.is_high());
            self.scl.set_low();
        }
        if last {
            self.sda.set_high();
        } else {
            self.sda.set_low();
        }
        self.scl.set_high();
        Self::tick();
        self.scl.set_low();
        self.sda.set_high();
        byte
    }
}

impl ErrorType for SoftI2c {
    type Error = Infallible;
}

impl I2c for SoftI2c {
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut [Operation<'_>],
    ) -> Result<(), Self::Error> {
        for operation in operations {
            self.start();
            match operation {
                Operation::Read(bytes) => {
                    self.write_byte((address << 1) | 1);
                    let length = bytes.len();
                    for (index, byte) in bytes.iter_mut().enumerate() {
                        *byte = self.read_byte(index + 1 == length);
                    }
                }
                Operation::Write(bytes) => {
                    self.write_byte(address << 1);
                    for &byte in *bytes {
                        self.write_byte(byte);
                    }
                }
            }
            self.stop();
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
        Ok(Self { inner })
    }

    pub fn flush(&mut self) -> Result<(), ()> {
        self.inner.flush().map_err(|_| ())
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
