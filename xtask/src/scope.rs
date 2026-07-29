//! Minimal USBTMC client for the bench oscilloscope (Siglent SDS2102X
//! Plus), covering the two things `xtask` needs: a screen dump and a raw
//! waveform export. There's no NI-VISA runtime installed, and USBTMC's
//! bulk-transfer framing is simple enough to talk to the device's USBTMC
//! interface (class 0xFE, subclass 0x03) directly via `nusb`.

use anyhow::{Context, Result, bail};

const SIGLENT_VID: u16 = 0xF4EC;
const SDS2000XPLUS_PID: u16 = 0x1011;

const USBTMC_CLASS: u8 = 0xFE;
const USBTMC_SUBCLASS: u8 = 0x03;

const MSG_DEV_DEP_MSG_OUT: u8 = 1;
const MSG_REQUEST_DEV_DEP_MSG_IN: u8 = 2;
const MSG_DEV_DEP_MSG_IN: u8 = 2;
const EOM: u8 = 0x01;

pub struct Scope {
    interface: nusb::Interface,
    ep_out: u8,
    ep_in: u8,
    next_tag: u8,
}

impl Scope {
    /// Finds the first Siglent SDS2000X Plus on the bus.
    fn find() -> Result<nusb::DeviceInfo> {
        nusb::list_devices()
            .context("listing USB devices")?
            .find(|d| d.vendor_id() == SIGLENT_VID && d.product_id() == SDS2000XPLUS_PID)
            .context("no Siglent SDS2000X Plus found on USB -- is it connected and powered on?")
    }

    /// Opens the scope's USBTMC interface, ready for `write`/`query`.
    pub fn open() -> Result<Self> {
        let device = Self::find()?.open().context("opening USB device")?;
        // A previous run that died mid-transfer can leave an endpoint
        // STALLed in a way plain clear_halt won't recover from; a full bus
        // reset guarantees a clean slate before we touch anything else.
        // The device drops off and re-enumerates, so re-find/re-open it
        // afterwards rather than reusing this handle.
        device.reset().context("resetting USB device")?;
        drop(device);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let device = Self::find()?.open().context("re-opening USB device after reset")?;

        let mut interface_num = None;
        let mut ep_out = None;
        let mut ep_in = None;
        for config in device.configurations() {
            for iface in config.interface_alt_settings() {
                if iface.class() == USBTMC_CLASS && iface.subclass() == USBTMC_SUBCLASS {
                    interface_num = Some(iface.interface_number());
                    for ep in iface.endpoints() {
                        // The USBTMC interface also has an interrupt-IN
                        // endpoint (status notifications); only the bulk
                        // pair carries DEV_DEP_MSG_* traffic.
                        if ep.transfer_type() != nusb::transfer::EndpointType::Bulk {
                            continue;
                        }
                        match ep.direction() {
                            nusb::transfer::Direction::Out => ep_out = Some(ep.address()),
                            nusb::transfer::Direction::In => ep_in = Some(ep.address()),
                        }
                    }
                }
            }
        }

        let interface_num = interface_num.context("device has no USBTMC interface")?;
        let interface = device
            .claim_interface(interface_num)
            .context("claiming USBTMC interface")?;
        let ep_in = ep_in.context("USBTMC interface has no bulk IN endpoint")?;
        let ep_out = ep_out.context("USBTMC interface has no bulk OUT endpoint")?;

        // A prior run that errored out mid-transfer (e.g. this program got
        // killed partway through reading a multi-megabyte screen dump)
        // leaves the device's USBTMC state machine expecting the host to
        // keep draining a response it never finished sending. Without
        // this, the *next* command's reply comes back corrupted/stale.
        // INITIATE_CLEAR + CHECK_CLEAR_STATUS (USBTMC 1.0 4.2.1.4/4.2.1.5)
        // resets that state before we send anything.
        // Best-effort: some USBTMC firmware (this scope included, it seems)
        // STALLs INITIATE_CLEAR outright rather than implementing it. The
        // device.reset() above already guarantees a clean slate, so don't
        // treat this as fatal.
        if let Err(e) = usbtmc_clear(&interface, interface_num as u16) {
            eprintln!("note: USBTMC clear handshake not supported by device ({e}), continuing");
        }
        interface.clear_halt(ep_out).context("clearing bulk OUT halt")?;
        interface.clear_halt(ep_in).context("clearing bulk IN halt")?;

        Ok(Scope {
            interface,
            ep_out,
            ep_in,
            next_tag: 1,
        })
    }

    fn next_tag(&mut self) -> u8 {
        let tag = self.next_tag;
        self.next_tag = if tag == 255 { 1 } else { tag + 1 };
        tag
    }

    /// Sends a SCPI command/query with no response read back.
    pub fn write(&mut self, command: &str) -> Result<()> {
        let tag = self.next_tag();
        let payload = command.as_bytes();
        let mut msg = Vec::with_capacity(12 + payload.len() + 3);
        msg.push(MSG_DEV_DEP_MSG_OUT);
        msg.push(tag);
        msg.push(!tag);
        msg.push(0);
        msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        msg.push(EOM);
        msg.extend_from_slice(&[0, 0, 0]);
        msg.extend_from_slice(payload);
        while msg.len() % 4 != 0 {
            msg.push(0);
        }
        let completion = pollster::block_on(self.interface.bulk_out(self.ep_out, msg));
        completion.status.context("USB bulk OUT transfer failed")
    }

    /// Reads a DEV_DEP_MSG_IN response of up to `max_len` bytes.
    ///
    /// This is a two-level protocol: a single logical message (ending when
    /// its header's EOM bit is set) can still be split across many raw USB
    /// packets below the 64/512-byte endpoint max-packet-size -- that
    /// splitting is *not* re-headered, so a short `bulk_in` read partway
    /// through a message's declared `transfer_size` just means "keep
    /// reading raw bytes," not "message over." Only once `transfer_size`
    /// bytes of payload have actually been accumulated does EOM get
    /// consulted to decide whether to expect another header.
    pub fn read_response(&mut self, max_len: usize) -> Result<Vec<u8>> {
        let tag = self.next_tag();
        let mut req = Vec::with_capacity(12);
        req.push(MSG_REQUEST_DEV_DEP_MSG_IN);
        req.push(tag);
        req.push(!tag);
        req.push(0);
        req.extend_from_slice(&(max_len as u32).to_le_bytes());
        req.extend_from_slice(&[0, 0, 0, 0]);
        let completion = pollster::block_on(self.interface.bulk_out(self.ep_out, req));
        completion
            .status
            .context("USB bulk OUT (request-in) failed")?;

        let mut out = Vec::new();
        let mut raw = Vec::new();
        // None => need to parse a fresh 12-byte header next.
        // Some((remaining_payload, padding_after, eom)) => mid-message.
        let mut state: Option<(usize, usize, bool)> = None;

        loop {
            if raw.is_empty() {
                // A large, slow-to-render response (e.g. a screen dump) can
                // legitimately answer an early poll with a zero-length
                // packet before real data is ready; that's not the same as
                // the device giving up, so just poll again rather than
                // failing immediately.
                let mut empty_reads = 0;
                loop {
                    let buf = nusb::transfer::RequestBuffer::new(1 << 20);
                    let completion = pollster::block_on(self.interface.bulk_in(self.ep_in, buf));
                    completion.status.context("USB bulk IN transfer failed")?;
                    if !completion.data.is_empty() {
                        raw = completion.data;
                        break;
                    }
                    empty_reads += 1;
                    if empty_reads > 100 {
                        bail!("USBTMC device kept returning empty reads (waited ~10s)");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            match state {
                None => {
                    if raw.len() < 12 {
                        // Header itself got split across packets; top up.
                        let buf = nusb::transfer::RequestBuffer::new(1 << 20);
                        let completion = pollster::block_on(self.interface.bulk_in(self.ep_in, buf));
                        completion.status.context("USB bulk IN transfer failed")?;
                        raw.extend_from_slice(&completion.data);
                        continue;
                    }
                    let header: Vec<u8> = raw.drain(..12).collect();
                    if header[0] != MSG_DEV_DEP_MSG_IN {
                        bail!("unexpected USBTMC MsgID {} in response header", header[0]);
                    }
                    let transfer_size =
                        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
                    let eom = header[8] & EOM != 0;
                    // Each DEV_DEP_MSG_IN transfer (header + payload) is
                    // padded to a multiple of 4 bytes.
                    let padding = (4 - (12 + transfer_size) % 4) % 4;
                    state = Some((transfer_size, padding, eom));
                }
                Some((remaining, padding, eom)) => {
                    let take = raw.len().min(remaining);
                    out.extend(raw.drain(..take));
                    let remaining = remaining - take;
                    if remaining > 0 {
                        state = Some((remaining, padding, eom));
                        continue;
                    }
                    // Payload for this message is fully in `out`; drop any
                    // trailing alignment padding before the next header.
                    let skip = padding.min(raw.len());
                    raw.drain(..skip);
                    let padding = padding - skip;
                    if padding > 0 {
                        state = Some((0, padding, eom));
                        continue;
                    }
                    if eom {
                        return Ok(out);
                    }
                    state = None;
                }
            }
        }
    }

    /// SCPI query: write + read the response as text.
    pub fn query(&mut self, command: &str) -> Result<String> {
        self.write(command)?;
        let data = self.read_response(4096)?;
        Ok(String::from_utf8_lossy(&data).trim_end().to_owned())
    }

    /// Parses `C1:PAVA? <param>` style replies (`C1:PAVA <PARAM>,<value><unit>`).
    fn query_pava(&mut self, channel: &str, param: &str) -> Result<f64> {
        let reply = self.query(&format!("{channel}:PAVA? {param}"))?;
        let value = reply
            .rsplit_once(',')
            .map(|(_, v)| v)
            .context("unexpected PAVA reply format")?;
        let numeric: String = value
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
            .collect();
        numeric
            .parse()
            .with_context(|| format!("parsing PAVA {param} value {value:?}"))
    }

    /// Captures the current screen as a PNG (converted from the scope's
    /// native BMP over SCDP).
    pub fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        self.write("SCDP")?;
        let bmp = self.read_response(4 * 1024 * 1024)?;
        let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp)
            .context("decoding scope screen dump as BMP")?;
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png)
            .context("encoding screenshot as PNG")?;
        Ok(png.into_inner())
    }

    /// Exports one channel's on-screen waveform as (sample_index, volts).
    ///
    /// The scope's raw samples are signed bytes with a device-internal
    /// codes-per-division scaling; rather than hardcode that constant, this
    /// calibrates the byte range against the channel's own TOP/BASE
    /// measurement (its reported max/min), which is exact and
    /// self-consistent regardless of firmware/model quirks.
    pub fn dump_waveform(&mut self, channel: &str) -> Result<Vec<(usize, f64)>> {
        self.write(&format!("{channel}:WF? DAT2"))?;
        let raw = self.read_response(2 * 1024 * 1024)?;
        let data = extract_block(&raw)?;

        let top = self.query_pava(channel, "TOP")?;
        let base = self.query_pava(channel, "BASE")?;
        let bytes: Vec<i8> = data.iter().map(|&b| b as i8).collect();
        let (&lo, &hi) = (
            bytes.iter().min().context("empty waveform")?,
            bytes.iter().max().context("empty waveform")?,
        );
        if hi == lo {
            bail!("waveform is flat (min == max); can't calibrate against TOP/BASE");
        }
        // hi/lo are i8; hi - lo can exceed i8::MAX (e.g. 100 - (-100) =
        // 200), so widen before subtracting.
        let scale = (top - base) / (hi as f64 - lo as f64);
        let offset = top - scale * hi as f64;

        Ok(bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| (i, scale * b as f64 + offset))
            .collect())
    }
}

/// USBTMC INITIATE_CLEAR + CHECK_CLEAR_STATUS, per USBTMC 1.0 sections
/// 4.2.1.4/4.2.1.5: aborts any in-progress bulk transfer and resets the
/// device's USBTMC state machine so the next command starts clean.
fn usbtmc_clear(interface: &nusb::Interface, interface_num: u16) -> Result<()> {
    use nusb::transfer::{ControlIn, ControlType, Recipient};

    let completion = pollster::block_on(interface.control_in(ControlIn {
        control_type: ControlType::Class,
        recipient: Recipient::Interface,
        request: 5, // INITIATE_CLEAR
        value: 0,
        index: interface_num,
        length: 1,
    }));
    completion.status.context("USBTMC INITIATE_CLEAR failed")?;

    for _ in 0..50 {
        let completion = pollster::block_on(interface.control_in(ControlIn {
            control_type: ControlType::Class,
            recipient: Recipient::Interface,
            request: 6, // CHECK_CLEAR_STATUS
            value: 0,
            index: interface_num,
            length: 2,
        }));
        completion.status.context("USBTMC CHECK_CLEAR_STATUS failed")?;
        // USBTMC_STATUS_SUCCESS = 0x01; anything else (notably PENDING =
        // 0x02) means keep polling.
        if completion.data.first() == Some(&0x01) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("USBTMC clear did not complete after polling CHECK_CLEAR_STATUS")
}

/// Strips the USBTMC echo prefix and `#<n><len>` IEEE-488.2 block header,
/// returning the raw data bytes.
fn extract_block(raw: &[u8]) -> Result<&[u8]> {
    let hash = raw
        .iter()
        .position(|&b| b == b'#')
        .context("no '#' block header found in waveform reply")?;
    let ndig = (raw[hash + 1] - b'0') as usize;
    let len_start = hash + 2;
    let len: usize = std::str::from_utf8(&raw[len_start..len_start + ndig])
        .ok()
        .and_then(|s| s.parse().ok())
        .context("malformed block length in waveform reply")?;
    let data_start = len_start + ndig;
    raw.get(data_start..data_start + len)
        .context("waveform reply shorter than its declared block length")
}
