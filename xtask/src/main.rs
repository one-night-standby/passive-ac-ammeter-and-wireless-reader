mod callog;
mod dmm;
mod scope;
mod usbtmc;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Bench tooling: the DSO (Siglent SDS2102X Plus), the DMM (SDM3055X-E), and
/// the calibration logger that pairs the meter against the DMM.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save a screenshot of the scope's display as a PNG.
    ScopeShot {
        /// Output file path.
        #[arg(short, long, default_value = "scope.png")]
        output: PathBuf,
    },
    /// Export one channel's on-screen waveform as CSV (sample,volts).
    ScopeDump {
        /// Channel to read, e.g. C1, C2.
        #[arg(short, long, default_value = "C1")]
        channel: String,
        /// Output file path.
        #[arg(short, long, default_value = "waveform.csv")]
        output: PathBuf,
    },
    /// Send a raw SCPI query to the scope and print the reply (debugging aid).
    ScopeQuery { command: String },

    /// 列出 VISA/USBTMC 能看到的仪器和它们的 *IDN?，不做别的。
    ListDmm,
    /// 扫描蓝牙并列出设备，不连接。
    ListBle {
        /// 扫描秒数。
        #[arg(long, default_value_t = 8.0)]
        scan_secs: f64,
    },

    /// 标定用：自动发 MEAS 驱动电流表，每次读数配一次万用表，成对存 CSV。
    ///
    /// 对着 main.rs 用（stream.rs 自己按 PERIOD_MS 跑，会跟这里抢）。
    CalLog {
        /// CSV 输出路径，存在则追加（表头必须对得上）。
        #[arg(long, default_value = "cal.csv")]
        out: PathBuf,
        /// 按广播名子串挑设备，默认认 HC-42/METER/BYJX_ 等。
        #[arg(long)]
        name: Option<String>,
        /// 直接指定 BLE 地址（macOS 上是 UUID），跳过按名字挑。
        #[arg(long)]
        address: Option<String>,
        /// 扫描秒数。
        #[arg(long, default_value_t = 8.0)]
        scan_secs: f64,
        /// 只让编码开关等于这个值的表应答，发 MEAS,ADDR=n；不给就广播。
        #[arg(long)]
        addr: Option<u8>,
        /// 万用表：usb（默认，按 *IDN? 找 SDM）、usb:VID:PID、tcp://IP[:5025]、none。
        #[arg(long, default_value = "usb")]
        dmm: String,
        /// 连上后配置一次的 SCPI（空字符串表示不配置）。
        #[arg(long, default_value = "CONF:CURR:AC")]
        dmm_conf: String,
        /// 每次取数的 SCPI（某些机型用 MEAS:CURR:AC?）。
        #[arg(long, default_value = "READ?")]
        dmm_read: String,
        /// 万用表超时秒数。
        #[arg(long, default_value_t = 10.0)]
        dmm_timeout: f64,
        /// 从电流表答话算起，等多久再发下一条 MEAS，毫秒。
        ///
        /// main.rs 出数后先清掉待处理命令，再亮屏 DISPLAY_ON_MS（1000 ms）才回来收
        /// 命令，所以立刻发出去的那条必然被丢，而这里没有重发，会一直等下去。
        #[arg(long, default_value_t = 1100)]
        gap_ms: u64,
        /// 同一次读数的多帧合并窗口，毫秒（一次测量发 METER_TEST + METER_CAL 两行）。
        ///
        /// 两行之间可能隔一个蓝牙连接间隔，窗口太短就会把一次读数劈成两行。宁可宽：
        /// 下一次读数是靠字段撞车认出来的，不是靠这个窗口。
        #[arg(long, default_value_t = 600)]
        burst_ms: u64,
        /// 打印收到的每一行原文。
        #[arg(long)]
        echo: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::ScopeShot { output } => {
            let mut dso = scope::Scope::open()?;
            let png = dso.screenshot_png()?;
            std::fs::write(&output, png)
                .with_context(|| format!("writing {}", output.display()))?;
            println!("saved {}", output.display());
        }
        Command::ScopeDump { channel, output } => {
            let mut dso = scope::Scope::open()?;
            let samples = dso.dump_waveform(&channel)?;
            let mut csv = String::from("sample,volts\n");
            for (i, v) in samples {
                csv.push_str(&format!("{i},{v}\n"));
            }
            std::fs::write(&output, csv)
                .with_context(|| format!("writing {}", output.display()))?;
            println!("saved {}", output.display());
        }
        Command::ScopeQuery { command } => {
            let mut dso = scope::Scope::open()?;
            println!("{}", dso.query(&command)?);
        }
        Command::ListDmm => dmm::list()?,
        Command::ListBle { scan_secs } => block_on(callog::list_ble(scan_secs))?,
        Command::CalLog {
            out,
            name,
            address,
            scan_secs,
            addr,
            dmm,
            dmm_conf,
            dmm_read,
            dmm_timeout,
            gap_ms,
            burst_ms,
            echo,
        } => block_on(callog::run(callog::Args {
            out,
            name,
            address,
            scan_secs,
            addr,
            dmm,
            dmm_conf,
            dmm_read,
            dmm_timeout,
            gap_ms,
            burst_ms,
            echo,
        }))?,
    }
    Ok(())
}

/// Only the BLE paths need an async runtime; the instrument paths are blocking
/// USB and stay that way.
fn block_on<F: std::future::Future<Output = Result<()>>>(future: F) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        .block_on(future)
}
