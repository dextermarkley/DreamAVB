// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fixed-capacity transmit queue (design §5.2): each protocol crate's `poll`
//! writes into its own class's `Outbox<N>`; a push into a full outbox drops
//! the **new** frame and returns [`TxFull`]. No allocation after construction.

use std::fmt;

use crate::eth::EthHeader;

/// Largest frame the dext accepts in `SendFrame` (AVB_FRAME_MAX_LEN, no FCS).
pub const TX_FRAME_MAX: usize = 1514;

/// The outbox was full; the new frame was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxFull;

/// The frame exceeds [`TX_FRAME_MAX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameTooLong;

/// One frame to send.
#[derive(Clone, Copy)]
pub struct TxFrame {
    len: u16,
    /// Request a TX hardware timestamp (gPTP event messages).
    pub want_ts: bool,
    /// Opaque owner cookie (e.g. a gPTP sequence ID) returned with the
    /// timestamp result.
    pub tag: u64,
    buf: [u8; TX_FRAME_MAX],
}

impl TxFrame {
    pub const EMPTY: TxFrame = TxFrame {
        len: 0,
        want_ts: false,
        tag: 0,
        buf: [0; TX_FRAME_MAX],
    };

    pub fn from_bytes(bytes: &[u8], want_ts: bool, tag: u64) -> Result<Self, FrameTooLong> {
        let mut f = Self::EMPTY;
        f.buf
            .get_mut(..bytes.len())
            .ok_or(FrameTooLong)?
            .copy_from_slice(bytes);
        f.len = bytes.len() as u16;
        f.want_ts = want_ts;
        f.tag = tag;
        Ok(f)
    }

    /// `hdr` followed by `payload`.
    pub fn build(
        hdr: &EthHeader,
        payload: &[u8],
        want_ts: bool,
        tag: u64,
    ) -> Result<Self, FrameTooLong> {
        let mut f = Self::EMPTY;
        let n = hdr.encode_frame(payload, &mut f.buf).ok_or(FrameTooLong)?;
        f.len = n as u16;
        f.want_ts = want_ts;
        f.tag = tag;
        Ok(f)
    }

    pub fn bytes(&self) -> &[u8] {
        self.buf.get(..usize::from(self.len)).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for TxFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxFrame")
            .field("len", &self.len)
            .field("want_ts", &self.want_ts)
            .field("tag", &self.tag)
            .finish_non_exhaustive()
    }
}

/// FIFO of at most `N` frames.
pub struct Outbox<const N: usize> {
    frames: [TxFrame; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Default for Outbox<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Outbox<N> {
    pub const fn new() -> Self {
        Self {
            frames: [TxFrame::EMPTY; N],
            head: 0,
            len: 0,
        }
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn is_full(&self) -> bool {
        self.len >= N
    }

    /// Append; on a full outbox the new frame is dropped (`Err(TxFull)`).
    pub fn push(&mut self, frame: TxFrame) -> Result<(), TxFull> {
        if self.is_full() {
            return Err(TxFull);
        }
        let idx = (self.head + self.len) % N;
        *self.frames.get_mut(idx).ok_or(TxFull)? = frame;
        self.len += 1;
        Ok(())
    }

    /// The oldest frame, left in place (a frame the dext refused with
    /// Busy/NoResources stays at the head).
    pub fn front(&self) -> Option<&TxFrame> {
        if self.is_empty() {
            return None;
        }
        self.frames.get(self.head)
    }

    /// Remove and return the oldest frame.
    pub fn pop(&mut self) -> Option<TxFrame> {
        if self.is_empty() {
            return None;
        }
        let f = *self.frames.get(self.head)?;
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(f)
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Frames oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &TxFrame> + '_ {
        (0..self.len).filter_map(move |i| self.frames.get((self.head + i) % N))
    }
}

impl<const N: usize> fmt::Debug for Outbox<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outbox")
            .field("capacity", &N)
            .field("len", &self.len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac::MacAddr;

    fn frame(tag: u64) -> TxFrame {
        TxFrame::from_bytes(&[tag as u8; 20], false, tag).unwrap()
    }

    #[test]
    fn fifo_and_drop_new() {
        let mut o = Outbox::<3>::new();
        assert!(o.is_empty() && o.front().is_none() && o.pop().is_none());
        for t in 1..=3 {
            o.push(frame(t)).unwrap();
        }
        assert!(o.is_full());
        assert_eq!(o.push(frame(4)), Err(TxFull));
        assert_eq!(o.len(), 3);
        assert_eq!(o.iter().map(|f| f.tag).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(o.front().unwrap().tag, 1);
        assert_eq!(o.pop().unwrap().tag, 1);
        o.push(frame(5)).unwrap(); // wraps
        assert_eq!(o.iter().map(|f| f.tag).collect::<Vec<_>>(), [2, 3, 5]);
        assert_eq!(o.pop().unwrap().tag, 2);
        assert_eq!(o.pop().unwrap().tag, 3);
        assert_eq!(o.pop().unwrap().tag, 5);
        assert!(o.pop().is_none());
        o.push(frame(6)).unwrap();
        o.clear();
        assert!(o.is_empty());
    }

    #[test]
    fn zero_capacity_is_always_full() {
        let mut o = Outbox::<0>::new();
        assert_eq!(o.push(frame(1)), Err(TxFull));
        assert!(o.pop().is_none());
    }

    #[test]
    fn frames() {
        let f = TxFrame::from_bytes(&[7; TX_FRAME_MAX], true, 9).unwrap();
        assert_eq!(f.len(), TX_FRAME_MAX);
        assert!(f.want_ts);
        assert_eq!(
            TxFrame::from_bytes(&[0; TX_FRAME_MAX + 1], false, 0).unwrap_err(),
            FrameTooLong
        );
        let hdr = EthHeader {
            dst: MacAddr([0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E]),
            src: MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]),
            vlan: None,
            ethertype: 0x88F7,
        };
        let f = TxFrame::build(&hdr, &[1, 2, 3], true, 0x4000).unwrap();
        assert_eq!(
            f.bytes(),
            [
                0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E, 0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8, 0x88, 0xF7,
                1, 2, 3
            ]
        );
        assert_eq!(
            TxFrame::build(&hdr, &[0; TX_FRAME_MAX - 13], false, 0).unwrap_err(),
            FrameTooLong
        );
        assert!(TxFrame::EMPTY.is_empty());
    }
}
