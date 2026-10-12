// SPDX-License-Identifier: MIT OR Apache-2.0

//! Consumer side of the dext's shared-memory RX rings (design §3.4, §5.2).
//!
//! The dext (producer) writes slots with plain stores and publishes them with
//! a release store of `ctl_head`/`audio_head`; it never overwrites a slot in
//! `[tail, head)` (drop-newest). The consumer acquire-loads the head, reads
//! the slots in place and release-stores its tail. Every shared field is
//! accessed as `&*(p as *const AtomicU64)` / `AtomicU32` on the mapping:
//! heads and live header fields with `Acquire`, tails with `Release`,
//! counters with `Relaxed`. Alignment follows from the §3.1 offsets (all
//! multiples of 8) and an 8-byte aligned base, checked in [`RxRings::from_raw`].

use std::fmt;
use std::marker::PhantomData;
use std::mem::{offset_of, size_of};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::abi::{self, ctr, Counters, RxClass, RxSlot, ShmConsumer, ShmHeader};

/// A mapped header or mapping size differs from the compiled constants
/// (daemon exit 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutMismatch {
    pub field: &'static str,
    pub expected: u64,
    pub found: u64,
}

impl fmt::Display for LayoutMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "shared-memory {} mismatch: expected {}, found {}",
            self.field, self.expected, self.found
        )
    }
}

impl std::error::Error for LayoutMismatch {}

fn check(field: &'static str, expected: u64, found: u64) -> Result<(), LayoutMismatch> {
    if expected == found {
        Ok(())
    } else {
        Err(LayoutMismatch {
            field,
            expected,
            found,
        })
    }
}

/// Every constant header field against the compiled ABI, and `generation`
/// against RingAttach's `scalarOut[0]` (design §5.2 step 5).
pub fn validate_header(hdr: &ShmHeader, generation: u64) -> Result<(), LayoutMismatch> {
    // The ring LAYOUT is identical across the ABI range we drive, so the
    // version is range-checked while every layout field below stays exact.
    // A v3 dext writes 3 here and its rings are byte-identical to a v4's.
    if !abi::abi_supported(hdr.abi_version) {
        return Err(LayoutMismatch {
            field: "abi_version",
            expected: u64::from(abi::ABI_VERSION),
            found: u64::from(hdr.abi_version),
        });
    }
    let fields: [(&'static str, u32, u32); 8] = [
        ("magic", abi::SHM_MAGIC, hdr.magic),
        ("header_size", abi::SHM_HEADER_SIZE, hdr.header_size),
        ("slot_size", abi::RX_SLOT_SIZE, hdr.slot_size),
        ("ctl_slots", abi::RX_CTL_SLOTS, hdr.ctl_slots),
        ("audio_slots", abi::RX_AUDIO_SLOTS, hdr.audio_slots),
        ("ctl_offset", abi::RX_CTL_OFFSET, hdr.ctl_offset),
        ("audio_offset", abi::RX_AUDIO_OFFSET, hdr.audio_offset),
        ("total_size", abi::RX_MEM_SIZE, hdr.total_size),
    ];
    for (field, expected, found) in fields {
        check(field, u64::from(expected), u64::from(found))?;
    }
    check("generation", generation, u64::from(hdr.generation))
}

/// The two RX rings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ring {
    /// gPTP, MSRP, MVRP, AVDECC, MAAP.
    Ctl,
    /// Bound AVTP streams.
    Audio,
}

impl Ring {
    pub const fn slots(self) -> u64 {
        match self {
            Ring::Ctl => abi::RX_CTL_SLOTS as u64,
            Ring::Audio => abi::RX_AUDIO_SLOTS as u64,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Ring::Ctl => "ctl",
            Ring::Audio => "audio",
        }
    }

    const fn base_offset(self) -> usize {
        match self {
            Ring::Ctl => abi::RX_CTL_OFFSET as usize,
            Ring::Audio => abi::RX_AUDIO_OFFSET as usize,
        }
    }

    const fn head_offset(self) -> usize {
        match self {
            Ring::Ctl => offset_of!(ShmHeader, ctl_head),
            Ring::Audio => offset_of!(ShmHeader, audio_head),
        }
    }

    const fn tail_offset(self) -> usize {
        match self {
            Ring::Ctl => offset_of!(ShmConsumer, ctl_tail),
            Ring::Audio => offset_of!(ShmConsumer, audio_tail),
        }
    }

    /// Byte offset of the slot holding record `index`.
    const fn slot_offset(self, index: u64) -> usize {
        self.base_offset() + ((index & (self.slots() - 1)) as usize) * abi::RX_SLOT_SIZE as usize
    }
}

/// Why a slot was rejected (counted by the consumer; the tail still advances).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotError {
    /// `len` > RX_SLOT_DATA_MAX.
    BadLen(u16),
    /// `cls` not a deliverable RxClass (1..=6).
    BadClass(u8),
    /// RXF_VLAN_FRAME and RXF_VLAN_DESC both set.
    BothVlanFlags,
    /// `clock_gen` ahead of the header's `clock_gen` (`slot_gen_invalid`).
    GenAhead { slot: u32, header: u32 },
}

impl SlotError {
    /// Status counter this rejection is reported under.
    pub const fn counter_name(self) -> &'static str {
        match self {
            SlotError::GenAhead { .. } => "slot_gen_invalid",
            _ => "slot_invalid",
        }
    }
}

impl fmt::Display for SlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlotError::BadLen(n) => write!(f, "slot len {n} > {}", abi::RX_SLOT_DATA_MAX),
            SlotError::BadClass(c) => write!(f, "slot cls {c} not deliverable"),
            SlotError::BothVlanFlags => f.write_str("slot has both VLAN flags"),
            SlotError::GenAhead { slot, header } => {
                write!(
                    f,
                    "slot clock_gen {slot} ahead of header clock_gen {header}"
                )
            }
        }
    }
}

/// A validated, read-only view of one slot.
#[derive(Clone, Copy, Debug)]
pub struct RxSlotRef<'a> {
    slot: &'a RxSlot,
    cls: RxClass,
}

impl<'a> RxSlotRef<'a> {
    /// Bounds and consistency checks: `len` <= 1984, `cls` deliverable, not
    /// both VLAN flags, `clock_gen` not ahead of `header_clock_gen` (wrapping
    /// u32 comparison).
    pub fn validate(slot: &'a RxSlot, header_clock_gen: u32) -> Result<Self, SlotError> {
        if u32::from(slot.len) > abi::RX_SLOT_DATA_MAX {
            return Err(SlotError::BadLen(slot.len));
        }
        let cls = match RxClass::from_u8(slot.cls) {
            Some(RxClass::None) | None => return Err(SlotError::BadClass(slot.cls)),
            Some(c) => c,
        };
        let both = abi::RXF_VLAN_FRAME | abi::RXF_VLAN_DESC;
        if slot.flags & both == both {
            return Err(SlotError::BothVlanFlags);
        }
        if (slot.clock_gen.wrapping_sub(header_clock_gen) as i32) > 0 {
            return Err(SlotError::GenAhead {
                slot: slot.clock_gen,
                header: header_clock_gen,
            });
        }
        Ok(Self { slot, cls })
    }

    pub fn raw(&self) -> &'a RxSlot {
        self.slot
    }

    pub fn index(&self) -> u64 {
        self.slot.index
    }

    pub fn cls(&self) -> RxClass {
        self.cls
    }

    /// The frame (no FCS, no TS header); `len` is bounded by `validate`.
    pub fn data(&self) -> &'a [u8] {
        let n = usize::from(self.slot.len).min(self.slot.data.len());
        &self.slot.data[..n]
    }

    /// SYSTIM ns of reception, if the NIC timestamped the frame.
    pub fn hw_ts(&self) -> Option<u64> {
        (self.slot.flags & abi::RXF_TS_HW != 0).then_some(self.slot.hw_ts_ns)
    }

    pub fn harvest_mach(&self) -> u64 {
        self.slot.harvest_mach
    }

    pub fn clock_gen(&self) -> u32 {
        self.slot.clock_gen
    }

    /// Harvested inside a SYSTIM step's settling window.
    pub fn step_ambig(&self) -> bool {
        self.slot.flags & abi::RXF_TS_STEP_AMBIG != 0
    }

    /// The tag the NIC stripped into the descriptor (data[] carries none).
    pub fn desc_vlan_tci(&self) -> Option<u16> {
        (self.slot.flags & abi::RXF_VLAN_DESC != 0).then_some(self.slot.vlan_tci)
    }

    /// data[12..14] == 0x8100.
    pub fn vlan_in_frame(&self) -> bool {
        self.slot.flags & abi::RXF_VLAN_FRAME != 0
    }

    pub fn flags(&self) -> u16 {
        self.slot.flags
    }

    pub fn status_error(&self) -> u32 {
        self.slot.status_error
    }

    pub fn desc_len(&self) -> u32 {
        self.slot.desc_len
    }
}

/// The consumer's tails in `mem::CONSUMER`, with a local copy (the daemon is
/// their only writer).
pub struct ConsumerTails<'m> {
    base: NonNull<u8>,
    ctl: u64,
    audio: u64,
    _m: PhantomData<&'m ()>,
}

impl ConsumerTails<'_> {
    fn atomic(&self, ring: Ring) -> &AtomicU64 {
        // SAFETY: base is 8-aligned and covers a ShmConsumer (from_raw).
        unsafe { &*(self.base.as_ptr().add(ring.tail_offset()) as *const AtomicU64) }
    }

    /// Records consumed so far.
    pub fn tail(&self, ring: Ring) -> u64 {
        match ring {
            Ring::Ctl => self.ctl,
            Ring::Audio => self.audio,
        }
    }

    /// Publish a new tail (release), freeing the slots before it.
    pub fn store(&mut self, ring: Ring, tail: u64) {
        self.atomic(ring).store(tail, Ordering::Release);
        match ring {
            Ring::Ctl => self.ctl = tail,
            Ring::Audio => self.audio = tail,
        }
    }
}

/// The record at the tail did not carry the tail's index, or the head/tail
/// distance was impossible; the tail jumped to the head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Desync {
    pub ring: Ring,
    pub tail: u64,
    pub head: u64,
    /// `slot.index` found at the tail (`None`: head − tail out of range).
    pub found_index: Option<u64>,
}

/// Result of one [`RxRings::drain`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Valid slots handed to the callback.
    pub delivered: u64,
    /// Slots rejected by [`RxSlotRef::validate`] (also handed to the callback).
    pub invalid: u64,
    pub desync: Option<Desync>,
}

/// The mapped RX memory plus the consumer tails.
pub struct RxRings<'m> {
    rx: NonNull<u8>,
    tails: ConsumerTails<'m>,
}

// SAFETY: RxRings only holds pointers into memory shared with the dext; all
// shared fields are accessed atomically or under the ring protocol, and
// draining needs `&mut self`, so moving it to the thread that consumes is fine.
unsafe impl Send for RxRings<'_> {}

impl<'m> RxRings<'m> {
    /// View a mapping. Checks the sizes and 8-byte alignment, not the header
    /// contents (call [`RxRings::validate`]).
    ///
    /// # Safety
    /// `rx` must be valid for reads of `rx_len` bytes and `consumer` for
    /// reads and writes of `consumer_len` bytes for `'m`, laid out per
    /// AVBShared.h, and every other writer must follow the ring protocol
    /// (atomic header fields, no writes to slots in `[tail, head)`, no writes
    /// to the consumer tails).
    pub unsafe fn from_raw(
        rx: *const u8,
        rx_len: usize,
        consumer: *mut u8,
        consumer_len: usize,
    ) -> Result<Self, LayoutMismatch> {
        let at_least = |field, min: u32, len: usize| {
            if (len as u64) < u64::from(min) {
                Err(LayoutMismatch {
                    field,
                    expected: u64::from(min),
                    found: len as u64,
                })
            } else {
                Ok(())
            }
        };
        at_least("rx_mem_size", abi::RX_MEM_SIZE, rx_len)?;
        at_least("consumer_mem_size", abi::CONSUMER_MEM_SIZE, consumer_len)?;
        check("rx_alignment", 0, (rx as usize % 8) as u64)?;
        check("consumer_alignment", 0, (consumer as usize % 8) as u64)?;
        let rx = NonNull::new(rx as *mut u8).ok_or(LayoutMismatch {
            field: "rx_address",
            expected: 1,
            found: 0,
        })?;
        let base = NonNull::new(consumer).ok_or(LayoutMismatch {
            field: "consumer_address",
            expected: 1,
            found: 0,
        })?;
        let mut tails = ConsumerTails {
            base,
            ctl: 0,
            audio: 0,
            _m: PhantomData,
        };
        tails.ctl = tails.atomic(Ring::Ctl).load(Ordering::Acquire);
        tails.audio = tails.atomic(Ring::Audio).load(Ordering::Acquire);
        Ok(Self { rx, tails })
    }

    fn u32_at(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off.is_multiple_of(4) && off + 4 <= size_of::<ShmHeader>());
        // SAFETY: inside the header, 4-aligned (offsets from offset_of!).
        unsafe { &*(self.rx.as_ptr().add(off) as *const AtomicU32) }
    }

    fn u64_at(&self, off: usize) -> &AtomicU64 {
        debug_assert!(off.is_multiple_of(8) && off + 8 <= size_of::<ShmHeader>());
        // SAFETY: inside the header, 8-aligned (offsets from offset_of!).
        unsafe { &*(self.rx.as_ptr().add(off) as *const AtomicU64) }
    }

    fn slot(&self, ring: Ring, index: u64) -> &RxSlot {
        // SAFETY: slot_offset + 2048 <= RX_MEM_SIZE <= rx_len, 8-aligned.
        unsafe { &*(self.rx.as_ptr().add(ring.slot_offset(index)) as *const RxSlot) }
    }

    /// Snapshot of the constant header fields and `generation` (acquire).
    fn layout(&self) -> ShmHeader {
        let ld = |off| self.u32_at(off).load(Ordering::Acquire);
        let mut h = ShmHeader::expected(0);
        h.magic = ld(offset_of!(ShmHeader, magic));
        h.abi_version = ld(offset_of!(ShmHeader, abi_version));
        h.header_size = ld(offset_of!(ShmHeader, header_size));
        h.slot_size = ld(offset_of!(ShmHeader, slot_size));
        h.ctl_slots = ld(offset_of!(ShmHeader, ctl_slots));
        h.audio_slots = ld(offset_of!(ShmHeader, audio_slots));
        h.ctl_offset = ld(offset_of!(ShmHeader, ctl_offset));
        h.audio_offset = ld(offset_of!(ShmHeader, audio_offset));
        h.total_size = ld(offset_of!(ShmHeader, total_size));
        h.generation = ld(offset_of!(ShmHeader, generation));
        h
    }

    /// [`validate_header`] on the mapped header.
    pub fn validate(&self, generation: u64) -> Result<(), LayoutMismatch> {
        validate_header(&self.layout(), generation)
    }

    pub fn generation(&self) -> u32 {
        self.u32_at(offset_of!(ShmHeader, generation))
            .load(Ordering::Acquire)
    }

    /// Live AvbStage.
    pub fn stage(&self) -> u32 {
        self.u32_at(offset_of!(ShmHeader, stage))
            .load(Ordering::Acquire)
    }

    /// Live AvbFault.
    pub fn fault(&self) -> u32 {
        self.u32_at(offset_of!(ShmHeader, fault))
            .load(Ordering::Acquire)
    }

    /// Live AVB_FLAG_*.
    pub fn flags(&self) -> u32 {
        self.u32_at(offset_of!(ShmHeader, flags))
            .load(Ordering::Acquire)
    }

    pub fn producer_running(&self) -> bool {
        self.u32_at(offset_of!(ShmHeader, producer_running))
            .load(Ordering::Acquire)
            != 0
    }

    pub fn last_tick_mach(&self) -> u64 {
        self.u64_at(offset_of!(ShmHeader, last_tick_mach))
            .load(Ordering::Acquire)
    }

    /// Dext clock generation (incremented by every SYSTIM write).
    pub fn clock_gen(&self) -> u32 {
        self.u32_at(offset_of!(ShmHeader, clock_gen))
            .load(Ordering::Acquire)
    }

    pub fn mac(&self) -> [u8; 6] {
        // SAFETY: written by the dext before RingAttach returned; constant.
        unsafe {
            std::ptr::read(self.rx.as_ptr().add(offset_of!(ShmHeader, mac)) as *const [u8; 6])
        }
    }

    /// Records the producer has written (acquire).
    pub fn head(&self, ring: Ring) -> u64 {
        self.u64_at(ring.head_offset()).load(Ordering::Acquire)
    }

    /// Records consumed (local copy of our own tail).
    pub fn tail(&self, ring: Ring) -> u64 {
        self.tails.tail(ring)
    }

    /// One dext counter (relaxed), 0 if out of range.
    pub fn counter(&self, index: usize) -> u64 {
        if index >= ctr::COUNT {
            return 0;
        }
        self.u64_at(offset_of!(ShmHeader, counters) + index * 8)
            .load(Ordering::Relaxed)
    }

    /// All dext counters (each relaxed).
    pub fn counters(&self) -> Counters {
        let mut c = Counters::default();
        for (i, v) in c.c.iter_mut().enumerate() {
            *v = self.counter(i);
        }
        c
    }

    /// Consume up to `max` records of `ring`, oldest first. Each record is
    /// checked with `slot.index == tail` (mismatch: report a [`Desync`], jump
    /// the tail to the head, stop) and then [`RxSlotRef::validate`]; valid and
    /// invalid slots both go to `f` and both advance the tail. The new tail
    /// is release-stored once at the end, after `f` has returned for every
    /// slot, so the producer cannot reuse a slot `f` is still reading.
    pub fn drain<F>(&mut self, ring: Ring, max: usize, mut f: F) -> DrainReport
    where
        F: FnMut(Result<RxSlotRef<'_>, SlotError>),
    {
        let mut report = DrainReport::default();
        let head = self.head(ring);
        // Read after the head: every slot below `head` was written with a
        // generation the dext had already published here.
        let header_gen = self.clock_gen();
        let start = self.tails.tail(ring);
        let avail = head.wrapping_sub(start);
        if avail > ring.slots() {
            report.desync = Some(Desync {
                ring,
                tail: start,
                head,
                found_index: None,
            });
            self.tails.store(ring, head);
            return report;
        }
        let mut tail = start;
        for _ in 0..avail.min(max as u64) {
            let slot = self.slot(ring, tail);
            if slot.index != tail {
                report.desync = Some(Desync {
                    ring,
                    tail,
                    head,
                    found_index: Some(slot.index),
                });
                tail = head;
                break;
            }
            match RxSlotRef::validate(slot, header_gen) {
                Ok(s) => {
                    report.delivered += 1;
                    f(Ok(s));
                }
                Err(e) => {
                    report.invalid += 1;
                    f(Err(e));
                }
            }
            tail = tail.wrapping_add(1);
        }
        if tail != start {
            self.tails.store(ring, tail);
        }
        report
    }
}

/// Heap-backed RX and consumer memory laid out like the dext's, with a
/// producer that follows the dext's rules. For tests and the daemon's fake
/// dext; never mapped.
pub struct HeapRxMem {
    rx: NonNull<u64>,
    rx_words: usize,
    consumer: NonNull<u64>,
    consumer_words: usize,
}

fn alloc_words(n: usize) -> NonNull<u64> {
    let b: Box<[u64]> = vec![0u64; n].into_boxed_slice();
    NonNull::new(Box::into_raw(b) as *mut u64).expect("Box::into_raw is non-null")
}

/// # Safety
/// `p` and `n` must come from [`alloc_words`].
unsafe fn free_words(p: NonNull<u64>, n: usize) {
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
        p.as_ptr(),
        n,
    )));
}

impl HeapRxMem {
    /// Zeroed memory with the header constants of attach `generation`.
    pub fn new(generation: u32) -> Self {
        let rx_words = abi::RX_MEM_SIZE as usize / 8;
        let consumer_words = abi::CONSUMER_MEM_SIZE as usize / 8;
        let m = Self {
            rx: alloc_words(rx_words),
            rx_words,
            consumer: alloc_words(consumer_words),
            consumer_words,
        };
        // SAFETY: the buffer is 8-aligned and larger than a header.
        unsafe {
            std::ptr::write(
                m.rx.as_ptr() as *mut ShmHeader,
                ShmHeader::expected(generation),
            );
        }
        m
    }

    /// The header, for setting live fields or corrupting constants in tests.
    /// Not while an [`RxRings`] from [`HeapRxMem::rings`] is alive.
    pub fn header_mut(&mut self) -> &mut ShmHeader {
        // SAFETY: 8-aligned, in bounds, uniquely borrowed through &mut self.
        unsafe { &mut *(self.rx.as_ptr() as *mut ShmHeader) }
    }

    /// A consumer view of this memory.
    pub fn rings(&self) -> RxRings<'_> {
        // SAFETY: both buffers live as long as &self, are 8-aligned and sized
        // per the ABI; HeapRxMem's producer methods follow the ring protocol.
        unsafe {
            RxRings::from_raw(
                self.rx.as_ptr() as *const u8,
                self.rx_words * 8,
                self.consumer.as_ptr() as *mut u8,
                self.consumer_words * 8,
            )
        }
        .expect("heap memory has the ABI layout")
    }

    fn rx_u8(&self) -> *mut u8 {
        self.rx.as_ptr() as *mut u8
    }

    fn head_atomic(&self, ring: Ring) -> &AtomicU64 {
        // SAFETY: in-bounds, 8-aligned header field.
        unsafe { &*(self.rx_u8().add(ring.head_offset()) as *const AtomicU64) }
    }

    /// The consumer's published tail (acquire), as the dext reads it.
    pub fn consumer_tail(&self, ring: Ring) -> u64 {
        // SAFETY: in-bounds, 8-aligned consumer field.
        unsafe {
            (*((self.consumer.as_ptr() as *mut u8).add(ring.tail_offset()) as *const AtomicU64))
                .load(Ordering::Acquire)
        }
    }

    pub fn head(&self, ring: Ring) -> u64 {
        self.head_atomic(ring).load(Ordering::Relaxed)
    }

    /// Overwrite a head (release), e.g. to simulate a corrupted index.
    pub fn set_head(&self, ring: Ring, head: u64) {
        self.head_atomic(ring).store(head, Ordering::Release);
    }

    /// Release-store the header clock generation.
    pub fn set_clock_gen(&self, gen: u32) {
        // SAFETY: in-bounds, 4-aligned header field.
        unsafe {
            (*(self.rx_u8().add(offset_of!(ShmHeader, clock_gen)) as *const AtomicU32))
                .store(gen, Ordering::Release);
        }
    }

    /// Overwrite the slot holding record `index` without publishing it.
    /// Callers must not target a slot a live [`RxSlotRef`] points at.
    pub fn write_slot(&self, ring: Ring, index: u64, slot: &RxSlot) {
        // SAFETY: in-bounds, 8-aligned slot; see the method contract.
        unsafe {
            std::ptr::write(
                self.rx_u8().add(ring.slot_offset(index)) as *mut RxSlot,
                *slot,
            );
        }
    }

    /// Produce one record like the dext: drop-newest if the ring is full
    /// (returns false), else write the slot at `head` with `index = head`
    /// and release-store `head + 1`.
    pub fn produce(&self, ring: Ring, slot: &RxSlot) -> bool {
        let head = self.head(ring);
        let tail = self.consumer_tail(ring);
        if head.wrapping_sub(tail) >= ring.slots() {
            return false;
        }
        let mut s = *slot;
        s.index = head;
        self.write_slot(ring, head, &s);
        self.set_head(ring, head.wrapping_add(1));
        true
    }
}

impl Drop for HeapRxMem {
    fn drop(&mut self) {
        // SAFETY: allocated in new() with these sizes.
        unsafe {
            free_words(self.rx, self.rx_words);
            free_words(self.consumer, self.consumer_words);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(cls: RxClass, payload: &[u8]) -> RxSlot {
        let mut s = RxSlot {
            len: payload.len() as u16,
            cls: cls as u8,
            ..RxSlot::default()
        };
        s.data[..payload.len()].copy_from_slice(payload);
        s
    }

    fn drain_all(r: &mut RxRings<'_>, ring: Ring) -> (Vec<u64>, DrainReport) {
        let mut idx = Vec::new();
        let rep = r.drain(ring, usize::MAX, |s| idx.push(s.unwrap().index()));
        (idx, rep)
    }

    #[test]
    fn header_accept_and_reject_per_field() {
        let mut m = HeapRxMem::new(7);
        assert_eq!(m.rings().validate(7), Ok(()));
        assert_eq!(
            m.rings().validate(8),
            Err(LayoutMismatch {
                field: "generation",
                expected: 8,
                found: 7
            })
        );
        type Corrupt = fn(&mut ShmHeader);
        let cases: [(&str, Corrupt); 9] = [
            ("magic", |h| h.magic ^= 1),
            ("abi_version", |h| h.abi_version = 2),
            ("header_size", |h| h.header_size = 4096),
            ("slot_size", |h| h.slot_size = 1024),
            ("ctl_slots", |h| h.ctl_slots = 256),
            ("audio_slots", |h| h.audio_slots = 4096),
            ("ctl_offset", |h| h.ctl_offset += 8),
            ("audio_offset", |h| h.audio_offset += 8),
            ("total_size", |h| h.total_size -= 1),
        ];
        for (field, corrupt) in cases {
            let saved = *m.header_mut();
            corrupt(m.header_mut());
            let err = m.rings().validate(7).unwrap_err();
            assert_eq!(err.field, field);
            // Same check on a plain value.
            assert_eq!(validate_header(m.header_mut(), 7).unwrap_err().field, field);
            *m.header_mut() = saved;
            assert_eq!(m.rings().validate(7), Ok(()));
        }
        assert_eq!(validate_header(&ShmHeader::expected(3), 3), Ok(()));
    }

    #[test]
    fn from_raw_checks_sizes_and_alignment() {
        let rx = vec![0u64; abi::RX_MEM_SIZE as usize / 8];
        let mut cons = vec![0u64; abi::CONSUMER_MEM_SIZE as usize / 8];
        let rxp = rx.as_ptr() as *const u8;
        let cp = cons.as_mut_ptr() as *mut u8;
        unsafe {
            let e = RxRings::from_raw(rxp, abi::RX_MEM_SIZE as usize - 8, cp, 16384)
                .err()
                .unwrap();
            assert_eq!(e.field, "rx_mem_size");
            let e = RxRings::from_raw(rxp, abi::RX_MEM_SIZE as usize, cp, 256)
                .err()
                .unwrap();
            assert_eq!(e.field, "consumer_mem_size");
            let e = RxRings::from_raw(rxp.add(4), abi::RX_MEM_SIZE as usize - 8, cp, 16384);
            assert_eq!(e.err().unwrap().field, "rx_mem_size");
            let e = RxRings::from_raw(rxp.add(4), abi::RX_MEM_SIZE as usize, cp, 16384);
            assert_eq!(e.err().unwrap().field, "rx_alignment");
            let e = RxRings::from_raw(rxp, abi::RX_MEM_SIZE as usize, cp.add(4), 16380);
            assert_eq!(e.err().unwrap().field, "consumer_mem_size");
            let r = RxRings::from_raw(rxp, abi::RX_MEM_SIZE as usize, cp, 16384).unwrap();
            // Zeroed memory fails on the ABI version, which is checked BEFORE
            // any layout field: a header whose ABI we do not drive tells us
            // nothing by its magic or its offsets, and reporting a layout
            // mismatch for it would send the reader after the wrong bug.
            assert_eq!(r.validate(0).unwrap_err().field, "abi_version");
        }
    }

    /// The layout fields are still checked, and `magic` is still the first of
    /// them — a header that clears the version gate but is not ours must be
    /// rejected as foreign memory rather than parsed.
    #[test]
    fn a_supported_version_with_a_foreign_header_fails_on_magic() {
        let mut m = HeapRxMem::new(1);
        for v in abi::ABI_VERSION_MIN..=abi::ABI_VERSION {
            let h = m.header_mut();
            *h = unsafe { core::mem::zeroed() };
            h.abi_version = v;
            assert_eq!(
                validate_header(m.header_mut(), 0).unwrap_err().field,
                "magic",
                "ABI v{v} is driven, so its layout must be checked"
            );
        }
    }

    #[test]
    fn live_fields_and_counters() {
        let mut m = HeapRxMem::new(1);
        {
            let h = m.header_mut();
            h.stage = 2;
            h.fault = 0;
            h.flags = crate::abi::FLAG_LINK_UP | crate::abi::FLAG_RINGS_ATTACHED;
            h.producer_running = 1;
            h.last_tick_mach = 123_456;
            h.mac = [0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8];
            h.counters.c[ctr::RX_FRAMES] = 42;
            h.counters.c[ctr::RX_OVERSIZE] = 3;
            h.clock_gen = 5;
        }
        let r = m.rings();
        assert_eq!(r.generation(), 1);
        assert_eq!(r.stage(), 2);
        assert_eq!(r.fault(), 0);
        assert_eq!(
            r.flags(),
            crate::abi::FLAG_LINK_UP | crate::abi::FLAG_RINGS_ATTACHED
        );
        assert!(r.producer_running());
        assert_eq!(r.last_tick_mach(), 123_456);
        assert_eq!(r.mac(), [0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
        assert_eq!(r.clock_gen(), 5);
        assert_eq!(r.counter(ctr::RX_FRAMES), 42);
        assert_eq!(r.counter(ctr::COUNT), 0);
        let c = r.counters();
        assert_eq!(c.get(ctr::RX_OVERSIZE), 3);
        assert_eq!(c.get(ctr::RX_ERRORS), 0);
    }

    #[test]
    fn delivers_in_order_and_publishes_tail() {
        let m = HeapRxMem::new(1);
        for i in 0..3u8 {
            assert!(m.produce(Ring::Ctl, &slot(RxClass::Gptp, &[i; 60])));
        }
        assert!(m.produce(Ring::Audio, &slot(RxClass::Stream, &[9; 100])));
        let mut r = m.rings();
        let mut seen = Vec::new();
        let rep = r.drain(Ring::Ctl, usize::MAX, |s| {
            let s = s.unwrap();
            assert_eq!(s.cls(), RxClass::Gptp);
            seen.push((s.index(), s.data().to_vec()));
        });
        assert_eq!(
            rep,
            DrainReport {
                delivered: 3,
                invalid: 0,
                desync: None
            }
        );
        assert_eq!(seen.len(), 3);
        for (i, (idx, data)) in seen.iter().enumerate() {
            assert_eq!(*idx, i as u64);
            assert_eq!(data, &vec![i as u8; 60]);
        }
        assert_eq!(r.tail(Ring::Ctl), 3);
        assert_eq!(m.consumer_tail(Ring::Ctl), 3);
        // The audio ring is independent.
        assert_eq!(r.tail(Ring::Audio), 0);
        let (idx, _) = drain_all(&mut r, Ring::Audio);
        assert_eq!(idx, [0]);
        assert_eq!(m.consumer_tail(Ring::Audio), 1);
        // Nothing new: no tail store, empty report.
        assert_eq!(drain_all(&mut r, Ring::Ctl).1, DrainReport::default());
    }

    #[test]
    fn drain_respects_max() {
        let m = HeapRxMem::new(1);
        for _ in 0..20 {
            assert!(m.produce(Ring::Ctl, &slot(RxClass::Avdecc, &[0; 64])));
        }
        let mut r = m.rings();
        let mut n = 0;
        let rep = r.drain(Ring::Ctl, 8, |_| n += 1);
        assert_eq!((n, rep.delivered, r.tail(Ring::Ctl)), (8, 8, 8));
        let (idx, _) = drain_all(&mut r, Ring::Ctl);
        assert_eq!(idx, (8..20).collect::<Vec<u64>>());
    }

    #[test]
    fn wraps_at_ring_size_with_drop_newest() {
        let m = HeapRxMem::new(1);
        let slots = Ring::Ctl.slots();
        for _ in 0..slots {
            assert!(m.produce(Ring::Ctl, &slot(RxClass::Msrp, &[1; 30])));
        }
        // Full: the producer drops the new record.
        assert!(!m.produce(Ring::Ctl, &slot(RxClass::Msrp, &[1; 30])));
        let mut r = m.rings();
        let mut got = Vec::new();
        r.drain(Ring::Ctl, 300, |s| got.push(s.unwrap().index()));
        // Producer refills the freed slots; positions wrap past the end.
        for _ in 0..300 {
            assert!(m.produce(Ring::Ctl, &slot(RxClass::Msrp, &[2; 30])));
        }
        assert!(!m.produce(Ring::Ctl, &slot(RxClass::Msrp, &[2; 30])));
        let rep = r.drain(Ring::Ctl, usize::MAX, |s| got.push(s.unwrap().index()));
        assert_eq!(rep.desync, None);
        assert_eq!(got, (0..slots + 300).collect::<Vec<u64>>());
        assert_eq!(r.tail(Ring::Ctl), slots + 300);
        assert_eq!(Ring::Ctl.slot_offset(slots), Ring::Ctl.slot_offset(0));
        assert_eq!(
            Ring::Audio.slot_offset(Ring::Audio.slots() - 1) + abi::RX_SLOT_SIZE as usize,
            abi::RX_MEM_SIZE as usize
        );
    }

    #[test]
    fn index_mismatch_reports_desync_and_jumps_to_head() {
        let m = HeapRxMem::new(1);
        for _ in 0..5 {
            assert!(m.produce(Ring::Ctl, &slot(RxClass::Gptp, &[0; 44])));
        }
        let mut bad = slot(RxClass::Gptp, &[0; 44]);
        bad.index = 1000;
        m.write_slot(Ring::Ctl, 2, &bad);
        let mut r = m.rings();
        let (idx, rep) = drain_all(&mut r, Ring::Ctl);
        assert_eq!(idx, [0, 1]);
        assert_eq!(
            rep.desync,
            Some(Desync {
                ring: Ring::Ctl,
                tail: 2,
                head: 5,
                found_index: Some(1000)
            })
        );
        assert_eq!(r.tail(Ring::Ctl), 5);
        assert_eq!(m.consumer_tail(Ring::Ctl), 5);
        // Normal delivery resumes from the head.
        assert!(m.produce(Ring::Ctl, &slot(RxClass::Gptp, &[0; 44])));
        assert_eq!(drain_all(&mut r, Ring::Ctl).0, [5]);
    }

    #[test]
    fn impossible_head_tail_distance_is_desync() {
        let m = HeapRxMem::new(1);
        m.set_head(Ring::Audio, Ring::Audio.slots() + 1);
        let mut r = m.rings();
        let rep = r.drain(Ring::Audio, usize::MAX, |_| panic!("no slot expected"));
        assert_eq!(
            rep.desync,
            Some(Desync {
                ring: Ring::Audio,
                tail: 0,
                head: Ring::Audio.slots() + 1,
                found_index: None
            })
        );
        assert_eq!(r.tail(Ring::Audio), Ring::Audio.slots() + 1);
        // Head behind tail (wrapping distance huge) is also a desync.
        m.set_head(Ring::Audio, 3);
        let rep = r.drain(Ring::Audio, usize::MAX, |_| panic!("no slot expected"));
        assert_eq!(rep.desync.unwrap().found_index, None);
        assert_eq!(r.tail(Ring::Audio), 3);
    }

    #[test]
    fn slot_validation_rejects() {
        let base = slot(RxClass::Stream, &[0; 10]);
        assert!(RxSlotRef::validate(&base, 0).is_ok());
        let max = RxSlot {
            len: abi::RX_SLOT_DATA_MAX as u16,
            ..base
        };
        assert_eq!(RxSlotRef::validate(&max, 0).unwrap().data().len(), 1984);
        let long = RxSlot { len: 1985, ..base };
        assert_eq!(
            RxSlotRef::validate(&long, 0).unwrap_err(),
            SlotError::BadLen(1985)
        );
        for cls in [0u8, 7, 0x80, 0xFF] {
            let s = RxSlot { cls, ..base };
            assert_eq!(
                RxSlotRef::validate(&s, 0).unwrap_err(),
                SlotError::BadClass(cls)
            );
        }
        let both = RxSlot {
            flags: abi::RXF_VLAN_FRAME | abi::RXF_VLAN_DESC,
            ..base
        };
        assert_eq!(
            RxSlotRef::validate(&both, 0).unwrap_err(),
            SlotError::BothVlanFlags
        );
        // clock_gen: equal or behind is fine, ahead is slot_gen_invalid
        // (wrapping comparison).
        let g = |clock_gen| RxSlot { clock_gen, ..base };
        assert!(RxSlotRef::validate(&g(2), 2).is_ok());
        assert!(RxSlotRef::validate(&g(1), 2).is_ok());
        let e = RxSlotRef::validate(&g(3), 2).unwrap_err();
        assert_eq!(e, SlotError::GenAhead { slot: 3, header: 2 });
        assert_eq!(e.counter_name(), "slot_gen_invalid");
        assert!(RxSlotRef::validate(&g(u32::MAX), 0).is_ok());
        assert!(RxSlotRef::validate(&g(0), u32::MAX).is_err());
        assert_eq!(SlotError::BothVlanFlags.counter_name(), "slot_invalid");
    }

    #[test]
    fn slot_accessors() {
        let mut s = slot(RxClass::Stream, &[1, 2, 3]);
        s.flags = abi::RXF_TS_HW | abi::RXF_VLAN_DESC | abi::RXF_TS_STEP_AMBIG;
        s.hw_ts_ns = 77;
        s.vlan_tci = 0x6002;
        s.harvest_mach = 99;
        s.clock_gen = 4;
        s.status_error = 0x8000_0003;
        s.desc_len = 19;
        let v = RxSlotRef::validate(&s, 4).unwrap();
        assert_eq!(v.data(), &[1, 2, 3]);
        assert_eq!(v.hw_ts(), Some(77));
        assert_eq!(v.desc_vlan_tci(), Some(0x6002));
        assert!(!v.vlan_in_frame());
        assert!(v.step_ambig());
        assert_eq!(
            (
                v.harvest_mach(),
                v.clock_gen(),
                v.status_error(),
                v.desc_len()
            ),
            (99, 4, 0x8000_0003, 19)
        );
        s.flags = abi::RXF_VLAN_FRAME;
        let v = RxSlotRef::validate(&s, 4).unwrap();
        assert_eq!((v.hw_ts(), v.desc_vlan_tci()), (None, None));
        assert!(v.vlan_in_frame() && !v.step_ambig());
    }

    #[test]
    fn invalid_slots_are_reported_and_skipped() {
        let m = HeapRxMem::new(1);
        m.set_clock_gen(1);
        assert!(m.produce(Ring::Ctl, &slot(RxClass::Gptp, &[0; 44])));
        let mut ahead = slot(RxClass::Gptp, &[0; 44]);
        ahead.clock_gen = 2;
        assert!(m.produce(Ring::Ctl, &ahead));
        assert!(m.produce(Ring::Ctl, &slot(RxClass::Gptp, &[0; 44])));
        let mut r = m.rings();
        let mut errs = Vec::new();
        let rep = r.drain(Ring::Ctl, usize::MAX, |s| {
            if let Err(e) = s {
                errs.push(e);
            }
        });
        assert_eq!((rep.delivered, rep.invalid), (2, 1));
        assert_eq!(errs, [SlotError::GenAhead { slot: 2, header: 1 }]);
        assert_eq!(r.tail(Ring::Ctl), 3);
    }
}
