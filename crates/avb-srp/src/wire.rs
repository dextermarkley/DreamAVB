//! Bounds-checked big-endian cursors (`clippy::indexing_slicing` is denied).

pub(crate) struct Rd<'a> {
    b: &'a [u8],
}

impl<'a> Rd<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Self { b }
    }

    pub(crate) fn peek_u16(&self) -> Option<u16> {
        let a: [u8; 2] = self.b.get(..2)?.try_into().ok()?;
        Some(u16::from_be_bytes(a))
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

    pub(crate) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.arr()?))
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.arr()?))
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.arr()?))
    }
}

/// Writer into a fixed buffer; writes past the end latch `overflow`.
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

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn overflowed(&self) -> bool {
        self.overflow
    }
}
