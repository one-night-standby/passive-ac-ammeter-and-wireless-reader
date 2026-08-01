//! Bench calibration logger: the meter over BLE on one side, the reference
//! multimeter on the other, one CSV row per reading.
//!
//! Connects the way the Android reader does -- Nordic-style transparent
//! serial, service 0xFFE0 / characteristic 0xFFE1 -- so whatever the firmware
//! pushes out UART2 lands here as lines.
//!
//! **This drives the meter.** `MEAS` goes out on FFE1, the same line the phone
//! writes (`MeterPollingService.ask`) and the same one `main.rs` answers, so a
//! run calibrates the delivered firmware rather than a bench build of it. One
//! request is outstanding at a time, and the next only goes out once the
//! previous reading has been paired with the reference.
//!
//! One CSV row is one reading, and that has to hold even when the link does
//! not cooperate. A re-sent request the meter did hear is another answer owed,
//! and BLE delivers them when it feels like it, so two whole readings can turn
//! up inside one collection window. The boundary is therefore a *field
//! collision* -- a frame reporting a measurement this row already has belongs
//! to the next reading -- and the frame that trips it opens the next row with
//! its own reference reading rather than being merged or binned. Merging would
//! put one reading's RMS in a row whose amps came from another; binning would
//! throw away a bench point that cost a load change to produce.
//!
//! `bin/stream.rs` measures on its own timer and would talk over this; point
//! it at `main.rs`.
//!
//! The reference reading is taken *when the frame arrives*, not on a timer:
//! the current that matters is the one flowing while that measurement was
//! taken.
//!
//! This logs. It does not reduce, fit, average or judge -- what makes a bench
//! point is a decision about the bench, and the CSV is the evidence it gets
//! made from.

use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use btleplug::api::{
    Central, Manager as _, Peripheral as _, ScanFilter, ValueNotification, WriteType,
};
use btleplug::platform::{Manager, Peripheral};
use futures::stream::{Stream, StreamExt};
use uuid::Uuid;

use crate::dmm::Dmm;

/// Same transparent-serial UUIDs the Android reader uses
/// (`MeterPollingService.java`). The HC-42 exposes 0xFFE1 as notify+write.
const CHAR_UUID: Uuid = Uuid::from_u128(0x0000ffe1_0000_1000_8000_00805f9b34fb);

/// Advertised-name substrings that mark a candidate, matching
/// `MeterPollingService.isMeterCandidate` so the two agree on what a meter is.
const NAME_HINTS: &[&str] = &["HC-42", "HC42", "METER", "AMMETER", "BYJX_"];

/// One row per reading. `raw` is last because it is the wide one, and it is
/// written even when nothing parsed: a frame this tool did not understand is
/// still evidence, and losing it means repeating the bench run to get it back.
const CSV_FIELDS: &[&str] = &[
    "time",
    "dmm_a",
    "lag_ms",
    "rms_lsb",
    "gain",
    "flag",
    "current_ma",
    "status",
    "addr",
    "mean",
    "pp",
    "extra",
    "raw",
];

/// Frame key -> CSV column. Calibration needs the raw RMS and the gain it was
/// taken at; the already-scaled `CURRENT_MA` is the output of the table being
/// fitted, so it is logged as context, not as the datum. Keys are not fixed
/// here beyond this map -- a frame that grows a field still lands in `extra`
/// rather than being dropped.
fn field_for(key: &str) -> Option<&'static str> {
    Some(match key.to_ascii_uppercase().as_str() {
        "RMS" | "RMS_LSB" => "rms_lsb",
        "GAIN" => "gain",
        "FLAG" => "flag",
        "CURRENT_MA" => "current_ma",
        "STATUS" => "status",
        "ADDR" => "addr",
        "MEAN" | "MEAN_IN" => "mean",
        "PP" | "PP_IN" => "pp",
        _ => return None,
    })
}

/// The columns that mark a frame as carrying a measurement. Tested on the
/// payload rather than on the tag, for the same reason the keys are: what the
/// firmware calls its frames is its business. `METER_CAL` brings the RMS and
/// `METER_TEST` brings the scaled amps, so either is a bench point; `IMHERE`
/// parses cleanly and carries neither.
const READING_COLS: &[&str] = &["rms_lsb", "current_ma"];

/// How often to say out loud that a request is still unanswered. Only a
/// notice: the wait itself has no deadline.
const WAITING_NOTICE: Duration = Duration::from_secs(5);

pub struct Args {
    pub out: PathBuf,
    pub name: Option<String>,
    pub address: Option<String>,
    pub scan_secs: f64,
    pub addr: Option<u8>,
    pub dmm: String,
    pub dmm_conf: String,
    pub dmm_read: String,
    pub dmm_timeout: f64,
    pub gap_ms: u64,
    pub burst_ms: u64,
    pub echo: bool,
}

#[derive(Default)]
struct Row {
    cols: HashMap<&'static str, String>,
    tags: Vec<String>,
    extra: Vec<String>,
}

impl Row {
    fn has_reading(&self) -> bool {
        READING_COLS.iter().any(|c| self.cols.contains_key(c))
    }

    /// Whether `other` belongs to a *different* reading and must not be folded
    /// in: it repeats a tag this row already carries, or reports a measurement
    /// this row already has. One reading emits each of its frames once.
    fn collides_with(&self, other: &Row) -> bool {
        other.tags.iter().any(|t| self.tags.contains(t))
            || READING_COLS
                .iter()
                .any(|c| self.cols.contains_key(c) && other.cols.contains_key(c))
    }

    /// Folds a later frame of the same reading in. One measurement goes out as
    /// more than one frame and BLE hands them over in whatever notifications
    /// they land in, so they arrive as a burst rather than together.
    fn merge(&mut self, other: Row) {
        self.cols.extend(other.cols);
        self.tags.extend(other.tags);
        self.extra.extend(other.extra);
    }
}

/// `TAG,KEY=VALUE,KEY=VALUE...`, which is the shape of the `METER_*` frames
/// and of anything else the firmware may start sending.
fn parse_frame(line: &str) -> Option<Row> {
    let mut parts = line.split(',');
    let tag = parts.next()?;
    if tag.is_empty()
        || !tag.starts_with(|c: char| c.is_ascii_uppercase())
        || !tag
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }

    let mut row = Row::default();
    let mut extra = vec![tag.to_owned()];
    let mut pairs = 0;
    for part in parts {
        let (key, value) = part.split_once('=')?;
        if key.is_empty()
            || !key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        pairs += 1;
        match field_for(key) {
            Some(col) => {
                row.cols.insert(col, value.to_owned());
            }
            None => extra.push(part.to_owned()),
        }
    }
    if pairs == 0 {
        return None;
    }
    row.tags = vec![tag.to_owned()];
    row.extra = vec![extra.join(",")];
    Some(row)
}

/// Drains whole lines out of `pending`, leaving any partial tail behind.
///
/// BLE hands over MTU-sized chunks with no relation to line boundaries, so a
/// frame routinely arrives split across two notifications and two frames
/// routinely arrive in one.
fn split_lines(pending: &mut Vec<u8>, out: &mut VecDeque<String>) {
    while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = pending.drain(..=nl).collect();
        let text = String::from_utf8_lossy(&line[..line.len() - 1])
            .replace('\r', "")
            .trim()
            .to_owned();
        if !text.is_empty() {
            out.push_back(text);
        }
    }
}

enum Wait {
    Line(String),
    Timeout,
    Closed,
}

type Notifications = Pin<Box<dyn Stream<Item = ValueNotification> + Send>>;

struct Wire {
    notifs: Notifications,
    pending: Vec<u8>,
    queue: VecDeque<String>,
}

impl Wire {
    /// One line, or `Timeout` once `deadline` has passed.
    async fn line(&mut self, deadline: Instant) -> Wait {
        loop {
            if let Some(line) = self.queue.pop_front() {
                return Wait::Line(line);
            }
            let Some(budget) = deadline.checked_duration_since(Instant::now()) else {
                return Wait::Timeout;
            };
            match tokio::time::timeout(budget, self.notifs.next()).await {
                Ok(Some(n)) => {
                    self.pending.extend_from_slice(&n.value);
                    split_lines(&mut self.pending, &mut self.queue);
                }
                Ok(None) => return Wait::Closed,
                Err(_) => return Wait::Timeout,
            }
        }
    }

    /// Throws away whatever arrived while nothing was being asked for, so each
    /// request starts from silence -- the host side of `Link::discard_pending`.
    /// Returns how many whole lines went in the bin.
    ///
    /// Discards *lines*, never bytes. A notification carries whatever fraction
    /// of a frame the connection interval happened to fit, so dropping the
    /// buffer outright saws a frame in half and the remainder comes back as a
    /// headless `ADDR=6,CURRENT_MA=...` that parses as nothing. A partial tail
    /// is therefore waited out rather than dropped, and only a tail that never
    /// completes -- the meter stopped mid-frame -- is finally cleared.
    async fn discard_pending(&mut self) -> usize {
        const QUIET: Duration = Duration::from_millis(60);
        const STRANDED: Duration = Duration::from_millis(300);

        let mut dropped = 0;
        while let Wait::Line(_) = self.line(Instant::now() + QUIET).await {
            dropped += 1;
        }
        if !self.pending.is_empty() {
            let give_up = Instant::now() + STRANDED;
            while !self.pending.is_empty() {
                match self.line(give_up).await {
                    Wait::Line(_) => dropped += 1,
                    _ => break,
                }
            }
            self.pending.clear();
        }
        dropped
    }
}

/// CSV appender that flushes every row: a calibration run is long, interactive
/// and easy to interrupt, and a row that reached the file is a row that
/// survives.
struct Logger {
    file: std::fs::File,
    rows: usize,
}

impl Logger {
    fn open(path: &Path) -> Result<Self> {
        let header = CSV_FIELDS.join(",");
        let existing = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if existing > 0 {
            // Appending rows of one shape under a header of another produces a
            // file that parses without complaint and means something else.
            let first = BufReader::new(
                std::fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?,
            )
            .lines()
            .next()
            .transpose()?
            .unwrap_or_default();
            if first.trim() != header {
                bail!(
                    "{} 的表头和现在的列不一样，别往里追加。\n  文件: {}\n  现在: {}",
                    path.display(),
                    first.trim(),
                    header
                );
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("打开 {}", path.display()))?;
        if existing == 0 {
            writeln!(file, "{header}")?;
            file.flush()?;
        }
        Ok(Logger { file, rows: 0 })
    }

    fn write(&mut self, cells: &HashMap<&'static str, String>) -> Result<()> {
        let line: Vec<String> = CSV_FIELDS
            .iter()
            .map(|f| escape(cells.get(f).map(String::as_str).unwrap_or("")))
            .collect();
        writeln!(self.file, "{}", line.join(","))?;
        self.file.flush()?;
        self.rows += 1;
        Ok(())
    }
}

fn escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// Scans and prints what is in range, marking the meters.
pub async fn list_ble(scan_secs: f64) -> Result<()> {
    let central = adapter().await?;
    println!("[ble] 扫描 {scan_secs:.0}s ...");
    central.start_scan(ScanFilter::default()).await?;
    tokio::time::sleep(Duration::from_secs_f64(scan_secs)).await;
    for p in central.peripherals().await? {
        let name = local_name(&p).await;
        println!("  {}  {}", p.id(), name.unwrap_or_else(|| "(无名)".into()));
    }
    central.stop_scan().await?;
    Ok(())
}

async fn adapter() -> Result<btleplug::platform::Adapter> {
    let manager = Manager::new().await.context("初始化蓝牙")?;
    manager
        .adapters()
        .await
        .context("列出蓝牙适配器")?
        .into_iter()
        .next()
        .context("没有可用的蓝牙适配器")
}

async fn local_name(p: &Peripheral) -> Option<String> {
    p.properties().await.ok().flatten().and_then(|props| {
        props
            .local_name
            .or(props.advertisement_name)
            .filter(|n| !n.is_empty())
    })
}

fn is_candidate(name: &Option<String>, filter: Option<&str>) -> bool {
    let Some(name) = name else { return false };
    let upper = name.to_uppercase();
    match filter {
        Some(f) => upper.contains(&f.to_uppercase()),
        None => NAME_HINTS.iter().any(|h| upper.contains(h)),
    }
}

async fn find_meter(args: &Args) -> Result<Peripheral> {
    let central = adapter().await?;
    println!("[ble] 扫描 {:.0}s ...", args.scan_secs);
    central.start_scan(ScanFilter::default()).await?;
    tokio::time::sleep(Duration::from_secs_f64(args.scan_secs)).await;
    let peripherals = central.peripherals().await?;
    central.stop_scan().await.ok();

    // Matching on the advertised name is the portable way to pick the meter:
    // macOS hands out per-host UUIDs instead of MAC addresses. `--address`
    // exists for when the name is ambiguous.
    let mut matches = Vec::new();
    for p in peripherals {
        let name = local_name(&p).await;
        let hit = match &args.address {
            Some(a) => {
                let a = a.to_uppercase();
                p.id().to_string().to_uppercase().contains(&a)
                    || p.address().to_string().to_uppercase().contains(&a)
            }
            None => is_candidate(&name, args.name.as_deref()),
        };
        println!("  {}  {}", p.id(), name.unwrap_or_else(|| "(无名)".into()));
        if hit {
            matches.push(p);
        }
    }

    if matches.len() > 1 {
        println!(
            "[ble] {} 个候选，取第一个；要指定用 --address",
            matches.len()
        );
    }
    matches
        .into_iter()
        .next()
        .context("没找到电流表。list-ble 看看扫到了什么，或者用 --address 指定")
}

pub async fn run(args: Args) -> Result<()> {
    // The output file first: it is the one thing that can be wrong for free,
    // and finding out after an eight-second scan and an instrument handshake
    // is finding out too late.
    let mut log = Logger::open(&args.out)?;
    println!("[csv] {}", args.out.display());

    let meter = find_meter(&args).await?;

    let mut dmm = Some(Dmm::open(
        &args.dmm,
        &args.dmm_conf,
        Duration::from_secs_f64(args.dmm_timeout),
    )?);

    meter.connect().await.context("连接电流表")?;
    println!("[ble] 已连接 {}", meter.id());
    meter.discover_services().await.context("发现服务")?;
    let characteristic = meter
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == CHAR_UUID)
        .context("这个设备上没有 FFE1 透传特征值")?;
    meter
        .subscribe(&characteristic)
        .await
        .context("订阅 FFE1")?;

    let mut wire = Wire {
        notifs: meter.notifications().await.context("取通知流")?,
        pending: Vec::new(),
        queue: VecDeque::new(),
    };

    let command = match args.addr {
        Some(n) => format!("MEAS,ADDR={n}\n"),
        None => "MEAS\n".to_owned(),
    };
    // Write-without-response is what the phone uses. Resolved on the first
    // write and then left alone: a stack that refuses it refuses every one.
    let mut write_type = WriteType::WithoutResponse;

    // Asking for the signal replaces the process default, so nothing else will
    // ever kill this on ctrl-C: the second press has to do it here. The first
    // one is a request to stop after the reading in flight, which is what
    // leaves the CSV ending on a whole row.
    let quit = Arc::new(AtomicBool::new(false));
    {
        let quit = quit.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            println!("\n[收尾] 这一发读完就停，再按一次 ctrl-C 立刻退出");
            quit.store(true, Ordering::Relaxed);
            let _ = tokio::signal::ctrl_c().await;
            std::process::exit(130);
        });
    }
    let mut keys = keyboard();
    let mut paused = false;

    println!(
        "[就绪] 自动发 {}，回车 暂停/继续，q 回车 退出\n",
        command.trim()
    );

    let burst = Duration::from_millis(args.burst_ms);
    let gap = Duration::from_millis(args.gap_ms);

    /// A frame that turned up during one reading's burst but belongs to the
    /// next one. It is a whole reading with its own arrival time, so it opens
    /// the next row rather than being thrown away -- and while one is in hand
    /// there is nothing to request and nothing to wait out.
    struct Carry {
        arrived: Instant,
        row: Row,
        line: String,
    }
    let mut carry: Option<Carry> = None;

    while !quit.load(Ordering::Relaxed) {
        while let Ok(key) = keys.try_recv() {
            match key.trim() {
                "q" | "Q" => quit.store(true, Ordering::Relaxed),
                _ => {
                    paused = !paused;
                    println!(
                        "{}",
                        if paused {
                            "[暂停] 回车继续"
                        } else {
                            "[继续]"
                        }
                    );
                }
            }
        }
        if quit.load(Ordering::Relaxed) {
            break;
        }
        if paused {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        // A reading already in hand needs no request: asking for another one on
        // top of it is how a backlog gets built.
        let (arrived, mut row, first_line) = match carry.take() {
            Some(c) => (c.arrived, c.row, c.line),
            None => {
                // Whatever is on the wire now arrived after the last reading
                // was paired and logged, so it belongs to no request: the
                // power-up frames on the first pass, a heartbeat after that.
                let dropped = wire.discard_pending().await;
                if dropped > 0 {
                    println!("     丢掉 {dropped} 行没人要的");
                }
                if let Err(e) = meter
                    .write(&characteristic, command.as_bytes(), write_type)
                    .await
                {
                    if write_type == WriteType::WithoutResponse {
                        println!("     无应答写被拒（{e}），改用有应答写");
                        write_type = WriteType::WithResponse;
                    } else {
                        println!("     发 {} 失败: {e}", command.trim());
                    }
                    continue;
                }
                let sent = Instant::now();

                // Wait for the answer, however long it takes. There is no
                // re-send: every request the meter hears is an answer owed and
                // they all arrive eventually, so a second `MEAS` does not
                // replace a missing reading -- it adds one nobody is holding a
                // reference value for, and every pairing until the backlog
                // drains belongs to the wrong measurement.
                //
                // Nothing should go missing anyway while `--gap-ms` keeps the
                // send inside the meter's listening window. If the wire really
                // does go quiet, the bench needs to be told that rather than
                // handed rows built on it.
                let mut waited = 0u64;
                loop {
                    match wire.line(Instant::now() + WAITING_NOTICE).await {
                        Wait::Line(line) => {
                            if args.echo {
                                // Stamped against the request: how long the
                                // answer took is the number that says whether
                                // the meter is slow or the link is holding
                                // frames back.
                                println!("     +{:>5}ms {line}", sent.elapsed().as_millis());
                            }
                            match parse_frame(&line) {
                                Some(frame) if frame.has_reading() => {
                                    break (Instant::now(), frame, line);
                                }
                                Some(_) => {}
                                None => println!("     解析不了: {line}"),
                            }
                        }
                        Wait::Timeout => {
                            waited += WAITING_NOTICE.as_secs();
                            println!(
                                "     {} 发出去 {waited}s 了还没读数（不重发，ctrl-C 停）",
                                command.trim()
                            );
                        }
                        Wait::Closed => bail!("蓝牙通知流断了"),
                    }
                }
            }
        };

        // Ask the reference *before* collecting the rest of the burst: every
        // millisecond between the meter's measurement window and the
        // instrument's is a millisecond the load had to change in, and the
        // collection below is free while the instrument integrates.
        let read_cmd = args.dmm_read.clone();
        let mut taken = dmm.take().expect("dmm is put back every iteration");
        let reading = tokio::task::spawn_blocking(move || {
            let amps = taken.read(&read_cmd);
            // Timed in here, where the read actually ends. Measuring it after
            // the burst collection below would report the collection window
            // instead of the gap between the two measurements.
            (taken, amps, arrived.elapsed())
        });

        let mut raw = first_line;
        let burst_end = Instant::now() + burst;
        loop {
            match wire.line(burst_end).await {
                Wait::Line(line) => {
                    if args.echo {
                        println!("     {line}");
                    }
                    let Some(frame) = parse_frame(&line) else {
                        println!("     解析不了: {line}");
                        continue;
                    };
                    if row.collides_with(&frame) {
                        // A measurement this row already reports, so the frame
                        // is the start of the *next* reading. Merging it would
                        // put its RMS in a row whose reference reading belongs
                        // to the other one; dropping it would throw away a
                        // whole bench point. It opens the next row instead.
                        carry = Some(Carry {
                            arrived: Instant::now(),
                            row: frame,
                            line,
                        });
                        break;
                    }
                    raw = format!("{raw} | {line}");
                    row.merge(frame);
                }
                Wait::Timeout => break,
                Wait::Closed => bail!("蓝牙通知流断了"),
            }
        }

        let (returned, amps, lag) = reading.await.context("万用表任务崩了")?;
        dmm = Some(returned);
        let amps = match amps {
            Ok(a) => a,
            Err(e) => {
                // Keep the run alive and log the gap: a row with no reference
                // still records what the meter said.
                println!("     万用表读数失败: {e}");
                None
            }
        };

        let mut cells = row.cols;
        cells.insert(
            "time",
            chrono::Local::now()
                .format("%Y-%m-%dT%H:%M:%S%.3f")
                .to_string(),
        );
        cells.insert("dmm_a", amps.map(|a| format!("{a:.6}")).unwrap_or_default());
        // How far the reference reading trails the frame that asked for it. A
        // row whose lag is a large fraction of the meter's period was paired
        // across a gap the bench should know about.
        cells.insert("lag_ms", format!("{}", lag.as_millis()));
        cells.insert("extra", row.extra.join(","));
        cells.insert("raw", raw);
        log.write(&cells)?;

        let shown: Vec<String> = ["rms_lsb", "gain", "flag", "current_ma"]
            .iter()
            .filter_map(|f| cells.get(f).map(|v| format!("{f}={v}")))
            .collect();
        let amps_txt = amps
            .map(|a| format!("{a:.6} A"))
            .unwrap_or_else(|| "----".into());
        println!("  {:<4} {amps_txt:>14}   {}", log.rows, shown.join(" "));

        // `main.rs` blanks pending commands the moment it answers and then
        // holds the panel for DISPLAY_ON_MS before it reads the link again, so
        // a request sent now is simply thrown away.
        //
        // Timed from when the meter answered, not from here: the reference
        // reading and the burst collection have already spent several hundred
        // milliseconds of that window, and waiting it out twice would idle the
        // bench for no reason.
        //
        // Skipped entirely when a reading is already in hand: the wait exists
        // to keep the next request from landing while the meter is still lit,
        // and there is no next request until the backlog is worked off.
        if carry.is_none()
            && let Some(left) = (arrived + gap).checked_duration_since(Instant::now())
        {
            tokio::time::sleep(left).await;
        }
    }

    meter.unsubscribe(&characteristic).await.ok();
    meter.disconnect().await.ok();
    println!("\n[完成] {} 行 -> {}", log.rows, args.out.display());
    Ok(())
}

/// Line-at-a-time keyboard control, off the runtime.
///
/// Plain line reads rather than raw mode: the only two controls are a toggle
/// and a quit, and a terminal left in raw mode by a process that died badly is
/// worse than pressing Enter.
fn keyboard() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}
