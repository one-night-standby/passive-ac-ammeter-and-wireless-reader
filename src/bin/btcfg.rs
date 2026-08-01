#![no_std]
#![no_main]
// 共用模块里这个 bin 用不到的东西一概是 dead_code。
#![allow(dead_code)]

//! 一次性工具:把 HC-42 的串口波特率从出厂的 9600 改成固件用的
//! [`link::BT_BAUD_RATE`],并读回来验证。
//!
//! 烧一次、看一遍 XDS110 虚拟串口的输出、然后烧回正式固件即可。改动掉电不
//! 丢失(HC42.pdf 5.1),所以只需要做一次;换模块或者模块被恢复出厂设置之后
//! 要重做。
//!
//! 两条手册里的硬约束,做错了都是「毫无反应」而不是报错:
//!
//! - **AT 指令不带回车换行**(5.1 注)。加了终止符,指令不成立,而指令不成立
//!   时模块什么都不返回。
//! - **必须处于未连线状态**。连上之后模块进入透传,AT 指令会被当成数据原样
//!   转发给对端。跑这个工具之前先把手机断开。
//!
//! 输出在 UART0/PA10,115200 8N1,走板载 XDS110 的虚拟串口——和 `oparails`
//! 同一个口,与被配置的 UART2 分开。

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_mspm0::gpio::{Level, Output};
use embassy_mspm0::mode::Blocking;
use embassy_mspm0::uart::{BufferedInterruptHandler, BufferedUart, Config as UartConfig, UartTx};
use embassy_mspm0::{bind_interrupts, peripherals};
use embassy_time::Timer;
use embedded_io_async::Read;
use panic_halt as _;

// 只为了 link::BT_BAUD_RATE 一个常量,但值得把它的依赖一并拉进来:波特率是
// 固件和模块之间的约定,在两个地方各写一份,正是会让模块变哑的那种漂移。
#[path = "../cal.rs"]
mod cal;
#[path = "../dac.rs"]
mod dac;
#[path = "../dsp.rs"]
mod dsp;
#[path = "../link.rs"]
mod link;
#[path = "../meter.rs"]
mod meter;
#[path = "../nvcal.rs"]
mod nvcal;
#[path = "../range.rs"]
mod range;
#[path = "../sampler.rs"]
mod sampler;
#[path = "../vref.rs"]
mod vref;

bind_interrupts!(struct Irqs {
    UART2 => BufferedInterruptHandler<peripherals::UART2>;
});

const CONSOLE_BAUD_RATE: u32 = 115_200;

/// 工具用哪个波特率跟模块说话。
///
/// 默认就是固件用的那个:模块改过一次就停在那里(掉电不丢失),所以这是常态,
/// 跑一遍等于确认整条链是通的。**拿到一块出厂状态的新模块时**,把这里改成
/// 9600 烧一次——工具发现它不在目标波特率上就会把它改过去——然后再改回来。
///
/// 只按一个波特率建一次 UART,不做重配:台面上试过重配活着的 BufferedUart,
/// `set_baudrate` 报 OK,但之后两个方向都不通了。全新构造没有这个问题,而
/// 正式固件走的也正是全新构造这条路。
const PROBE_BAUD_RATE: u32 = link::BT_BAUD_RATE;

/// 手册 5.1:模块启动约 300 ms,建议上电或复位 350 ms 之后再发 AT 指令。
const BOOT_MS: u64 = 350;

/// 单条指令等回应的时限。AT 指令是本地处理的,不过无线,几十毫秒就该回来。
const REPLY_MS: u64 = 800;

static mut TX_BUF: [u8; 64] = [0; 64];

static mut RX_BUF: [u8; 64] = [0; 64];

/* 每次尝试一套独立的缓冲。同一对 static 用两次会同时存在两个 &mut,而且会
把 UART 的生命周期钉成 'static、反过来卡住 Peri 的借用。一次性工具,
256 字节换一个能编译的干净结构值得。 */
static mut TX_BUF_HIGH: [u8; 64] = [0; 64];

static mut RX_BUF_HIGH: [u8; 64] = [0; 64];

static mut TX_BUF_LOW: [u8; 64] = [0; 64];

static mut RX_BUF_LOW: [u8; 64] = [0; 64];

struct Console<'d>(UartTx<'d, Blocking>);

impl core::fmt::Write for Console<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for part in s.split_inclusive('\n') {
            match part.strip_suffix('\n') {
                Some(head) => {
                    let _ = self.0.blocking_write(head.as_bytes());
                    let _ = self.0.blocking_write(b"\r\n");
                }
                None => {
                    let _ = self.0.blocking_write(part.as_bytes());
                }
            }
        }
        Ok(())
    }
}

/// 发一条 AT 指令,把 `REPLY_MS` 之内收到的东西原样打出来。返回收到的字节数。
///
/// 不解析回应,只转述:模块在指令不成立时什么都不返回,所以「收到几个字节」
/// 本身就是最重要的信息,比把回应套进某个期望的格式有用。
async fn at(console: &mut Console<'_>, bt: &mut BufferedUart<'static>, command: &str) -> usize {
    let _ = write!(console, "  -> {}  ", command);
    // 不带任何终止符:手册 5.1 明写 AT 指令一律不采用换行发送。
    let _ = bt.blocking_write(command.as_bytes());

    let mut got = 0usize;
    let mut buf = [0u8; 32];
    loop {
        match select(bt.read(&mut buf), Timer::after_millis(REPLY_MS)).await {
            Either::First(Ok(n)) if n > 0 => {
                got += n;
                for &byte in &buf[..n] {
                    // 回应里的 CR/LF 打成可见的占位,免得把控制台的排版搅乱。
                    let _ = match byte {
                        b'\r' | b'\n' => write!(console, "."),
                        0x20..=0x7e => write!(console, "{}", byte as char),
                        _ => write!(console, "?"),
                    };
                }
            }
            Either::First(_) => break,
            Either::Second(()) => break,
        }
    }
    if got == 0 {
        let _ = write!(console, "(无回应)");
    }
    let _ = writeln!(console);
    got
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    let p = embassy_mspm0::init(Default::default());

    let mut console_config = UartConfig::default();
    console_config.baudrate = CONSOLE_BAUD_RATE;
    let mut console = Console(UartTx::new_blocking(p.UART0, p.PA10, console_config).unwrap());

    let _ = writeln!(&mut console, "\n# btcfg: 把 HC-42 的串口波特率改成 115200");
    let _ = writeln!(
        &mut console,
        "# 前提:手机/读表器必须先断开,模块处于未连线状态"
    );

    // 射频上电,等它启动完。这里的 350 ms 和 link.rs 的 BT_SETTLE_MS 同源。
    let mut power = Output::new(p.PB17, Level::High);
    Timer::after_millis(BOOT_MS).await;

    let mut config = UartConfig::default();
    config.baudrate = PROBE_BAUD_RATE;
    let mut bt = BufferedUart::new(
        p.UART2,
        p.PB15,
        p.PB16,
        Irqs,
        unsafe { &mut *core::ptr::addr_of_mut!(TX_BUF) },
        unsafe { &mut *core::ptr::addr_of_mut!(RX_BUF) },
        config,
    )
    .unwrap();

    let _ = writeln!(&mut console, "\n[1] 以 {} 找模块", PROBE_BAUD_RATE);
    if at(&mut console, &mut bt, "AT").await == 0 {
        let _ = writeln!(
            &mut console,
            "\n# {} 上问不到模块。三种可能:\n\
             #   模块在别的波特率上——改 PROBE_BAUD_RATE 重烧;\n\
             #   手机还连着——连上后模块进透传,AT 会被当数据转发给对端;\n\
             #   供电或 PB15/PB16 接线的问题。",
            PROBE_BAUD_RATE
        );
    } else {
        let _ = writeln!(&mut console, "\n[2] 查当前波特率");
        at(&mut console, &mut bt, "AT+UART").await;

        if PROBE_BAUD_RATE == link::BT_BAUD_RATE {
            let _ = writeln!(
                &mut console,
                "\n# 模块已经在固件要用的 {} 上。烧回正式固件即可",
                link::BT_BAUD_RATE
            );
        } else {
            let _ = writeln!(&mut console, "\n[3] 改成 {}", link::BT_BAUD_RATE);
            // 从常量拼指令,而不是把数字再写一遍:这条指令和固件用的波特率
            // 必须是同一个数,写两份就是让它们有机会分家。
            let mut command: heapless::String<24> = heapless::String::new();
            let _ = write!(command, "AT+UART={}", link::BT_BAUD_RATE);
            at(&mut console, &mut bt, &command).await;
            let _ = writeln!(
                &mut console,
                "\n# 模块收到 OK+UART= 就是改成功了(掉电不丢失)。\n\
                 # 把 PROBE_BAUD_RATE 改回 link::BT_BAUD_RATE 再跑一遍可以验证。"
            );
        }
    }

    // 配置完就把射频断电,别让它在这块无所事事的固件下面一直耗着。
    power.set_low();
    let _ = writeln!(&mut console, "\n# 射频已断电,可以烧回固件了");
    loop {
        Timer::after_millis(1_000).await;
    }
}
