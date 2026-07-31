//! Minimal raw-register driver for the MSPM0G3507 VREF module, the 2.5 V
//! reference the ADC and the DAC both run on. Base address from the datasheet's
//! peripheral file map (SLASEX6C table 8-6); the mspm0-metapac revision pinned
//! here knows VREF only as a pair of pin signals, with no register block, so
//! this cannot be done through the PAC.
//!
//! Two hardware conditions, both from the datasheet, and neither of them
//! something firmware can check:
//!
//! - A 1 uF capacitor (0.7-1.15 uF) must sit between VREF+ (PA23) and
//!   VREF-/GND. Datasheet 7.15.2 note 5 is unusually blunt about it: "The VREF
//!   module should only be enabled when CVREF is connected and should not be
//!   enabled otherwise."
//! - VDD must be at least 2.7 V for the 2.5 V setting (7.15.1). On a harvested
//!   rail that is a real bound, not a formality: below it the reference is out
//!   of spec while `READY` still reads 1, so every reading scales wrong with no
//!   flag anywhere.

use core::ptr::read_volatile;
use core::ptr::write_volatile;

const VREF_BASE: u32 = 0x4003_0000;

const PWREN: u32 = 0x0800;
const RSTCTL: u32 = 0x0804;
const CLKDIV: u32 = 0x1000;
const CLKSEL: u32 = 0x1008;
const CTL0: u32 = 0x1100;
const CTL1: u32 = 0x1104;

const PWREN_KEY_ENABLE: u32 = 0x2600_0001;
const PWREN_KEY_DISABLE: u32 = 0x2600_0000;
/// RSTCTL takes key B1h, not the 26h PWREN takes; RESETASSERT and
/// RESETSTKYCLR are bits 0 and 1.
const RSTCTL_KEY_ASSERT: u32 = 0xB100_0003;

/// CLKSEL resets to zero, meaning *no* source is selected, and a VREF with no
/// clock never asserts READY however long it is given. Same three-way choice
/// the timers get; BUSCLK is what the rest of this firmware already runs on.
const CLKSEL_BUSCLK: u32 = 1 << 3;

/// Reference buffer 0, the one the ADC and DAC draw from.
const CTL0_ENABLE0: u32 = 1 << 0;

const CTL1_READY: u32 = 1 << 0;

/// Bounded like the OPA's enable wait, and for the same reason: never block the
/// CPU forever on analog state. The budget is far past the 200 us the datasheet
/// allows (7.15.2 Tstartup), so exhausting it means the reference is not coming
/// up at all -- a missing CVREF, or a supply below the 2.7 V the 2.5 V setting
/// needs.
const READY_TRIES: u32 = 100_000;

/// Power up the 2.5 V reference and wait for it to settle. Returns whether it
/// reported ready.
///
/// A `false` here invalidates every number the cycle would go on to produce,
/// which is why it is returned rather than swallowed: the ADC still converts
/// happily against an unsettled reference and the samples look entirely
/// ordinary.
pub fn init() -> bool {
    unsafe {
        // Reset before power, the order every other block in this firmware
        // brings up, so the module starts from a known state rather than from
        // whatever the last cycle left.
        write_volatile((VREF_BASE + RSTCTL) as *mut u32, RSTCTL_KEY_ASSERT);
        write_volatile((VREF_BASE + PWREN) as *mut u32, PWREN_KEY_ENABLE);
        cortex_m::asm::delay(16);

        // Clock first, then enable: READY is clocked logic, so a buffer enabled
        // without a source can sit there indefinitely with the reference
        // itself perfectly fine and nothing to say so.
        write_volatile((VREF_BASE + CLKDIV) as *mut u32, 0);
        write_volatile((VREF_BASE + CLKSEL) as *mut u32, CLKSEL_BUSCLK);

        // BUFCONFIG (bit 7) stays clear, which is the 2.5 V output; setting it
        // would select 1.4 V.
        write_volatile((VREF_BASE + CTL0) as *mut u32, CTL0_ENABLE0);

        for _ in 0..READY_TRIES {
            if read_volatile((VREF_BASE + CTL1) as *const u32) & CTL1_READY != 0 {
                return true;
            }
        }
        false
    }
}

/// Turn the reference off and drop the module out of the power domain. Costs
/// the 189 uA typ (330 uA max) it draws while enabled, which is the same order
/// as OPA1 and DAC12 and belongs in the same power-down.
pub fn power_down() {
    unsafe {
        write_volatile((VREF_BASE + CTL0) as *mut u32, 0);
        write_volatile((VREF_BASE + PWREN) as *mut u32, PWREN_KEY_DISABLE);
    }
}
