//! Single-writer sequence lock over atomic words (design §5.1).
//!
//! The storage is `seq: AtomicU32` plus `[AtomicU64; N]`, so there are no
//! plain fields and no data races in the Rust memory model. Protocol:
//! - writer: `seq = s + 1` (odd, relaxed) → `fence(Release)` → each word
//!   (relaxed) → `seq = s + 2` (release);
//! - reader: `s1 = seq` (acquire); odd → retry; each word (relaxed) →
//!   `fence(Acquire)` → `s2 = seq` (relaxed); accept iff `s1 == s2`.
//!
//! The writer never blocks. A reader gives up after [`MAX_RETRIES`] attempts
//! with [`Contended`]; the caller keeps its previous copy and counts it.
//! [`write_words`]/[`read_words`] expose the same protocol over caller-owned
//! atomics (the ring clock block in shared memory uses them, design §10.6).

use std::marker::PhantomData;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

/// Reader attempts before [`Contended`].
pub const MAX_RETRIES: u32 = 1000;

/// The reader saw a write in progress (or a changed sequence) on every attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contended;

/// A value stored as `N` 64-bit words (f64 via `to_bits`/`from_bits`,
/// bool/u32 widened).
pub trait SeqWords<const N: usize>: Copy {
    fn to_words(self) -> [u64; N];
    fn from_words(words: [u64; N]) -> Self;
}

/// Publish `vals` into `words` under `seq`. Exactly one writer may call this
/// for a given `seq`.
pub fn write_words(seq: &AtomicU32, words: &[AtomicU64], vals: &[u64]) {
    let s = seq.load(Ordering::Relaxed);
    seq.store(s.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    for (w, v) in words.iter().zip(vals) {
        w.store(*v, Ordering::Relaxed);
    }
    seq.store(s.wrapping_add(2), Ordering::Release);
}

/// Read a consistent snapshot of `words` into `out`.
pub fn read_words(seq: &AtomicU32, words: &[AtomicU64], out: &mut [u64]) -> Result<(), Contended> {
    for _ in 0..MAX_RETRIES {
        let s1 = seq.load(Ordering::Acquire);
        if s1 & 1 == 0 {
            for (o, w) in out.iter_mut().zip(words) {
                *o = w.load(Ordering::Relaxed);
            }
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == s1 {
                return Ok(());
            }
        }
        std::hint::spin_loop();
    }
    Err(Contended)
}

/// A `T` shared by one writer and any number of readers.
pub struct SeqLock<T: SeqWords<N>, const N: usize> {
    seq: AtomicU32,
    words: [AtomicU64; N],
    _t: PhantomData<T>,
}

impl<T: SeqWords<N>, const N: usize> SeqLock<T, N> {
    pub fn new(init: T) -> Self {
        Self {
            seq: AtomicU32::new(0),
            words: init.to_words().map(AtomicU64::new),
            _t: PhantomData,
        }
    }

    /// Publish a new value. Single writer only; never blocks.
    pub fn write(&self, v: T) {
        write_words(&self.seq, &self.words, &v.to_words());
    }

    /// A consistent copy, or `Contended` after [`MAX_RETRIES`] attempts.
    pub fn read(&self) -> Result<T, Contended> {
        let mut out = [0u64; N];
        read_words(&self.seq, &self.words, &mut out)?;
        Ok(T::from_words(out))
    }
}

impl<T: SeqWords<N>, const N: usize> std::fmt::Debug for SeqLock<T, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeqLock")
            .field("seq", &self.seq.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// Every field derives from `k`, so a torn read is detectable.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Model {
        k: u64,
        neg: u64,
        f: f64,
        flag: bool,
    }

    impl Model {
        fn of(k: u64) -> Self {
            Self {
                k,
                neg: !k,
                f: k as f64 * 0.5,
                flag: k % 2 == 1,
            }
        }
        fn consistent(&self) -> bool {
            *self == Self::of(self.k)
        }
    }

    impl SeqWords<4> for Model {
        fn to_words(self) -> [u64; 4] {
            [self.k, self.neg, self.f.to_bits(), u64::from(self.flag)]
        }
        fn from_words([k, neg, f, flag]: [u64; 4]) -> Self {
            Self {
                k,
                neg,
                f: f64::from_bits(f),
                flag: flag != 0,
            }
        }
    }

    #[test]
    fn round_trip() {
        let l = SeqLock::new(Model::of(3));
        assert_eq!(l.read(), Ok(Model::of(3)));
        l.write(Model::of((-1.5f64).to_bits()));
        assert_eq!(l.read(), Ok(Model::of((-1.5f64).to_bits())));
    }

    #[test]
    fn concurrent_reads_never_tear() {
        let l = Arc::new(SeqLock::new(Model::of(0)));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let l = Arc::clone(&l);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut k = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    k += 1;
                    l.write(Model::of(k));
                }
                k
            })
        };
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let l = Arc::clone(&l);
                std::thread::spawn(move || {
                    let (mut ok, mut contended, mut last) = (0u64, 0u64, 0u64);
                    while ok < 200_000 {
                        match l.read() {
                            Ok(m) => {
                                assert!(m.consistent(), "torn read {m:?}");
                                assert!(m.k >= last, "went backwards");
                                last = m.k;
                                ok += 1;
                            }
                            Err(Contended) => contended += 1,
                        }
                    }
                    (ok, contended, last)
                })
            })
            .collect();
        for r in readers {
            let (ok, _contended, _last) = r.join().unwrap();
            assert_eq!(ok, 200_000);
        }
        stop.store(true, Ordering::Relaxed);
        let written = writer.join().unwrap();
        assert!(written > 0);
        assert!(l.read().unwrap().consistent());
    }

    #[test]
    fn contended_against_writer_held_odd() {
        let l = SeqLock::new(Model::of(7));
        // A writer that stopped between its odd store and its final store.
        l.seq.store(1, Ordering::Relaxed);
        assert_eq!(l.read(), Err(Contended));
        // Once the writer finishes, readers succeed again.
        l.seq.store(2, Ordering::Release);
        assert_eq!(l.read(), Ok(Model::of(7)));
    }

    #[test]
    fn raw_words_protocol() {
        let seq = AtomicU32::new(0);
        let words = [AtomicU64::new(0), AtomicU64::new(0)];
        write_words(&seq, &words, &[11, 22]);
        assert_eq!(seq.load(Ordering::Relaxed), 2);
        let mut out = [0u64; 2];
        assert_eq!(read_words(&seq, &words, &mut out), Ok(()));
        assert_eq!(out, [11, 22]);
        seq.store(3, Ordering::Relaxed);
        assert_eq!(read_words(&seq, &words, &mut out), Err(Contended));
    }
}
