// SPDX-License-Identifier: MIT OR Apache-2.0

//! GET_COUNTERS blocks (1722.1-2021 §7.4.42, Milan v1.3 §5.4.2.25).
//!
//! # The block ordering, and how we know it
//!
//! The response payload is `descriptor_type(2) descriptor_index(2)
//! counters_valid(4)` then 32 big-endian `u32` counters. `counters_valid` is a
//! bitmask saying which of the 32 slots mean anything.
//!
//! Milan's tables give each counter TWO numbers: a "Bit #" counted MSB-first
//! (so bit 31 is the least significant bit) and a "Bit value". The slot a
//! counter occupies in the block is **`log2(bit_value)`**, equivalently
//! `31 - bit#`.
//!
//! That is not read off the table — a bit-packed layout taken from a spec
//! table is exactly the mistake this project has been bitten by before. It is
//! anchored on the 16R's OWN GET_COUNTERS response, which the daemon dumped
//! slot by slot. Three slots were non-zero and all three agree:
//!
//! | observed slot | value seen | Milan symbol       | Bit # | bit value |
//! |---------------|------------|--------------------|-------|-----------|
//! | 3             | 9          | SEQ_NUM_MISMATCH   | 28    | `0x0008`  |
//! | 9             | 103013     | LATE_TIMESTAMP     | 22    | `0x0200`  |
//! | 11            | +8000/s    | FRAMES_RX          | 20    | `0x0800`  |
//!
//! `log2(0x8) = 3`, `log2(0x200) = 9`, `log2(0x800) = 11`. A shipping Milan
//! device confirms the convention, so [`slot_index`] is derived from the bit
//! value rather than tabulated per counter.

/// Mandatory (Table 5.10) and optional (Table 5.11) AVB_INTERFACE counters.
pub mod avb_interface {
    pub const LINK_UP: u32 = 0x0000_0001;
    pub const LINK_DOWN: u32 = 0x0000_0002;
    /// Optional (Table 5.11).
    pub const FRAMES_TX: u32 = 0x0000_0004;
    /// Optional (Table 5.11).
    pub const FRAMES_RX: u32 = 0x0000_0008;
    /// Optional (Table 5.11).
    pub const RX_CRC_ERROR: u32 = 0x0000_0010;
    pub const GPTP_GM_CHANGED: u32 = 0x0000_0020;
}

/// CLOCK_DOMAIN counters (Table 5.12). Both mandatory.
pub mod clock_domain {
    pub const LOCKED: u32 = 0x0000_0001;
    pub const UNLOCKED: u32 = 0x0000_0002;
}

/// STREAM_INPUT counters (Table 5.13). All mandatory.
pub mod stream_input {
    pub const MEDIA_LOCKED: u32 = 0x0000_0001;
    pub const MEDIA_UNLOCKED: u32 = 0x0000_0002;
    pub const STREAM_INTERRUPTED: u32 = 0x0000_0004;
    pub const SEQ_NUM_MISMATCH: u32 = 0x0000_0008;
    pub const MEDIA_RESET: u32 = 0x0000_0010;
    pub const TIMESTAMP_UNCERTAIN: u32 = 0x0000_0020;
    pub const UNSUPPORTED_FORMAT: u32 = 0x0000_0100;
    pub const LATE_TIMESTAMP: u32 = 0x0000_0200;
    pub const EARLY_TIMESTAMP: u32 = 0x0000_0400;
    pub const FRAMES_RX: u32 = 0x0000_0800;
}

/// STREAM_OUTPUT counters (Table 5.14).
///
/// Milan v1.3 notes that earlier versions numbered these differently from
/// ATDECC and that a controller is expected to read `specification_version`
/// from GET_MILAN_INFO to interpret them. We report `1.3.0.0`, so these are
/// the v1.3 positions.
pub mod stream_output {
    pub const STREAM_START: u32 = 0x0000_0001;
    pub const STREAM_STOP: u32 = 0x0000_0002;
    pub const MEDIA_RESET: u32 = 0x0000_0008;
    pub const TIMESTAMP_UNCERTAIN: u32 = 0x0000_0010;
    pub const FRAMES_TX: u32 = 0x0000_0080;
}

/// Which of the 32 slots a counter bit occupies: `log2(bit_value)`.
///
/// `None` for a value that is not a single bit, which would otherwise alias
/// onto some unrelated slot.
pub const fn slot_index(bit_value: u32) -> Option<usize> {
    if bit_value == 0 || !bit_value.is_power_of_two() {
        return None;
    }
    Some(bit_value.trailing_zeros() as usize)
}

/// One descriptor's counter block, plus the mask of which slots are real.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CounterBlock {
    valid: u32,
    slots: [u32; 32],
}

impl CounterBlock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a counter and mark it valid.
    ///
    /// A counter we DO track and whose event has simply never happened is
    /// correctly reported as valid and zero — that is a real reading. It is
    /// only a counter we do not maintain at all that must stay out of
    /// `counters_valid`, so a controller can tell "nothing happened" from
    /// "this device does not know".
    pub fn set(&mut self, bit_value: u32, value: u32) -> &mut Self {
        if let Some(i) = slot_index(bit_value) {
            if let Some(slot) = self.slots.get_mut(i) {
                *slot = value;
                self.valid |= bit_value;
            }
        }
        self
    }

    /// Same, from a wider source counter. Saturates: the wire field is 32
    /// bits, and wrapping a 5-billion-frame tally to a small number would
    /// read as a counter that went BACKWARDS, which a controller may treat as
    /// a device restart.
    pub fn set_u64(&mut self, bit_value: u32, value: u64) -> &mut Self {
        self.set(bit_value, u32::try_from(value).unwrap_or(u32::MAX))
    }

    /// Record a counter only if it is tracked. `None` leaves the slot zero and
    /// its valid bit CLEAR, which is the difference between "never happened"
    /// and "not measured".
    pub fn set_opt(&mut self, bit_value: u32, value: Option<u32>) -> &mut Self {
        match value {
            Some(v) => self.set(bit_value, v),
            None => self,
        }
    }

    pub fn counters_valid(&self) -> u32 {
        self.valid
    }

    pub fn slot(&self, bit_value: u32) -> Option<u32> {
        slot_index(bit_value).and_then(|i| self.slots.get(i).copied())
    }

    /// The GET_COUNTERS response payload for this block.
    pub fn encode_payload(&self, descriptor_type: u16, descriptor_index: u16) -> Vec<u8> {
        let mut p = Vec::with_capacity(4 + 4 + 128);
        p.extend_from_slice(&descriptor_type.to_be_bytes());
        p.extend_from_slice(&descriptor_index.to_be_bytes());
        p.extend_from_slice(&self.valid.to_be_bytes());
        for s in &self.slots {
            p.extend_from_slice(&s.to_be_bytes());
        }
        p
    }
}

/// Per-STREAM_INPUT counters, as plain numbers the daemon can fill in.
///
/// Every field is an `Option` on purpose. `counters_valid` exists to separate
/// "this event has not happened" from "this device does not track it", and a
/// plain `u32` cannot express the second — a forgotten field would default to
/// 0 and be published as a measured zero. That is not a smaller lie than a
/// wrong number: reporting MEDIA_LOCKED and SEQ_NUM_MISMATCH as a valid 0 is
/// a positive claim that the input stream has never glitched, which we would
/// be making without having looked. `None` keeps the counter out of
/// `counters_valid` entirely.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamInputCounters {
    pub media_locked: Option<u32>,
    pub media_unlocked: Option<u32>,
    pub stream_interrupted: Option<u32>,
    pub seq_num_mismatch: Option<u32>,
    pub media_reset: Option<u32>,
    pub timestamp_uncertain: Option<u32>,
    pub unsupported_format: Option<u32>,
    pub late_timestamp: Option<u32>,
    pub early_timestamp: Option<u32>,
    pub frames_rx: Option<u32>,
}

impl StreamInputCounters {
    pub fn block(&self) -> CounterBlock {
        let mut b = CounterBlock::new();
        b.set_opt(stream_input::MEDIA_LOCKED, self.media_locked)
            .set_opt(stream_input::MEDIA_UNLOCKED, self.media_unlocked)
            .set_opt(stream_input::STREAM_INTERRUPTED, self.stream_interrupted)
            .set_opt(stream_input::SEQ_NUM_MISMATCH, self.seq_num_mismatch)
            .set_opt(stream_input::MEDIA_RESET, self.media_reset)
            .set_opt(stream_input::TIMESTAMP_UNCERTAIN, self.timestamp_uncertain)
            .set_opt(stream_input::UNSUPPORTED_FORMAT, self.unsupported_format)
            .set_opt(stream_input::LATE_TIMESTAMP, self.late_timestamp)
            .set_opt(stream_input::EARLY_TIMESTAMP, self.early_timestamp)
            .set_opt(stream_input::FRAMES_RX, self.frames_rx);
        b
    }
}

/// Per-STREAM_OUTPUT counters. `None` means not tracked — see
/// [`StreamInputCounters`] for why that distinction is kept in the type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamOutputCounters {
    pub stream_start: Option<u32>,
    pub stream_stop: Option<u32>,
    pub media_reset: Option<u32>,
    pub timestamp_uncertain: Option<u32>,
    pub frames_tx: Option<u32>,
}

impl StreamOutputCounters {
    pub fn block(&self) -> CounterBlock {
        let mut b = CounterBlock::new();
        b.set_opt(stream_output::STREAM_START, self.stream_start)
            .set_opt(stream_output::STREAM_STOP, self.stream_stop)
            .set_opt(stream_output::MEDIA_RESET, self.media_reset)
            .set_opt(stream_output::TIMESTAMP_UNCERTAIN, self.timestamp_uncertain)
            .set_opt(stream_output::FRAMES_TX, self.frames_tx);
        b
    }
}

/// Everything GET_COUNTERS can be asked for, as the daemon supplies it.
///
/// Plain numbers only: the bit positions live in this module so the daemon
/// never encodes a layout, and a counter it cannot supply is simply left at
/// its default rather than being invented.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityCounters {
    // AVB_INTERFACE. `None` means not tracked, so it is omitted from
    // counters_valid rather than published as a measured zero.
    pub link_up: Option<u32>,
    pub link_down: Option<u32>,
    pub gptp_gm_changed: Option<u32>,
    pub frames_tx: Option<u32>,
    pub frames_rx: Option<u32>,
    pub rx_crc_error: Option<u32>,
    // CLOCK_DOMAIN.
    pub clock_locked: Option<u32>,
    pub clock_unlocked: Option<u32>,
    /// Per STREAM_INPUT descriptor index.
    pub inputs: Vec<StreamInputCounters>,
    /// Per STREAM_OUTPUT descriptor index.
    pub outputs: Vec<StreamOutputCounters>,
}

impl EntityCounters {
    pub fn avb_interface_block(&self) -> CounterBlock {
        let mut b = CounterBlock::new();
        // Table 5.10 (mandatory) and 5.11 (optional). Each is claimed only if
        // the caller actually supplied it, so a failed dext read publishes no
        // counters rather than a link that looks idle.
        b.set_opt(avb_interface::LINK_UP, self.link_up)
            .set_opt(avb_interface::LINK_DOWN, self.link_down)
            .set_opt(avb_interface::GPTP_GM_CHANGED, self.gptp_gm_changed)
            .set_opt(avb_interface::FRAMES_TX, self.frames_tx)
            .set_opt(avb_interface::FRAMES_RX, self.frames_rx)
            .set_opt(avb_interface::RX_CRC_ERROR, self.rx_crc_error);
        b
    }

    pub fn clock_domain_block(&self) -> CounterBlock {
        let mut b = CounterBlock::new();
        b.set_opt(clock_domain::LOCKED, self.clock_locked)
            .set_opt(clock_domain::UNLOCKED, self.clock_unlocked);
        b
    }

    pub fn stream_input_block(&self, index: u16) -> Option<CounterBlock> {
        self.inputs.get(usize::from(index)).map(|c| c.block())
    }

    pub fn stream_output_block(&self, index: u16) -> Option<CounterBlock> {
        self.outputs.get(usize::from(index)).map(|c| c.block())
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// The three slots the 16R reported non-zero, which is where the whole
    /// ordering comes from. If this breaks, the layout is wrong.
    #[test]
    fn slot_indices_match_the_16r_capture() {
        assert_eq!(slot_index(stream_input::SEQ_NUM_MISMATCH), Some(3));
        assert_eq!(slot_index(stream_input::LATE_TIMESTAMP), Some(9));
        assert_eq!(slot_index(stream_input::FRAMES_RX), Some(11));
    }

    /// `log2(bit value)` must equal `31 - bit#` for every counter Milan
    /// tabulates, which is the invariant the capture only sampled.
    #[test]
    fn every_counter_slot_is_31_minus_its_milan_bit_number() {
        let table: &[(u32, usize, &str)] = &[
            (avb_interface::LINK_UP, 31, "LINK_UP"),
            (avb_interface::LINK_DOWN, 30, "LINK_DOWN"),
            (avb_interface::FRAMES_TX, 29, "FRAMES_TX"),
            (avb_interface::FRAMES_RX, 28, "FRAMES_RX"),
            (avb_interface::RX_CRC_ERROR, 27, "RX_CRC_ERROR"),
            (avb_interface::GPTP_GM_CHANGED, 26, "GPTP_GM_CHANGED"),
            (clock_domain::LOCKED, 31, "CD LOCKED"),
            (clock_domain::UNLOCKED, 30, "CD UNLOCKED"),
            (stream_input::MEDIA_LOCKED, 31, "MEDIA_LOCKED"),
            (stream_input::MEDIA_UNLOCKED, 30, "MEDIA_UNLOCKED"),
            (stream_input::STREAM_INTERRUPTED, 29, "STREAM_INTERRUPTED"),
            (stream_input::SEQ_NUM_MISMATCH, 28, "SEQ_NUM_MISMATCH"),
            (stream_input::MEDIA_RESET, 27, "SI MEDIA_RESET"),
            (stream_input::TIMESTAMP_UNCERTAIN, 26, "SI TS_UNCERTAIN"),
            (stream_input::UNSUPPORTED_FORMAT, 23, "UNSUPPORTED_FORMAT"),
            (stream_input::LATE_TIMESTAMP, 22, "LATE_TIMESTAMP"),
            (stream_input::EARLY_TIMESTAMP, 21, "EARLY_TIMESTAMP"),
            (stream_input::FRAMES_RX, 20, "SI FRAMES_RX"),
            (stream_output::STREAM_START, 31, "STREAM_START"),
            (stream_output::STREAM_STOP, 30, "STREAM_STOP"),
            (stream_output::MEDIA_RESET, 28, "SO MEDIA_RESET"),
            (stream_output::TIMESTAMP_UNCERTAIN, 27, "SO TS_UNCERTAIN"),
            (stream_output::FRAMES_TX, 24, "SO FRAMES_TX"),
        ];
        for &(value, bit_no, name) in table {
            assert_eq!(
                slot_index(value),
                Some(31 - bit_no),
                "{name}: bit# {bit_no} and value {value:#010x} disagree"
            );
        }
    }

    #[test]
    fn a_non_single_bit_has_no_slot() {
        assert_eq!(slot_index(0), None);
        assert_eq!(slot_index(0b11), None);
    }

    /// A counter we track but which never fired reads valid-and-zero; one we
    /// do NOT track is absent from counters_valid entirely. That is the whole
    /// point of the mask, and the reason `Some(0)` and `None` must not be the
    /// same thing in the type.
    #[test]
    fn tracked_zero_is_valid_but_untracked_is_not_claimed() {
        let tracked = EntityCounters {
            link_up: Some(0),
            ..Default::default()
        };
        let b = tracked.avb_interface_block();
        assert_eq!(b.slot(avb_interface::LINK_UP), Some(0));
        assert_ne!(b.counters_valid() & avb_interface::LINK_UP, 0);
        // Everything else was left None and must NOT be claimed.
        assert_eq!(b.counters_valid() & avb_interface::FRAMES_RX, 0);
        assert_eq!(b.counters_valid() & avb_interface::LINK_DOWN, 0);
    }

    /// The default is "I measure nothing", NOT "nothing has happened". A
    /// default-constructed block that claimed every counter as a valid zero
    /// would assert a clean bill of health nobody measured.
    #[test]
    fn a_default_block_claims_no_counters_at_all() {
        let c = EntityCounters::default();
        assert_eq!(c.avb_interface_block().counters_valid(), 0);
        assert_eq!(c.clock_domain_block().counters_valid(), 0);
        assert_eq!(
            StreamInputCounters::default().block().counters_valid(),
            0,
            "MEDIA_LOCKED/SEQ_NUM_MISMATCH as a valid 0 would claim the \
             stream has never glitched"
        );
        assert_eq!(StreamOutputCounters::default().block().counters_valid(), 0);
    }

    #[test]
    fn interface_block_claims_only_what_was_supplied() {
        let c = EntityCounters {
            frames_rx: Some(7),
            ..Default::default()
        };
        let b = c.avb_interface_block();
        assert_ne!(b.counters_valid() & avb_interface::FRAMES_RX, 0);
        assert_eq!(b.slot(avb_interface::FRAMES_RX), Some(7));
        assert_eq!(b.counters_valid() & avb_interface::FRAMES_TX, 0);
    }

    /// The payload must be exactly 136 bytes with the value at the slot the
    /// capture pinned, not merely self-consistent through our own reader.
    #[test]
    fn payload_places_the_value_at_the_captured_byte_offset() {
        let c = StreamInputCounters {
            frames_rx: Some(0x0102_0304),
            ..Default::default()
        };
        let p = c.block().encode_payload(0x0005, 3);
        assert_eq!(p.len(), 4 + 4 + 128);
        assert_eq!(&p[0..2], &0x0005u16.to_be_bytes(), "descriptor_type");
        assert_eq!(&p[2..4], &3u16.to_be_bytes(), "descriptor_index");
        // FRAMES_RX is slot 11, so bytes 8 + 11*4 .. +4.
        let at = 8 + 11 * 4;
        assert_eq!(&p[at..at + 4], &0x0102_0304u32.to_be_bytes());
        // counters_valid carries the bit VALUE, not the slot.
        let valid = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        assert_ne!(valid & stream_input::FRAMES_RX, 0);
    }

    /// 32 bits on the wire: a wider tally must clamp, never wrap. A counter
    /// that appears to go backwards reads as a device restart.
    #[test]
    fn a_wide_counter_saturates_rather_than_wrapping() {
        let mut b = CounterBlock::new();
        b.set_u64(avb_interface::FRAMES_RX, u64::from(u32::MAX) + 5);
        assert_eq!(b.slot(avb_interface::FRAMES_RX), Some(u32::MAX));
    }

    #[test]
    fn per_index_blocks_are_bounded_by_what_exists() {
        let c = EntityCounters {
            inputs: vec![StreamInputCounters::default()],
            outputs: vec![StreamOutputCounters::default(); 2],
            ..Default::default()
        };
        assert!(c.stream_input_block(0).is_some());
        assert!(c.stream_input_block(1).is_none());
        assert!(c.stream_output_block(1).is_some());
        assert!(c.stream_output_block(2).is_none());
    }
}
