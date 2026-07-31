//! Minimal raw-register driver for the MSPM0G3507 DAC12 (register layout from
//! the TI SDK header `hw_dac12.h`). 12-bit, VDDA/VSSA reference, output
//! buffer enabled onto the DAC_OUT pin (PA15). Ported verbatim from
//! ~/embed/Single-Phase-Power-Analyzer/src/dac.rs -- the mspm0-metapac
//! revision pinned here has no DAC12 register definitions at all
//! (`pub const DAC0: () = ();`), so this can't be done through the PAC.

use core::ptr::write_volatile;

const DAC0_BASE: u32 = 0x4001_8000;

const PWREN: u32 = 0x0800;
const CTL0: u32 = 0x1100;
const CTL1: u32 = 0x1110;
const DATA0: u32 = 0x1200;

const PWREN_KEY_ENABLE: u32 = 0x2600_0001;
const PWREN_KEY_DISABLE: u32 = 0x2600_0000;
const CTL0_ENABLE: u32 = 1 << 0;
const CTL0_RES_12BIT: u32 = 1 << 8;
const CTL1_AMPEN: u32 = 1 << 0;
const CTL1_REFSN_VSSA: u32 = 1 << 9;
const CTL1_OPS_OUT0: u32 = 1 << 24;

/// Power up and route the DAC to PA15 (DAC_OUT), 12-bit, VDDA reference.
pub fn init() {
    unsafe {
        write_volatile((DAC0_BASE + PWREN) as *mut u32, PWREN_KEY_ENABLE);
        cortex_m::asm::delay(16);
        write_volatile(
            (DAC0_BASE + CTL1) as *mut u32,
            CTL1_AMPEN | CTL1_REFSN_VSSA | CTL1_OPS_OUT0,
        );
        write_volatile((DAC0_BASE + CTL0) as *mut u32, CTL0_RES_12BIT | CTL0_ENABLE);
    }
}

/// Set the 12-bit output code (`0..=4095`), Vout = code / 4096 * VDDA.
pub fn set(code: u16) {
    unsafe {
        write_volatile((DAC0_BASE + DATA0) as *mut u32, code as u32 & 0xFFF);
    }
}

/// Release DAC_OUT and drop the module out of the power domain.
///
/// CTL1 is cleared before PWREN so the pin is let go deliberately rather than
/// as a side effect of the block losing power: OPS=0 opens both output
/// switches, and AMPEN=0 with AMPHIZ=0 leaves the disabled output buffer in
/// high impedance rather than pulled to ground (TRM 20.2.4).
pub fn power_down() {
    unsafe {
        write_volatile((DAC0_BASE + CTL0) as *mut u32, 0);
        write_volatile((DAC0_BASE + CTL1) as *mut u32, 0);
        write_volatile((DAC0_BASE + PWREN) as *mut u32, PWREN_KEY_DISABLE);
    }
}
