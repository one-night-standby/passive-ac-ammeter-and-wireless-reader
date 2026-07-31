//! Power-on indicator on the LaunchPad's red LED1 (PA0, jumper J4).

use embassy_mspm0::gpio::Output;
use embassy_time::{Duration, Timer};

/// Blink rate, in blinks per second.
const BLINK_HZ: u64 = 3;

/// It says the firmware is alive, which is the one thing the panel cannot: the
/// OLED is dark except for the second after a reading, and a dark panel looks
/// the same whether the board is unpowered, stuck before the first press, or
/// simply idle. Blinking rather than steady, because a steady LED cannot tell
/// a running executor from a hung one.
///
/// Its own task so it keeps its rate through a measurement cycle, which holds
/// the caller for a few hundred milliseconds at a stretch. Toggling is twice
/// per blink, hence the doubled rate.
#[embassy_executor::task]
pub async fn blink(mut led: Output<'static>) -> ! {
    let half_period = Duration::from_hz(2 * BLINK_HZ);
    loop {
        led.toggle();
        Timer::after(half_period).await;
    }
}
