//! Minimal USBTMC client (class 0xFE, subclass 0x03) over `nusb`.
//!
//! There is no NI-VISA runtime on this bench, and USBTMC's bulk-transfer
//! framing is simple enough to speak directly. Both instruments this repo
//! talks to -- the SDS2102X Plus scope and the SDM3055X-E multimeter -- are
//! the same protocol behind different SCPI vocabularies, so the transport
//! lives here and the vocabularies live in `scope.rs` and `dmm.rs`.

use std::time::Duration;

use anyhow::{Context, Result, bail};

const USBTMC_CLASS: u8 = 0xFE;
const USBTMC_SUBCLASS: u8 = 0x03;

const MSG_DEV_DEP_MSG_OUT: u8 = 1;
const MSG_REQUEST_DEV_DEP_MSG_IN: u8 = 2;
const MSG_DEV_DEP_MSG_IN: u8 = 2;
const EOM: u8 = 0x01;

/// Siglent's vendor ID. Used only as a *fallback* candidate filter: some
/// platforms do not populate `DeviceInfo::interfaces()`, and without it the
/// class-based scan below finds nothing at all.
pub const SIGLENT_VID: u16 = 0xF4EC;

pub struct Usbtmc {
    interface: nusb::Interface,
    ep_out: u8,
    ep_in: u8,
    next_tag: u8,
}

impl Usbtmc {
    /// Devices worth trying to open, without opening any of them.
    ///
    /// A device qualifies by advertising a USBTMC interface in its enumeration
    /// data, or by being a Siglent. The second test exists because the first
    /// is not available everywhere, and opening every device on the bus to
    /// find out is not an acceptable way to answer the question.
    pub fn candidates() -> Result<Vec<nusb::DeviceInfo>> {
        Ok(nusb::list_devices()
            .context("listing USB devices")?
            .filter(|d| {
                d.vendor_id() == SIGLENT_VID
                    || d.interfaces()
                        .any(|i| i.class() == USBTMC_CLASS && i.subclass() == USBTMC_SUBCLASS)
            })
            .collect())
    }

    /// Opens a device's USBTMC interface as-is.
    pub fn open(info: &nusb::DeviceInfo) -> Result<Self> {
        Self::from_device(info.open().context("opening USB device")?)
    }

    /// Opens a device after a full USB reset.
    ///
    /// A previous run that died mid-transfer can leave an endpoint STALLed in
    /// a way `clear_halt` will not recover from. The reset makes the device
    /// drop off and re-enumerate, so the old handle is dead and the device has
    /// to be found again -- by VID/PID plus serial, since bus addresses move.
    pub fn open_reset(info: &nusb::DeviceInfo) -> Result<Self> {
        let (vid, pid) = (info.vendor_id(), info.product_id());
        let serial = info.serial_number().map(str::to_owned);

        let device = info.open().context("opening USB device")?;
        device.reset().context("resetting USB device")?;
        drop(device);
        std::thread::sleep(Duration::from_millis(500));

        let again = nusb::list_devices()
            .context("listing USB devices")?
            .find(|d| {
                d.vendor_id() == vid
                    && d.product_id() == pid
                    && (serial.is_none() || d.serial_number() == serial.as_deref())
            })
            .context("device did not come back after the USB reset")?;
        Self::from_device(again.open().context("re-opening USB device after reset")?)
    }

    fn from_device(device: nusb::Device) -> Result<Self> {
        let mut interface_num = None;
        let mut ep_out = None;
        let mut ep_in = None;
        for config in device.configurations() {
            for iface in config.interface_alt_settings() {
                if iface.class() != USBTMC_CLASS || iface.subclass() != USBTMC_SUBCLASS {
                    continue;
                }
                interface_num = Some(iface.interface_number());
                for ep in iface.endpoints() {
                    // The USBTMC interface also carries an interrupt-IN
                    // endpoint for status notifications; only the bulk pair
                    // moves DEV_DEP_MSG_* traffic.
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

        let interface_num = interface_num.context("device has no USBTMC interface")?;
        let interface = device
            .claim_interface(interface_num)
            .context("claiming USBTMC interface")?;
        let ep_in = ep_in.context("USBTMC interface has no bulk IN endpoint")?;
        let ep_out = ep_out.context("USBTMC interface has no bulk OUT endpoint")?;

        // A prior run that errored out mid-transfer leaves the device's USBTMC
        // state machine expecting the host to keep draining a response it
        // never finished sending, and without this the *next* command's reply
        // comes back stale. Best-effort: some firmware STALLs INITIATE_CLEAR
        // outright rather than implementing it.
        if let Err(e) = clear(&interface, interface_num as u16) {
            eprintln!("note: 设备不支持 USBTMC clear 握手（{e}），继续");
        }
        interface
            .clear_halt(ep_out)
            .context("clearing bulk OUT halt")?;
        interface
            .clear_halt(ep_in)
            .context("clearing bulk IN halt")?;

        Ok(Usbtmc {
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

    /// Sends a SCPI command with no response read back.
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
    /// Two-level protocol: one logical message (ending when its header's EOM
    /// bit is set) can still be split across many raw USB packets, and that
    /// splitting is *not* re-headered. So a short `bulk_in` read partway
    /// through a message's declared `transfer_size` means "keep reading raw
    /// bytes", not "message over"; only once `transfer_size` payload bytes
    /// have accumulated does EOM decide whether another header follows.
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

        // Sized off the caller's own bound: a 20-byte DMM reading has no use
        // for the megabyte a screen dump needs.
        let chunk = max_len.clamp(1024, 1 << 20);

        let mut out = Vec::new();
        let mut raw: Vec<u8> = Vec::new();
        // None => parse a fresh 12-byte header next.
        // Some((remaining_payload, padding_after, eom)) => mid-message.
        let mut state: Option<(usize, usize, bool)> = None;

        loop {
            if raw.is_empty() {
                // A large, slow-to-render response can legitimately answer an
                // early poll with a zero-length packet before real data is
                // ready; that is not the device giving up, so poll again.
                let mut empty_reads = 0;
                loop {
                    let buf = nusb::transfer::RequestBuffer::new(chunk);
                    let completion = pollster::block_on(self.interface.bulk_in(self.ep_in, buf));
                    completion.status.context("USB bulk IN transfer failed")?;
                    if !completion.data.is_empty() {
                        raw = completion.data;
                        break;
                    }
                    empty_reads += 1;
                    if empty_reads > 100 {
                        bail!("USBTMC 设备一直返回空读（等了约 10 s）");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }

            match state {
                None => {
                    if raw.len() < 12 {
                        // The header itself got split across packets; top up.
                        let buf = nusb::transfer::RequestBuffer::new(chunk);
                        let completion =
                            pollster::block_on(self.interface.bulk_in(self.ep_in, buf));
                        completion.status.context("USB bulk IN transfer failed")?;
                        raw.extend_from_slice(&completion.data);
                        continue;
                    }
                    let header: Vec<u8> = raw.drain(..12).collect();
                    if header[0] != MSG_DEV_DEP_MSG_IN {
                        bail!("响应头里的 USBTMC MsgID 不认识: {}", header[0]);
                    }
                    let transfer_size =
                        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
                    let eom = header[8] & EOM != 0;
                    // Each transfer (header + payload) is padded to a multiple
                    // of 4 bytes.
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
                    // Payload complete; drop trailing alignment padding before
                    // the next header.
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

    /// SCPI query: write, then read the reply as text.
    pub fn query(&mut self, command: &str) -> Result<String> {
        self.write(command)?;
        let data = self.read_response(4096)?;
        Ok(String::from_utf8_lossy(&data).trim_end().to_owned())
    }
}

/// USBTMC INITIATE_CLEAR + CHECK_CLEAR_STATUS (USBTMC 1.0 4.2.1.4/4.2.1.5):
/// aborts any in-progress bulk transfer and resets the device's state machine
/// so the next command starts clean.
fn clear(interface: &nusb::Interface, interface_num: u16) -> Result<()> {
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
        completion
            .status
            .context("USBTMC CHECK_CLEAR_STATUS failed")?;
        // USBTMC_STATUS_SUCCESS = 0x01; anything else (notably PENDING = 0x02)
        // means keep polling.
        if completion.data.first() == Some(&0x01) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("USBTMC clear 轮询 CHECK_CLEAR_STATUS 一直没完成")
}
