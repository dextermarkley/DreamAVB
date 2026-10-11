// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounds-checked big-endian cursors. The protocol crates deny
//! `clippy::indexing_slicing`, so every field access goes through these.

/// Reader over a byte slice; every getter returns `None` past the end.
pub(crate) struct Rd<'a> {
    b: &'a [u8],
}

impl<'a> Rd<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Self { b }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.b.len()
    }

    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (h, t) = self.b.split_at_checked(n)?;
        self.b = t;
        Some(h)
    }

    pub(crate) fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(u8::from_be_bytes(self.arr()?))
    }

    pub(crate) fn i8(&mut self) -> Option<i8> {
        Some(i8::from_be_bytes(self.arr()?))
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.arr()?))
    }

    pub(crate) fn i16(&mut self) -> Option<i16> {
        Some(i16::from_be_bytes(self.arr()?))
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.arr()?))
    }

    pub(crate) fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.arr()?))
    }

    pub(crate) fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.arr()?))
    }

    /// 48-bit unsigned (PTP seconds field).
    pub(crate) fn u48(&mut self) -> Option<u64> {
        let a: [u8; 6] = self.arr()?;
        Some(a.iter().fold(0u64, |acc, &x| (acc << 8) | u64::from(x)))
    }
}

/// Writer into a fixed buffer. Writes past the end are dropped and latch
/// `overflow`; encoders size their buffers exactly and assert it stays clear.
pub(crate) struct Wr<'a> {
    b: &'a mut [u8],
    pos: usize,
    overflow: bool,
}

impl<'a> Wr<'a> {
    pub(crate) fn new(b: &'a mut [u8]) -> Self {
        Self {
            b,
            pos: 0,
            overflow: false,
        }
    }

    pub(crate) fn put(&mut self, bytes: &[u8]) {
        match self.b.get_mut(self.pos..self.pos + bytes.len()) {
            Some(dst) => {
                dst.copy_from_slice(bytes);
                self.pos += bytes.len();
            }
            None => self.overflow = true,
        }
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.put(&[v]);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.put(&v.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.put(&v.to_be_bytes());
    }

    pub(crate) fn i64(&mut self, v: i64) {
        self.put(&v.to_be_bytes());
    }

    pub(crate) fn u48(&mut self, v: u64) {
        let b = v.to_be_bytes();
        if let Some(lo) = b.get(2..) {
            self.put(lo);
        }
    }

    pub(crate) fn zeros(&mut self, n: usize) {
        for _ in 0..n {
            self.u8(0);
        }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn overflowed(&self) -> bool {
        self.overflow
    }
}
