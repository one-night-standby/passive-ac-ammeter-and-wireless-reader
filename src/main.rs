#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_mspm0::gpio::{Input, Level, Output, Pull};
use embassy_mspm0::uart::{Config, Uart};
use embassy_time::Timer;
use panic_halt as _;

const HC42_BAUD_RATE: u32 = 9_600;
const ADDRESS_DECIMAL_OFFSET: usize = b"METER_TEST,ADDR=".len();

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let peripherals = embassy_mspm0::init(Default::default());

    let mut led = Output::new(peripherals.PA0, Level::Low);
    led.set_inversion(true);

    // Each switch connects its GPIO to GND when ON. The internal pull-up makes
    // an open switch read as 0 and a closed switch read as 1 after inversion.
    let address_bit0 = Input::new(peripherals.PB0, Pull::Up);
    let address_bit1 = Input::new(peripherals.PB6, Pull::Up);
    let address_bit2 = Input::new(peripherals.PB7, Pull::Up);
    let address_bit3 = Input::new(peripherals.PB8, Pull::Up);

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

    let mut test_frame = *b"METER_TEST,ADDR=00,CURRENT_MA=1234,STATUS=NORMAL\r\n";

    loop {
        let address = u8::from(address_bit0.is_low())
            | (u8::from(address_bit1.is_low()) << 1)
            | (u8::from(address_bit2.is_low()) << 2)
            | (u8::from(address_bit3.is_low()) << 3);

        test_frame[ADDRESS_DECIMAL_OFFSET] = b'0' + address / 10;
        test_frame[ADDRESS_DECIMAL_OFFSET + 1] = b'0' + address % 10;

        hc42.blocking_write(&test_frame).unwrap();
        hc42.blocking_flush().unwrap();
        led.toggle();
        Timer::after_secs(1).await;
    }
}
