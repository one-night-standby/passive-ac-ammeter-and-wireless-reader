//! The field calibration table in flash: the last sector of MAIN, holding the
//! table `cal.rs` looks up once the bench app has pushed one.
//!
//! It has to be flash rather than RAM because this meter loses power as a
//! matter of course -- it is powered by the load current it measures, so every
//! break in the loop, every discharge before a start-up test, and every dip
//! while the load is being adjusted takes the whole rail down. A table that
//! lived in RAM would have to be re-pushed after each of those, in front of the
//! examiner, and the one time it was not would be a reading silently taken on
//! the built-in table.
//!
//! Two properties matter more than density here, and both come from the same
//! fact: the supply can vanish mid-write.
//!
//! - **Records are appended, not overwritten.** A record is 144 bytes and the
//!   sector holds seven, so six pushes out of seven do not erase anything. The
//!   previously good table stays readable the whole time the new one is being
//!   written, and the erase -- the one step that destroys a good table before
//!   writing a new one -- happens on one push in seven instead of every push.
//! - **The magic word is programmed last.** Everything else in the record is in
//!   flash and verified before the eight bytes that make the slot count as a
//!   record exist at all. A push cut short by a brownout leaves a slot that
//!   `load` skips, not a half table it believes.
//!
//! Anything that gets past both of those still has to match its CRC. What
//! `load` cannot make sense of is not an error state: it means the meter comes
//! up on `CAL_X1`, reads a little worse, and says `SRC=ROM` in every frame it
//! sends -- which is the app's cue to push again.
//!
//! Reads go through the uncorrected alias at 0x0040_0000 (TRM table 6-3). On
//! the corrected path a word whose ECC does not match its data is a dual-bit
//! error, and SYSCTL turns that into an NMI or a reset -- which is precisely
//! what a slot abandoned mid-program can hold. The raw alias returns the bits
//! and lets the CRC be the judge. (An *erased* word is safe on either path:
//! ECC checks are skipped for all-ones, TRM 6.5.2.)

use core::cell::Cell;

use critical_section::Mutex;
use embassy_mspm0::pac;
use pac::flashctl::vals::{Command, Size};

use crate::cal::{FIELD_MAX, Table};

/// Last 1 KB sector of the 128 KB MAIN region. `memory.x` stops the linker
/// 1 KB short of the bank, so this address is the one place in flash that no
/// build can put code or constants into.
const SECTOR: u32 = 0x0001_FC00;

/// The same sector through the uncorrected read alias. See the module note.
const SECTOR_RAW: u32 = 0x0040_0000 + SECTOR;

const SECTOR_LEN: usize = 1024;

/// One flash word: 64 data bits plus 8 of ECC, programmed as a single command.
/// Erased-then-programmed once is the only sequence this module uses; a word
/// is never programmed twice without an erase in between, which is what ECC
/// requires.
const WORD: usize = 8;

/// CMDWEPROTB bit 11 covers MAIN sectors 120-127. On a single-bank device
/// CMDWEPROTA protects the first 32 sectors one bit each and CMDWEPROTB the
/// rest in groups of eight, starting at sector 32 (TRM 6.4.1, and the
/// CMDWEPROTB description). Erase resolution stays one sector, so unprotecting
/// the group does not widen what an erase touches.
const WEPROT_B_BIT: u32 = 11;

/// Marks a slot as holding a record. Also the format version: change it and
/// every older record on the part stops being read, which is the correct
/// behaviour for a layout change -- an old record read as a new one is a
/// plausible wrong table.
const MAGIC: u32 = 0x4341_4C31;

/// Words per record: magic+seq, len+crc, then one word per point.
const RECORD_WORDS: usize = 2 + FIELD_MAX;

const RECORD_BYTES: usize = RECORD_WORDS * WORD;

/// Seven records per sector, 16 bytes left over.
const SLOTS: usize = SECTOR_LEN / RECORD_BYTES;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// The flash controller reported the command failed. Nothing distinguishes
    /// the causes worth acting on differently: every one of them means the
    /// table in flash is not the table that was pushed.
    Flash,
}

/// Read the newest valid record. `None` means there is no table in flash --
/// the sector has never been written, or nothing in it survives its CRC.
pub fn load() -> Option<Table> {
    let mut best: Option<(u32, Table)> = None;
    for slot in 0..SLOTS {
        let Some((seq, table)) = read_slot(slot) else {
            continue;
        };
        // Sequence numbers only ever increase, so the highest is the newest,
        // and comparing them rather than trusting slot order is what makes a
        // record written into slot 0 after an erase win against the six older
        // ones that the erase did not reach.
        if best.is_none_or(|(best_seq, _)| seq > best_seq) {
            best = Some((seq, table));
        }
    }
    best.map(|(_, table)| table)
}

/// Append `table` as the newest record, erasing the sector first if there is
/// no blank slot left.
///
/// The table is validated by the caller -- this module stores what it is given
/// and only guarantees that what `load` returns afterwards is what was stored.
pub fn store(table: &Table) -> Result<(), StoreError> {
    let (mut next, newest_seq) = match critical_section::with(|cs| LAST.borrow(cs).get()) {
        Some((slot, seq)) => (slot + 1, seq),
        None => {
            let mut newest_seq = 0u32;
            let mut next = 0usize;
            for slot in 0..SLOTS {
                if let Some((seq, _)) = read_slot(slot) {
                    if seq >= newest_seq {
                        newest_seq = seq;
                        next = slot + 1;
                    }
                }
            }
            (next, newest_seq)
        }
    };

    // Erase when the append point has run past the end of the sector, and also
    // when the slot it lands on is not blank -- a slot can hold the debris of a
    // push that lost power before its magic word, and programming a word twice
    // without an erase is what ECC does not allow.
    if next >= SLOTS || !slot_is_blank(next) {
        erase_sector()?;
        next = 0;
    }

    let base = SECTOR + (next * RECORD_BYTES) as u32;
    let mut words = [[0u32; 2]; RECORD_WORDS];
    words[1] = [table.len as u32, crc(table, newest_seq + 1)];
    for (i, (lsb, amps)) in table.points.iter().enumerate() {
        words[2 + i] = [lsb.to_bits(), amps.to_bits()];
    }
    for (i, word) in words.iter().enumerate().skip(1) {
        program_word(base + (i * WORD) as u32, word[0], word[1])?;
    }

    // Last, and only now: the eight bytes that make everything above count as
    // a record.
    program_word(base, MAGIC, newest_seq + 1)?;
    critical_section::with(|cs| LAST.borrow(cs).set(Some((next, newest_seq + 1))));
    Ok(())
}

/// Where the store before this one landed, and what sequence number it used.
///
/// Not an optimisation. After a program the CPU's prefetch and cache can still
/// hold the pre-write contents of the addresses just written (TRM 6.3.3.5 step
/// 9), so a second push in one power cycle cannot find the first one's record
/// by re-reading the sector -- it would see a blank slot where a record is and
/// program a word twice, which ECC does not allow and post-verification
/// rejects. Boot has no such problem, nothing having been written yet, so the
/// scan stands as the answer until the first store replaces it.
///
/// A store that failed leaves this alone: the next one re-scans, which is the
/// weaker answer but the only honest one after a write of unknown extent.
static LAST: Mutex<Cell<Option<(usize, u32)>>> = Mutex::new(Cell::new(None));

/// `(sequence, table)` if the slot holds a record whose CRC matches.
fn read_slot(slot: usize) -> Option<(u32, Table)> {
    let base = SECTOR_RAW + (slot * RECORD_BYTES) as u32;
    if read_u32(base) != MAGIC {
        return None;
    }
    let seq = read_u32(base + 4);
    let len = read_u32(base + WORD as u32) as usize;
    let stored_crc = read_u32(base + WORD as u32 + 4);
    // `len == 0` is how the app takes a field table away again: an appended
    // record that says "no table", rather than an erase. Clearing is then the
    // same power-safe operation as pushing, and one that cannot leave the
    // meter with neither the old table nor the new one.
    if len == 1 || len > FIELD_MAX {
        return None;
    }

    let mut table = Table::EMPTY;
    table.len = len;
    for (i, point) in table.points.iter_mut().enumerate() {
        let at = base + ((2 + i) * WORD) as u32;
        *point = (f32::from_bits(read_u32(at)), f32::from_bits(read_u32(at + 4)));
    }
    if crc(&table, seq) != stored_crc {
        return None;
    }
    Some((seq, table))
}

/// Blank means every word still reads as erased. Not a `BLANKVERIFY` -- this
/// only decides whether to erase before appending, and the flash controller's
/// own post-verification is what actually catches a slot that was not blank.
fn slot_is_blank(slot: usize) -> bool {
    let base = SECTOR_RAW + (slot * RECORD_BYTES) as u32;
    (0..RECORD_BYTES as u32)
        .step_by(4)
        .all(|offset| read_u32(base + offset) == u32::MAX)
}

fn read_u32(addr: u32) -> u32 {
    // SAFETY: `addr` is inside the reserved sector's raw alias, which is
    // mapped, aligned and read-only from the CPU's point of view.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// CRC-32 over the whole record except the magic word, sequence included.
///
/// Bit-serial rather than table-driven: 144 bytes once per push and once per
/// slot at boot is not worth 1 KB of lookup table in a 128 KB part that is
/// otherwise spending its flash on the calibration this protects.
fn crc(table: &Table, seq: u32) -> u32 {
    let mut acc = 0xFFFF_FFFFu32;
    let mut feed = |value: u32| {
        for byte in value.to_le_bytes() {
            acc ^= byte as u32;
            for _ in 0..8 {
                acc = if acc & 1 != 0 {
                    (acc >> 1) ^ 0xEDB8_8320
                } else {
                    acc >> 1
                };
            }
        }
    };
    feed(seq);
    feed(table.len as u32);
    // Every slot, not just the filled ones: the unused tail is programmed too,
    // so covering it is what makes the CRC describe the record as written.
    for (lsb, amps) in table.points.iter() {
        feed(lsb.to_bits());
        feed(amps.to_bits());
    }
    !acc
}

/// Unprotect the sector's group. Every command re-protects everything on
/// completion (TRM 6.3.3.5 step 8, 6.3.4.2 step 6), so this runs before each
/// one rather than once per store.
fn unprotect() {
    let flash = pac::FLASHCTL;
    flash
        .cmdweprota()
        .write_value(pac::flashctl::regs::Cmdweprot(u32::MAX));
    flash
        .cmdweprotb()
        .write_value(pac::flashctl::regs::Cmdweprot(!(1 << WEPROT_B_BIT)));
}

/// Spin until the controller reports the command finished, and say whether it
/// passed. There is no timeout: `CMDDONE` is raised by the same hardware that
/// stalls this CPU's fetches for the duration, so a command that never
/// finished is not a state this loop could report from.
fn wait_done() -> Result<(), StoreError> {
    let flash = pac::FLASHCTL;
    loop {
        let status = flash.statcmd().read();
        if status.done() {
            return if status.pass() {
                Ok(())
            } else {
                Err(StoreError::Flash)
            };
        }
    }
}

fn program_word(addr: u32, lo: u32, hi: u32) -> Result<(), StoreError> {
    let flash = pac::FLASHCTL;
    flash.cmdtype().write(|w| {
        w.set_command(Command::PROGRAM);
        w.set_size(Size::ONEWORD);
    });
    flash.cmdctl().modify(|w| {
        // Both verifications on, explicitly. They are the reason a `PASS` from
        // the controller is worth more than a read-back would be: the hardware
        // checked the cells against the data it just wrote, at margin, before
        // raising it. Their reset state is enabled, but this register is not
        // ours alone and reset state is not the same as known state.
        w.set_preveren(true);
        w.set_postveren(true);
        // Hardware generates the ECC byte from the data, and the address is
        // translated from the system address in CMDADDR. Both are the defaults;
        // both are wrong if left set by something else.
        w.set_eccgenovr(false);
        w.set_addrxlateovr(false);
        w.set_progmaskdis(false);
    });
    flash.cmdaddr().write_value(addr);
    flash.cmdbyten().write(|w| {
        for byte in 0..8 {
            w.set_data(byte, true);
        }
        w.set_ecc(true);
    });
    flash.cmddata(0).write_value(lo);
    flash.cmddata(1).write_value(hi);
    unprotect();
    flash.cmdexec().write(|w| w.set_val(true));
    wait_done()
}

fn erase_sector() -> Result<(), StoreError> {
    let flash = pac::FLASHCTL;
    flash.cmdtype().write(|w| {
        w.set_command(Command::ERASE);
        w.set_size(Size::SECTOR);
    });
    flash.cmdctl().modify(|w| {
        w.set_preveren(true);
        w.set_postveren(true);
        w.set_addrxlateovr(false);
        w.set_erasemaskdis(false);
    });
    flash.cmdaddr().write_value(SECTOR);
    unprotect();
    flash.cmdexec().write(|w| w.set_val(true));
    wait_done()
}
