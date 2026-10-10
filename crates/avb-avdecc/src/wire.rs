//! Bounds-checked big-endian cursors (`clippy::indexing_slicing` is denied).

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

    pub(crate) fn rest(&self) -> &'a [u8] {
        self.b
    }

    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (h, t) = self.b.split_at_checked(n)?;
        self.b = t;
        Some(h)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
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

/// Growable big-endian writer.
#[derive(Default)]
pub(crate) struct Wr {
    pub(crate) b: Vec<u8>,
}

impl Wr {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn put(&mut self, bytes: &[u8]) {
        self.b.extend_from_slice(bytes);
    }

    pub(crate) fn zeros(&mut self, n: usize) {
        self.b.resize(self.b.len() + n, 0);
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.b.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.put(&v.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.put(&v.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.put(&v.to_be_bytes());
    }

    pub(crate) fn len(&self) -> usize {
        self.b.len()
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.b
    }
}
