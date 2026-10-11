// SPDX-License-Identifier: MIT OR Apache-2.0

//! File-backed shared memory for the recovered-audio ring (Phase 1b-1).
//!
//! The daemon (producer) creates a memory-mapped FILE at a fixed world-
//! accessible path, sizes it to the ring's `total_bytes`, maps it read/write,
//! and initialises the ring header. The CoreAudio plug-in (consumer), in
//! `coreaudiod`'s separate process, opens the same path read/write and
//! `AudioRing::from_raw`-attaches it. A FILE (not POSIX `shm_open`) is used
//! because macOS scopes shm names to the creator's bootstrap/sandbox namespace,
//! making a root daemon's segment invisible to the non-root plug-in. Both see
//! identical bytes; all header access is atomic, so no locking is needed.
//!
//! Only this module calls `libc::open`/`ftruncate`/`mmap` for the shared case;
//! `AudioRing` itself stays position-independent via `from_raw`.

use std::ffi::CString;
use std::os::raw::c_int;

use crate::layout::RingConfig;
use crate::ring::{AudioRing, RingError};

/// The stable shm name both processes agree on without the control socket
/// (the socket, 1b-2, later carries this + geometry; until then it is fixed).
/// macOS limits an shm name to 31 chars including the leading '/'.
/// The ring is backed by a memory-mapped FILE at a fixed, world-accessible
/// path — NOT a POSIX `shm_open` segment. macOS scopes POSIX shm names to the
/// creator's bootstrap/sandbox namespace, so a root-launched daemon's segment
/// is invisible (ENOENT) to the non-root `_coreaudiod` plug-in. A plain file
/// both open by absolute path has no such namespace boundary. `/Users/Shared`
/// is world-readable/writable and reachable by every user on the system.
pub const SHM_NAME: &str = "/Users/Shared/avb16r.ring";

/// The output (playback) ring: Mac audio OUT to the 16R (talker direction,
/// Phase 1b-2). Same ABI and backing-file rationale as [`SHM_NAME`], a
/// distinct path so the input and output rings never collide. The CoreAudio
/// plug-in's output stream is the PRODUCER (WriteMix) and the daemon's talker
/// TX loop is the CONSUMER — the mirror of the input ring's roles.
pub const SHM_NAME_OUT: &str = "/Users/Shared/avb16r-out.ring";

/// A mapped shm segment that owns its `mmap`/fd and, optionally, unlinks the
/// name on drop (the creator unlinks; an attacher does not).
pub struct ShmRing {
    ring: AudioRing,
    ptr: *mut u8,
    len: usize,
    fd: c_int,
    name: CString,
    unlink_on_drop: bool,
}

// The underlying AudioRing is Send+Sync (atomic-only access); the owning fd and
// pointer are only touched on drop.
unsafe impl Send for ShmRing {}
unsafe impl Sync for ShmRing {}

impl ShmRing {
    /// The attached ring (producer or consumer wraps this).
    pub fn ring(&self) -> &AudioRing {
        &self.ring
    }

    /// Create (or re-create) the shm segment, size it to `config`, map it
    /// read/write and initialise the header. Daemon/producer side. Unlinks any
    /// stale segment of the same name first so a crashed prior run cannot leave
    /// a wrongly-sized mapping behind. The returned handle unlinks on drop.
    pub fn create(config: &RingConfig) -> Result<Self, RingError> {
        Self::create_at(SHM_NAME, config)
    }

    /// As [`create`](Self::create) but at an explicit path (e.g.
    /// [`SHM_NAME_OUT`](crate::SHM_NAME_OUT) for the output ring).
    pub fn create_at(path: &str, config: &RingConfig) -> Result<Self, RingError> {
        if !config.is_valid() {
            return Err(RingError::BadConfig);
        }
        let name = CString::new(path).map_err(|_| RingError::BadConfig)?;
        let len = config.total_bytes();
        // Remove any stale file first so a crashed prior run cannot leave a
        // wrongly-sized mapping behind (ignore ENOENT).
        // SAFETY: name is a valid C string.
        unsafe { libc::unlink(name.as_ptr()) };
        // Create the backing FILE (not shm_open — see SHM_NAME doc).
        // SAFETY: standard open with O_CREAT|O_RDWR|O_EXCL, 0666.
        let fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                0o666,
            )
        };
        if fd < 0 {
            return Err(RingError::MapFailed);
        }
        // open's mode is masked by umask; the daemon runs as root but the
        // plug-in runs as the non-root _coreaudiod user and must open this
        // file O_RDWR — force world read/write past umask with fchmod.
        unsafe { libc::fchmod(fd, 0o666) };
        // SAFETY: fd is a valid file fd; sizing it to the full ring length.
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            unsafe {
                libc::close(fd);
                libc::unlink(name.as_ptr());
            }
            return Err(RingError::MapFailed);
        }
        let ptr = Self::map(fd, len)?;
        // Initialise the header by taking ownership through a transient
        // anonymous-style init: from_raw does not write the header, so we must
        // write it here using the same offsets. We do that by constructing a
        // writable AudioRing over the mapping and initialising it.
        // SAFETY: ptr/len are our fresh zeroed mapping; valid for its lifetime.
        let ring = unsafe { AudioRing::init_raw(ptr, len, config)? };
        Ok(Self {
            ring,
            ptr,
            len,
            fd,
            name,
            unlink_on_drop: true,
        })
    }

    /// Open an existing shm segment read/write and attach a ring over it.
    /// Plug-in/consumer side. Validates the header via `from_raw`. Does NOT
    /// unlink on drop (the creator owns the name's lifetime).
    pub fn attach() -> Result<Self, RingError> {
        Self::attach_at(SHM_NAME)
    }

    /// As [`attach`](Self::attach) but at an explicit path (e.g.
    /// [`SHM_NAME_OUT`](crate::SHM_NAME_OUT) for the output ring).
    pub fn attach_at(path: &str) -> Result<Self, RingError> {
        let name = CString::new(path).map_err(|_| RingError::BadConfig)?;
        Self::attach_inner(name, true)
    }

    /// Attach READ-ONLY: open `O_RDONLY` and map `PROT_READ`. For consumers
    /// that only read counters and the clock block (the companion app's stats
    /// panel), so a GUI process never holds a writable mapping of the live
    /// audio ring (review L10). Writing through this ring would fault.
    pub fn attach_ro() -> Result<Self, RingError> {
        Self::attach_ro_at(SHM_NAME)
    }

    /// As [`attach_ro`](Self::attach_ro) but at an explicit path.
    pub fn attach_ro_at(path: &str) -> Result<Self, RingError> {
        let name = CString::new(path).map_err(|_| RingError::BadConfig)?;
        Self::attach_inner(name, false)
    }

    fn attach_inner(name: CString, writable: bool) -> Result<Self, RingError> {
        // Open the backing FILE (not shm_open — see SHM_NAME doc).
        // SAFETY: name is a valid C string.
        let flags = if writable {
            libc::O_RDWR
        } else {
            libc::O_RDONLY
        };
        let fd = unsafe { libc::open(name.as_ptr(), flags) };
        if fd < 0 {
            return Err(RingError::MapFailed);
        }
        // Discover the size via fstat so the consumer need not know geometry
        // ahead of time (from_raw re-validates it against the header).
        // SAFETY: zeroed stat, valid fd.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 || st.st_size <= 0 {
            unsafe { libc::close(fd) };
            return Err(RingError::MapFailed);
        }
        let len = st.st_size as usize;
        let ptr = Self::map_prot(fd, len, writable)?;
        // SAFETY: ptr/len map a segment the creator initialised; from_raw
        // validates magic/version/sizes and refuses a bad or partial mapping.
        let ring = match unsafe { AudioRing::from_raw(ptr, len) } {
            Ok(r) => r,
            Err(e) => {
                unsafe {
                    libc::munmap(ptr.cast(), len);
                    libc::close(fd);
                }
                return Err(e);
            }
        };
        Ok(Self {
            ring,
            ptr,
            len,
            fd,
            name,
            unlink_on_drop: false,
        })
    }

    fn map(fd: c_int, len: usize) -> Result<*mut u8, RingError> {
        Self::map_prot(fd, len, true)
    }

    fn map_prot(fd: c_int, len: usize, writable: bool) -> Result<*mut u8, RingError> {
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        // SAFETY: mapping the whole fd shared, with the requested protection.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, 0) };
        if ptr == libc::MAP_FAILED {
            // SAFETY: fd is ours.
            unsafe { libc::close(fd) };
            return Err(RingError::MapFailed);
        }
        Ok(ptr.cast::<u8>())
    }
}

impl Drop for ShmRing {
    fn drop(&mut self) {
        // SAFETY: ptr/len/fd came from our own map()/open().
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
            libc::close(self.fd);
            if self.unlink_on_drop {
                libc::unlink(self.name.as_ptr());
            }
        }
    }
}

/// A producer that OWNS its shm segment, so the `Producer`'s borrow of the ring
/// is internal and sound (the daemon holds this for the whole run). Wraps the
/// unavoidable self-reference — the ring mapping is pinned on the heap for the
/// producer's lifetime and freed together on drop.
pub struct ShmProducer {
    // SAFETY INVARIANT: borrows `*shm`. Declared FIRST so it is dropped first
    // (Rust drops fields in declaration order), before the mapping it borrows.
    // Declared 'static and never handed out.
    producer: crate::producer::Producer<'static>,
    // Boxed so the mapping address is stable while `producer` borrows it.
    shm: Box<ShmRing>,
}

impl ShmProducer {
    /// Create the shm segment from `config` and a producer over it.
    /// `lag_frames`/`silent_lag` come from the media clock's `RateConsts`.
    pub fn create(
        config: &RingConfig,
        streams: &[(usize, usize, u32)],
        lag_frames: u64,
        silent_lag: u64,
    ) -> Result<Self, RingError> {
        Self::create_at(SHM_NAME, config, streams, lag_frames, silent_lag)
    }

    /// As [`create`](Self::create) but at an explicit path (output ring uses
    /// [`SHM_NAME_OUT`](crate::SHM_NAME_OUT)).
    pub fn create_at(
        path: &str,
        config: &RingConfig,
        streams: &[(usize, usize, u32)],
        lag_frames: u64,
        silent_lag: u64,
    ) -> Result<Self, RingError> {
        let shm = Box::new(ShmRing::create_at(path, config)?);
        // SAFETY: the Producer borrows the ring inside `shm`, which is boxed
        // (stable address) and dropped AFTER `producer` (field order), so the
        // borrow never dangles. We never expose the 'static borrow.
        let ring_ref: &'static AudioRing = unsafe { &*(shm.ring() as *const AudioRing) };
        let producer = crate::producer::Producer::new(ring_ref, streams, lag_frames, silent_lag);
        Ok(Self { shm, producer })
    }

    pub fn ring(&self) -> &AudioRing {
        self.shm.ring()
    }

    /// ATTACH to an existing ring at `path` (created by another process) and
    /// build a producer that resumes at its current write frontier. The ring
    /// is not unlinked on drop (the creator owns the name). Used by the
    /// CoreAudio plug-in to produce into the daemon's output ring.
    pub fn attach_at(
        path: &str,
        streams: &[(usize, usize, u32)],
        lag_frames: u64,
        silent_lag: u64,
    ) -> Result<Self, RingError> {
        let shm = Box::new(ShmRing::attach_at(path)?);
        // SAFETY: same self-borrow invariant as `create_at` — `shm` is boxed
        // (stable address) and dropped after `producer` (field order).
        let ring_ref: &'static AudioRing = unsafe { &*(shm.ring() as *const AudioRing) };
        let producer = crate::producer::Producer::resume(ring_ref, streams, lag_frames, silent_lag);
        Ok(Self { shm, producer })
    }

    pub fn producer(&mut self) -> &mut crate::producer::Producer<'static> {
        &mut self.producer
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::layout::StreamSlot;
    use crate::producer::{Producer, SampleFormat};
    use crate::Consumer;

    fn cfg() -> RingConfig {
        RingConfig {
            channels: 8,
            valid_bits: 24,
            nominal_rate_hz: 48_000,
            capacity_frames: 1024,
            streams: vec![StreamSlot {
                stream_id: 0x000a_9203_3ba9_0001,
                first_channel: 0,
                channels: 8,
                bit_depth: 24,
            }],
            channel_map: (0..8).map(|c| (0u16, c as u16)).collect(),
        }
    }

    #[test]
    fn create_attach_roundtrip_cross_mapping() {
        // The creator makes the segment; a SECOND independent mapping (as the
        // plug-in's separate process would) attaches and reads what the
        // producer wrote — proving the shared bytes line up across mappings.
        let creator = match ShmRing::create(&cfg()) {
            Ok(c) => c,
            // Sandboxes without shm (CI) — skip rather than fail the gate.
            Err(RingError::MapFailed) => return,
            Err(e) => panic!("create failed: {e}"),
        };
        let mut p = Producer::new(creator.ring(), &[(0, 8, 24)], 192, 192);
        // One frame: ch0 = 0x11223344 big-endian, rest zero.
        let mut data = vec![0u8; 8 * 4];
        data[0..4].copy_from_slice(&0x1122_3344u32.to_be_bytes());
        assert!(p.place(0, 0, 1, SampleFormat::Int32, &data));
        p.commit(false, None);

        let attached = ShmRing::attach().expect("attach existing segment");
        assert_eq!(attached.ring().channels(), 8);
        let mut c = Consumer::new(attached.ring());
        let mut out = vec![0u8; attached.ring().frame_bytes()];
        assert_eq!(c.read_into(&mut out, 1), 1);
        // Little-endian of the big-endian wire value lands in ch0.
        assert_eq!(&out[0..4], &0x1122_3344u32.to_le_bytes());
    }

    /// The OUTPUT path contract, end to end across two mappings: the daemon
    /// creates the ring exactly as `play` does, the plug-in attaches with
    /// exactly the spec `attach_out()` uses, writes, and the daemon's consumer
    /// reads it back.
    ///
    /// The suite had no test for this direction. On the real hardware the
    /// talker sent 1,437,707 PDUs and counted 1,437,707 underruns — the ring
    /// was empty for every single packet — so this pins the half of that
    /// failure that lives in the ring API, separating it from "the HAL never
    /// called WriteMix".
    #[test]
    fn output_ring_daemon_creates_plugin_attaches_and_writes() {
        // The daemon's `play` config, field for field (stack.rs): a non-zero
        // stream id from our MAC, 8 channels, 32-bit, 48 kHz, 65536 frames.
        let path = format!("/Users/Shared/avb16r-out-test-{}.ring", std::process::id());
        let cfg = RingConfig {
            channels: 8,
            valid_bits: 32,
            nominal_rate_hz: 48_000,
            capacity_frames: 65_536,
            streams: vec![StreamSlot {
                stream_id: 0x644b_f012_73b8_0000,
                first_channel: 0,
                channels: 8,
                bit_depth: 32,
            }],
            channel_map: (0..8).map(|c| (0u16, c as u16)).collect(),
        };
        let daemon_side = match ShmRing::create_at(&path, &cfg) {
            Ok(r) => r,
            // Sandboxes without shm (CI) — skip rather than fail the gate.
            Err(RingError::MapFailed) => return,
            Err(e) => panic!("daemon-side create failed: {e}"),
        };

        // The plug-in's attach_out() spec: (first_channel, channels, bit_depth)
        // with no lag, as a SEPARATE mapping of the same file.
        let mut plugin_side = ShmProducer::attach_at(&path, &[(0, 8, 32)], 0, 0)
            .expect("plug-in could not attach the daemon's output ring");
        assert_eq!(
            plugin_side.ring().channels(),
            8,
            "channel count the plug-in checks before attaching"
        );

        // write_mix's job: place S32 frames at the live frontier and commit.
        let frames = 4usize;
        let mut data = vec![0u8; frames * 8 * 4];
        for fr in 0..frames {
            let v = (0x0100_0000u32 + fr as u32).to_be_bytes();
            let off = fr * 8 * 4;
            data[off..off + 4].copy_from_slice(&v);
        }
        let start = plugin_side.ring().write_frame();
        assert!(
            plugin_side
                .producer()
                .place(0, start as i128, frames, SampleFormat::Int32, &data),
            "place rejected at the ring's own live frontier"
        );
        plugin_side.producer().commit(false, None);

        // The daemon's talker loop consumes from its own mapping.
        let mut c = Consumer::with_read_frame(daemon_side.ring(), start);
        let fb = daemon_side.ring().frame_bytes();
        let mut out = vec![0u8; frames * fb];
        assert_eq!(
            c.read_into(&mut out, frames),
            frames,
            "daemon read nothing the plug-in committed — this is the underrun"
        );
        for fr in 0..frames {
            let got = u32::from_le_bytes([
                out[fr * fb],
                out[fr * fb + 1],
                out[fr * fb + 2],
                out[fr * fb + 3],
            ]);
            assert_eq!(got, 0x0100_0000 + fr as u32, "frame {fr} corrupted");
        }
    }
}
