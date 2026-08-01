//! The bench oscilloscope (Siglent SDS2102X Plus): a screen dump and a raw
//! waveform export. Transport is `usbtmc`; what is here is only the SCPI this
//! particular instrument speaks.

use anyhow::{Context, Result, bail};

use crate::usbtmc::{SIGLENT_VID, Usbtmc};

const SDS2000XPLUS_PID: u16 = 0x1011;

pub struct Scope {
    tmc: Usbtmc,
}

impl Scope {
    /// Opens the first SDS2000X Plus on the bus.
    pub fn open() -> Result<Self> {
        let info = nusb::list_devices()
            .context("listing USB devices")?
            .find(|d| d.vendor_id() == SIGLENT_VID && d.product_id() == SDS2000XPLUS_PID)
            .context("USB 上没有 Siglent SDS2000X Plus —— 接上了吗，开机了吗？")?;
        // Reset first: a run that died mid-screen-dump leaves the endpoint
        // STALLed, and screen dumps are the transfers most likely to be
        // interrupted.
        Ok(Scope {
            tmc: Usbtmc::open_reset(&info)?,
        })
    }

    /// SCPI query: write + read the reply as text.
    pub fn query(&mut self, command: &str) -> Result<String> {
        self.tmc.query(command)
    }

    /// Parses `C1:PAVA? <param>` style replies (`C1:PAVA <PARAM>,<value><unit>`).
    fn query_pava(&mut self, channel: &str, param: &str) -> Result<f64> {
        let reply = self.tmc.query(&format!("{channel}:PAVA? {param}"))?;
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

    /// Captures the current screen as a PNG (converted from the scope's native
    /// BMP over SCDP).
    pub fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        self.tmc.write("SCDP")?;
        let bmp = self.tmc.read_response(4 * 1024 * 1024)?;
        let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp)
            .context("decoding scope screen dump as BMP")?;
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png)
            .context("encoding screenshot as PNG")?;
        Ok(png.into_inner())
    }

    /// Exports one channel's on-screen waveform as (sample_index, volts).
    ///
    /// The raw samples are signed bytes with a device-internal codes-per-
    /// division scaling; rather than hardcode that constant, this calibrates
    /// the byte range against the channel's own TOP/BASE measurement, which is
    /// exact and self-consistent regardless of firmware or model.
    pub fn dump_waveform(&mut self, channel: &str) -> Result<Vec<(usize, f64)>> {
        self.tmc.write(&format!("{channel}:WF? DAT2"))?;
        let raw = self.tmc.read_response(2 * 1024 * 1024)?;
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
        // hi/lo are i8 and their difference can exceed i8::MAX, so widen first.
        let scale = (top - base) / (hi as f64 - lo as f64);
        let offset = top - scale * hi as f64;

        Ok(bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| (i, scale * b as f64 + offset))
            .collect())
    }
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
