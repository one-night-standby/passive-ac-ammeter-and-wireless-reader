#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_mspm0::gpio::{Level, Output};
use embassy_mspm0::uart::{Config, Uart};
use embassy_time::Timer;
use panic_halt as _;

const HC42_BAUD_RATE: u32 = 9_600;
const TEST_FRAME: &[u8] = b"METER_TEST,ADDR=01,CURRENT_MA=1234,STATUS=NORMAL\r\n";

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let peripherals = embassy_mspm0::init(Default::default());

    let mut led = Output::new(peripherals.PA0, Level::Low);
    led.set_inversion(true);

    let mut uart_config = Config::default();
    uart_config.baudrate = HC42_BAUD_RATE;
    let mut hc42 = Uart::new_blocking(
        peripherals.UART2,
        peripherals.PB16,
        peripherals.PB15,
        uart_config,
    )
    .unwrap();

    // HC-42 needs about 300 ms after power-on before it is ready.
    Timer::after_millis(500).await;

    loop {
        hc42.blocking_write(TEST_FRAME).unwrap();
        hc42.blocking_flush().unwrap();
        led.toggle();
        Timer::after_secs(1).await;
    }
}
