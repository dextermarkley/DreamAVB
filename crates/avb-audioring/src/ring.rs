// SPDX-License-Identifier: MIT OR Apache-2.0

//! The ring mapping and header accessors (design §10.6). `AudioRing` owns the
//! region (anonymous mmap in Phase 1a, or a borrowed mapping via `from_raw`
//! in Phase 1b). All header access is atomic, by byte offset, so a C reader
//! sees the same bytes.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use crate::layout::{self, clock, field, RingConfig};

/// A validation failure building or attaching a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    BadMagic,
    BadVersion,
    BadSize,
    BadConfig,
    MapFailed,
}

impl std::fmt::Display for RingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            RingError::BadMagic => "ring magic mismatch",
            RingError::BadVersion => "ring version mismatch",
            RingError::BadSize => "ring size/offset mismatch",
            RingError::BadConfig => "ring config invalid",
            RingError::MapFailed => "mmap failed",
        };
        f.write_str(s)
    }
}

impl std::error::Error for RingError {}

/// How the ring memory is owned.
enum Owning {
    /// `libc::mmap(MAP_ANON|MAP_SHARED)`; unmapped on drop.
    Mmap { ptr: *mut u8, len: usize },
    /// Borrowed (Phase 1b `from_raw`); not unmapped.
    Borrowed { ptr: *mut u8, len: usize },
}

/// The recovered-audio ring (§10.6). Position-independent; `Producer` and
/// `Consumer` are the only access paths.
pub struct AudioRing {
    owning: Owning,
    capacity_frames: u64,
    frame_bytes: usize,
    channels: u32,
    data_offset: usize,
}

// The ring is shared across the audio (producer) and rec (consumer) threads;
// all access is through atomics, so it is Send + Sync.
unsafe impl Send for AudioRing {}
unsafe impl Sync for AudioRing {}

impl AudioRing {
    /// Create an anonymous shared mapping and initialise the header from
    /// `config` (Phase 1a). The one place in this crate that calls libc
    /// (design §6.1).
    pub fn new_anonymous(config: &RingConfig) -> Result<Self, RingError> {
        if !config.is_valid() {
            return Err(RingError::BadConfig);
        }
        let len = config.total_bytes();
        // SAFETY: standard anonymous shared mapping; len > 0.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_SHARED,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(RingError::MapFailed);
        }
        let ptr = ptr.cast::<u8>();
        let ring = Self {
            owning: Owning::Mmap { ptr, len },
            capacity_frames: u64::from(config.capacity_frames),
            frame_bytes: config.frame_bytes() as usize,
            channels: config.channels,
            data_offset: layout::HEADER_SIZE,
        };
        ring.init_header(config);
        Ok(ring)
    }

    /// Attach to a borrowed mapping and INITIALISE its header from `config`
    /// (the shm creator / producer side, Phase 1b-1). Like `new_anonymous` but
    /// over memory the caller mapped (e.g. an `shm_open`ed segment). Does not
    /// own the mapping — the caller unmaps it.
    ///
    /// # Safety
    /// `ptr` must point to at least `config.total_bytes()` writable bytes that
    /// stay valid for the ring's lifetime, and `len >= config.total_bytes()`.
    pub unsafe fn init_raw(
        ptr: *mut u8,
        len: usize,
        config: &RingConfig,
    ) -> Result<Self, RingError> {
        if !config.is_valid() || len < config.total_bytes() {
            return Err(RingError::BadConfig);
        }
        let ring = Self {
            owning: Owning::Borrowed { ptr, len },
            capacity_frames: u64::from(config.capacity_frames),
            frame_bytes: config.frame_bytes() as usize,
            channels: config.channels,
            data_offset: layout::HEADER_SIZE,
        };
        ring.init_header(config);
        Ok(ring)
    }

    /// Attach to an existing mapping (Phase 1b). Validates magic, version and
    /// sizes; does NOT write the header.
    ///
    /// # Safety
    /// `ptr` must point to at least `len` readable+writable bytes that remain
    /// valid for the lifetime of the returned ring.
    pub unsafe fn from_raw(ptr: *mut u8, len: usize) -> Result<Self, RingError> {
        if len < layout::HEADER_SIZE {
            return Err(RingError::BadSize);
        }
        let rd32 = |off: usize| -> u32 {
            // SAFETY: off < HEADER_SIZE ≤ len, 4-byte aligned read via atomic.
            unsafe { &*ptr.add(off).cast::<AtomicU32>() }.load(Ordering::Relaxed)
        };
        if rd32(field::MAGIC) != layout::MAGIC {
            return Err(RingError::BadMagic);
        }
        if rd32(field::VERSION) != layout::VERSION {
            return Err(RingError::BadVersion);
        }
        if rd32(field::HEADER_SIZE) as usize != layout::HEADER_SIZE {
            return Err(RingError::BadSize);
        }
        let channels = rd32(field::CHANNELS);
        let capacity_frames = rd32(field::CAPACITY_FRAMES);
        let frame_bytes = rd32(field::FRAME_BYTES) as usize;
        let data_offset = rd32(field::DATA_OFFSET) as usize;
        if frame_bytes != channels as usize * layout::BYTES_PER_SAMPLE
            || data_offset != layout::HEADER_SIZE
            || len < data_offset + capacity_frames as usize * frame_bytes
        {
            return Err(RingError::BadSize);
        }
        Ok(Self {
            owning: Owning::Borrowed { ptr, len },
            capacity_frames: u64::from(capacity_frames),
            frame_bytes,
            channels,
            data_offset,
        })
    }

    fn base(&self) -> *mut u8 {
        match self.owning {
            Owning::Mmap { ptr, .. } | Owning::Borrowed { ptr, .. } => ptr,
        }
    }

    pub fn len(&self) -> usize {
        match self.owning {
            Owning::Mmap { len, .. } | Owning::Borrowed { len, .. } => len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity_frames(&self) -> u64 {
        self.capacity_frames
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    fn init_header(&self, config: &RingConfig) {
        self.store_u32(field::MAGIC, layout::MAGIC);
        self.store_u32(field::VERSION, layout::VERSION);
        self.store_u32(field::HEADER_SIZE, layout::HEADER_SIZE as u32);
        self.store_u32(field::CHANNELS, config.channels);
        self.store_u32(field::SAMPLE_FORMAT, layout::SAMPLE_FORMAT_S32LE);
        self.store_u32(field::VALID_BITS, config.valid_bits);
        self.store_u32(field::NOMINAL_RATE_HZ, config.nominal_rate_hz);
        self.store_u32(field::CAPACITY_FRAMES, config.capacity_frames);
        self.store_u32(field::FRAME_BYTES, config.frame_bytes());
        self.store_u32(field::STREAM_COUNT, config.streams.len() as u32);
        self.store_u32(field::DATA_OFFSET, layout::HEADER_SIZE as u32);
        self.store_u64(field::CONFIG_GENERATION, 1);
        self.store_u64(field::WRITE_FRAME, 0);
        self.store_u64(field::READ_FRAME, 0);
        for (i, s) in config.streams.iter().enumerate() {
            let b = field::STREAMS + i * layout::STREAM_RECORD_SIZE;
            self.store_u64(b + layout::stream::STREAM_ID, s.stream_id);
            self.store_u32(b + layout::stream::FIRST_CHANNEL, s.first_channel);
            self.store_u32(b + layout::stream::CHANNELS, s.channels);
            self.store_u32(b + layout::stream::BIT_DEPTH, s.bit_depth);
            self.store_u32(b + layout::stream::ACTIVE, 1);
        }
        for (k, &(si, sc)) in config.channel_map.iter().enumerate() {
            if k >= layout::MAX_CHANNELS as usize {
                break;
            }
            let b = field::CHANNEL_MAP + k * layout::CHANNEL_MAP_ENTRY_SIZE;
            self.store_u32(b, u32::from(si) | (u32::from(sc) << 16));
        }
    }

    // --- atomic header accessors (by byte offset) ---

    fn atomic_u32(&self, off: usize) -> &AtomicU32 {
        // SAFETY: off < HEADER_SIZE and 4-aligned; the region lives as long as self.
        unsafe { &*self.base().add(off).cast::<AtomicU32>() }
    }

    fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        // SAFETY: off < HEADER_SIZE and 8-aligned; the region lives as long as self.
        unsafe { &*self.base().add(off).cast::<AtomicU64>() }
    }

    pub(crate) fn store_u32(&self, off: usize, v: u32) {
        self.atomic_u32(off).store(v, Ordering::Relaxed);
    }

    pub(crate) fn load_u32(&self, off: usize) -> u32 {
        self.atomic_u32(off).load(Ordering::Relaxed)
    }

    pub(crate) fn store_u64(&self, off: usize, v: u64) {
        self.atomic_u64(off).store(v, Ordering::Relaxed);
    }

    pub(crate) fn load_u64(&self, off: usize) -> u64 {
        self.atomic_u64(off).load(Ordering::Relaxed)
    }

    pub(crate) fn write_frame(&self) -> u64 {
        self.atomic_u64(field::WRITE_FRAME).load(Ordering::Acquire)
    }

    pub(crate) fn store_write_frame(&self, v: u64) {
        self.atomic_u64(field::WRITE_FRAME)
            .store(v, Ordering::Release);
    }

    pub(crate) fn read_frame(&self) -> u64 {
        self.atomic_u64(field::READ_FRAME).load(Ordering::Acquire)
    }

    pub(crate) fn store_read_frame(&self, v: u64) {
        self.atomic_u64(field::READ_FRAME)
            .store(v, Ordering::Release);
    }

    pub(crate) fn add_counter(&self, idx: usize, delta: u64) {
        if idx >= layout::counter::COUNT {
            return;
        }
        let off = field::COUNTERS + idx * 8;
        self.atomic_u64(off).fetch_add(delta, Ordering::Relaxed);
    }

    pub fn counter(&self, idx: usize) -> u64 {
        if idx >= layout::counter::COUNT {
            return 0;
        }
        self.load_u64(field::COUNTERS + idx * 8)
    }

    /// Data byte range of absolute frame `f` for `n` frames, wrapping the ring.
    /// Returns the byte offset of frame `f` within the data region.
    pub(crate) fn frame_byte_offset(&self, f: u64) -> usize {
        let slot = (f % self.capacity_frames) as usize;
        self.data_offset + slot * self.frame_bytes
    }

    /// A mutable byte slice for `count` frames starting at absolute frame `f`,
    /// provided it does not wrap the ring end (callers split at the wrap).
    ///
    /// # Safety
    /// Single producer only; the range must be within the mapping. The
    /// `&self` → `&mut [u8]` is sound because only one `Producer` ever writes
    /// the data region (SPSC, design §10.6).
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn frames_mut(&self, f: u64, count: usize) -> &mut [u8] {
        let off = self.frame_byte_offset(f);
        let bytes = count * self.frame_bytes;
        // SAFETY: off + bytes ≤ len by the caller's wrap split; single writer.
        unsafe { std::slice::from_raw_parts_mut(self.base().add(off), bytes) }
    }

    /// A read-only byte slice for `count` frames starting at absolute frame
    /// `f`, not wrapping.
    pub(crate) fn frames(&self, f: u64, count: usize) -> &[u8] {
        let off = self.frame_byte_offset(f);
        let bytes = count * self.frame_bytes;
        // SAFETY: within the mapping by the caller's wrap split.
        unsafe { std::slice::from_raw_parts(self.base().add(off).cast_const(), bytes) }
    }

    // --- clock block seqlock (§10.6; uses the avb-core protocol) ---

    /// Publish which clock is disciplining this ring's sample rate.
    ///
    /// A plain `u32`, not part of the clock-block seqlock: it changes at human
    /// speed (when the user picks a source, or a lock comes and goes), so a
    /// single relaxed store is enough and a reader can never see a torn value.
    /// Pass `avb_config::EffectiveClock::code()`; this crate deliberately does
    /// not depend on `avb-config`, so the meaning lives there.
    pub fn set_effective_clock(&self, code: u32) {
        self.store_u32(field::EFFECTIVE_CLOCK, code);
    }

    /// The published effective-clock code, or 0 if a daemon never wrote one.
    pub fn effective_clock(&self) -> u32 {
        self.load_u32(field::EFFECTIVE_CLOCK)
    }

    /// Publish the clock model into the header's clock block (writer = audio).
    pub fn publish_clock(&self, m: &crate::ClockBlock) {
        let base = field::CLOCK_BLOCK;
        let seq = self.atomic_u32(base + clock::SEQ);
        let s = seq.load(Ordering::Relaxed);
        seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.store_u64(base + clock::ANCHOR_FRAME, m.anchor_frame);
        self.store_u64(base + clock::ANCHOR_MACH, m.anchor_mach.to_bits());
        self.store_u64(base + clock::MACH_PER_FRAME, m.mach_per_frame.to_bits());
        self.store_u64(base + clock::RATE_HZ_VS_MACH, m.rate_hz_vs_mach.to_bits());
        self.store_u64(
            base + clock::MEDIA_PPM_VS_GPTP,
            m.media_ppm_vs_gptp.to_bits(),
        );
        self.store_u64(base + clock::UNCERTAINTY_NS, m.uncertainty_ns.to_bits());
        self.store_u32(base + clock::LOCKED, u32::from(m.locked));
        self.store_u32(base + clock::RESET_COUNT, m.reset_count);
        seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Read a consistent snapshot of the clock block (reader = rec / Phase 1b).
    /// Returns `None` if a write was in progress on every retry.
    pub fn read_clock(&self) -> Option<crate::ClockBlock> {
        let base = field::CLOCK_BLOCK;
        let seq = self.atomic_u32(base + clock::SEQ);
        for _ in 0..avb_core::seqlock::MAX_RETRIES {
            let s1 = seq.load(Ordering::Acquire);
            if s1 & 1 == 0 {
                let m = crate::ClockBlock {
                    anchor_frame: self.load_u64(base + clock::ANCHOR_FRAME),
                    anchor_mach: f64::from_bits(self.load_u64(base + clock::ANCHOR_MACH)),
                    mach_per_frame: f64::from_bits(self.load_u64(base + clock::MACH_PER_FRAME)),
                    rate_hz_vs_mach: f64::from_bits(self.load_u64(base + clock::RATE_HZ_VS_MACH)),
                    media_ppm_vs_gptp: f64::from_bits(
                        self.load_u64(base + clock::MEDIA_PPM_VS_GPTP),
                    ),
                    uncertainty_ns: f64::from_bits(self.load_u64(base + clock::UNCERTAINTY_NS)),
                    locked: self.load_u32(base + clock::LOCKED) != 0,
                    reset_count: self.load_u32(base + clock::RESET_COUNT),
                };
                fence(Ordering::Acquire);
                if seq.load(Ordering::Relaxed) == s1 {
                    return Some(m);
                }
            }
            std::hint::spin_loop();
        }
        None
    }
}

impl Drop for AudioRing {
    fn drop(&mut self) {
        if let Owning::Mmap { ptr, len } = self.owning {
            // SAFETY: ptr/len came from our own mmap.
            unsafe {
                libc::munmap(ptr.cast(), len);
            }
        }
    }
}
