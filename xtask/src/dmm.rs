//! The reference instrument for a calibration run: an SDM3055X-E, over its
//! USB (USBTMC) port or its LAN (raw SCPI) port.
//!
//! Configured once on open and then read with a bare `READ?`, so the function
//! and range cannot be renegotiated between two points of the same run -- a
//! `MEAS:CURR:AC?` per sample would let the instrument re-autorange midway and
//! silently change what the later rows mean.
//!
//! Everything here blocks. `callog` keeps it on a blocking thread so the BLE
//! side keeps running while the meter integrates.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::usbtmc::Usbtmc;

/// What the multimeter calls itself in `*IDN?`. The bench has more than one
/// Siglent instrument on USB and their descriptors differ only by product ID
/// and serial, so picking by position would eventually send `CONF:CURR:AC` to
/// the oscilloscope. Asking each candidate who it is costs one exchange and
/// cannot be got wrong by reordering a hub.
const IDN_HINT: &str = "SDM";

const DEFAULT_SCPI_PORT: u16 = 5025;

pub enum Dmm {
    /// No reference instrument: log the meter alone.
    None,
    Usb {
        tmc: Usbtmc,
        label: String,
    },
    Tcp {
        conn: TcpConn,
        label: String,
    },
}

impl Dmm {
    /// `spec` is `none`, `usb`, `usb:VID:PID` (hex), or `tcp://host[:port]`.
    pub fn open(spec: &str, conf: &str, timeout: Duration) -> Result<Self> {
        let dmm = match spec {
            "" | "none" => Dmm::None,
            "usb" => Self::open_usb(None)?,
            s if s.starts_with("usb:") => {
                let mut parts = s[4..].split(':');
                let vid = parts.next().context("usb:VID:PID 缺 VID")?;
                let pid = parts.next().context("usb:VID:PID 缺 PID")?;
                let vid = u16::from_str_radix(vid.trim_start_matches("0x"), 16)
                    .with_context(|| format!("VID {vid:?} 不是十六进制"))?;
                let pid = u16::from_str_radix(pid.trim_start_matches("0x"), 16)
                    .with_context(|| format!("PID {pid:?} 不是十六进制"))?;
                Self::open_usb(Some((vid, pid)))?
            }
            s => {
                let hostport = s.strip_prefix("tcp://").unwrap_or(s);
                let (host, port) = match hostport.rsplit_once(':') {
                    Some((h, p)) => (h, p.parse().unwrap_or(DEFAULT_SCPI_PORT)),
                    None => (hostport, DEFAULT_SCPI_PORT),
                };
                let conn = TcpConn::open(host, port, timeout)?;
                Dmm::Tcp {
                    label: format!("tcp://{host}:{port}"),
                    conn,
                }
            }
        };

        let mut dmm = dmm;
        if !matches!(dmm, Dmm::None) {
            let idn = dmm.query("*IDN?")?;
            println!("[dmm] {}  {idn}", dmm.label());
            if !conf.is_empty() {
                dmm.write(conf)?;
                // Let the configure land before the first READ?, which on a
                // freshly ranged instrument is the reading most likely to be
                // stale.
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        Ok(dmm)
    }

    fn open_usb(vid_pid: Option<(u16, u16)>) -> Result<Self> {
        let candidates = Usbtmc::candidates()?;
        if candidates.is_empty() {
            bail!("USB 上没找到仪器。--list-dmm 看看枚举到了什么");
        }

        if let Some((vid, pid)) = vid_pid {
            let info = candidates
                .iter()
                .find(|d| d.vendor_id() == vid && d.product_id() == pid)
                .with_context(|| format!("USB 上没有 {vid:04x}:{pid:04x}"))?;
            return Ok(Dmm::Usb {
                tmc: Usbtmc::open_reset(info)?,
                label: format!("usb:{vid:04x}:{pid:04x}"),
            });
        }

        // Ask each candidate who it is. A device that will not answer is not
        // the multimeter as far as this run is concerned -- said out loud
        // rather than skipped silently, because "the DMM was on the other USB
        // port" and "the DMM is wedged" look identical from here.
        let mut seen = Vec::new();
        for info in &candidates {
            let (vid, pid) = (info.vendor_id(), info.product_id());
            match Usbtmc::open(info).and_then(|mut t| {
                let idn = t.query("*IDN?")?;
                Ok((t, idn))
            }) {
                Ok((tmc, idn)) if idn.to_uppercase().contains(IDN_HINT) => {
                    return Ok(Dmm::Usb {
                        tmc,
                        label: format!("usb:{vid:04x}:{pid:04x}"),
                    });
                }
                Ok((_, idn)) => seen.push(format!("{vid:04x}:{pid:04x} {idn}")),
                Err(e) => seen.push(format!("{vid:04x}:{pid:04x} (问不出来: {e})")),
            }
        }
        bail!(
            "USBTMC 设备里没有 *IDN? 含 {IDN_HINT} 的。枚举到的是:\n  {}\n\
             要指定就用 --dmm usb:VID:PID",
            seen.join("\n  ")
        )
    }

    pub fn label(&self) -> &str {
        match self {
            Dmm::None => "none",
            Dmm::Usb { label, .. } | Dmm::Tcp { label, .. } => label,
        }
    }

    fn write(&mut self, command: &str) -> Result<()> {
        match self {
            Dmm::None => Ok(()),
            Dmm::Usb { tmc, .. } => tmc.write(command),
            Dmm::Tcp { conn, .. } => conn.write(command),
        }
    }

    fn query(&mut self, command: &str) -> Result<String> {
        match self {
            Dmm::None => Ok(String::new()),
            Dmm::Usb { tmc, .. } => tmc.query(command),
            Dmm::Tcp { conn, .. } => conn.query(command),
        }
    }

    /// One reference reading in amps, or `None` when there is no instrument.
    pub fn read(&mut self, command: &str) -> Result<Option<f64>> {
        if matches!(self, Dmm::None) {
            return Ok(None);
        }
        let reply = self.query(command)?;
        let value = reply.trim();
        value
            .parse()
            .map(Some)
            .with_context(|| format!("万用表回了 {value:?}，不是一个数"))
    }
}

/// Prints every USBTMC candidate with its own answer to who it is, so a spec
/// can be copied rather than guessed.
pub fn list() -> Result<()> {
    let candidates = Usbtmc::candidates()?;
    if candidates.is_empty() {
        println!("USB 上没枚举到仪器。");
        return Ok(());
    }
    for info in &candidates {
        let (vid, pid) = (info.vendor_id(), info.product_id());
        let who = match Usbtmc::open(info).and_then(|mut t| t.query("*IDN?")) {
            Ok(idn) => idn,
            Err(e) => format!("(问不出来: {e})"),
        };
        println!("  usb:{vid:04x}:{pid:04x}\n      {who}");
    }
    Ok(())
}

/// Raw SCPI over a socket, one command or query per line.
pub struct TcpConn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl TcpConn {
    fn open(host: &str, port: u16, timeout: Duration) -> Result<Self> {
        let addr = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("解析 {host}:{port}"))?
            .next()
            .with_context(|| format!("{host}:{port} 解析不出地址"))?;
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .with_context(|| format!("连 {host}:{port}"))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        Ok(TcpConn {
            reader: BufReader::new(stream.try_clone().context("cloning socket")?),
            writer: stream,
        })
    }

    fn write(&mut self, command: &str) -> Result<()> {
        self.writer
            .write_all(format!("{command}\n").as_bytes())
            .context("SCPI 写失败")?;
        self.writer.flush().context("SCPI 刷新失败")
    }

    fn query(&mut self, command: &str) -> Result<String> {
        self.write(command)?;
        let mut line = String::new();
        self.reader.read_line(&mut line).context("SCPI 读失败")?;
        Ok(line.trim_end().to_owned())
    }
}
