//! WAV / RF64 header construction (design §11.3). Pure byte-builders: the
//! daemon's `rec` thread writes the 104-byte header, then streams data, then
//! patches sizes (or rewrites to RF64) on finalize. Golden bytes are pinned
//! by tests.
//!
//! Layout written up front (104 B): `RIFF`(12) + `JUNK`(36) + `fmt `(48) +
//! `data`(8). On finalize, if the RIFF size fits in u32 it is a standard WAV;
//! otherwise the `JUNK` becomes a `ds64` chunk and the magic becomes `RF64`
//! (EBU Tech 3306).

/// KSDATAFORMAT_SUBTYPE_PCM GUID (§11.3).
pub const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

/// WAVE_FORMAT_EXTENSIBLE tag.
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// The fixed header length written before data (§11.3: 104 B).
pub const HEADER_LEN: usize = 104;

/// u32::MAX, the "see RF64" sentinel.
const U32_MAX: u32 = 0xFFFF_FFFF;

/// WAV parameters derived from the ring (§11.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavFormat {
    pub channels: u16,
    pub sample_rate: u32,
    /// Container bits (always 32 here).
    pub bits_per_sample: u16,
    /// `wValidBitsPerSample` = ring valid_bits.
    pub valid_bits: u16,
}

impl WavFormat {
    pub fn block_align(&self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }

    pub fn avg_bytes_per_sec(&self) -> u32 {
        self.sample_rate * u32::from(self.block_align())
    }
}

fn put_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_u64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// The 104-byte header written before any audio (sizes are placeholders,
/// patched on finalize). `RIFF`(12) + `JUNK`(36) + `fmt `(48) + `data`(8).
pub fn initial_header(fmt: &WavFormat) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    // RIFF chunk (12 B): "RIFF", size (placeholder 0), "WAVE".
    h.extend_from_slice(b"RIFF");
    put_u32(&mut h, 0);
    h.extend_from_slice(b"WAVE");
    // JUNK chunk (8 + 28 = 36 B): reserved for ds64.
    h.extend_from_slice(b"JUNK");
    put_u32(&mut h, 28);
    h.extend_from_slice(&[0u8; 28]);
    // fmt chunk (8 + 40 = 48 B), WAVE_FORMAT_EXTENSIBLE.
    h.extend_from_slice(b"fmt ");
    put_u32(&mut h, 40);
    put_u16(&mut h, WAVE_FORMAT_EXTENSIBLE);
    put_u16(&mut h, fmt.channels);
    put_u32(&mut h, fmt.sample_rate);
    put_u32(&mut h, fmt.avg_bytes_per_sec());
    put_u16(&mut h, fmt.block_align());
    put_u16(&mut h, fmt.bits_per_sample);
    put_u16(&mut h, 22); // cbSize
    put_u16(&mut h, fmt.valid_bits);
    put_u32(&mut h, 0); // dwChannelMask
    h.extend_from_slice(&SUBTYPE_PCM);
    // data chunk header (8 B): "data", size (placeholder 0).
    h.extend_from_slice(b"data");
    put_u32(&mut h, 0);
    debug_assert_eq!(h.len(), HEADER_LEN);
    h
}

/// Finalize the header given the total `data_bytes` written. Returns the full
/// 104-byte header to overwrite the file's start (§11.3). Chooses standard
/// WAV or RF64 depending on whether the RIFF size fits in u32.
pub fn finalize_header(fmt: &WavFormat, data_bytes: u64) -> Vec<u8> {
    let riff_size = HEADER_LEN as u64 - 8 + data_bytes; // everything after "RIFF"+size
    if riff_size <= u64::from(U32_MAX) {
        finalize_wav(fmt, data_bytes, riff_size as u32)
    } else {
        finalize_rf64(fmt, data_bytes, riff_size)
    }
}

fn finalize_wav(fmt: &WavFormat, data_bytes: u64, riff_size: u32) -> Vec<u8> {
    let mut h = initial_header(fmt);
    // Patch RIFF size at offset 4.
    patch_u32(&mut h, 4, riff_size);
    // Patch data size at offset 100 (HEADER_LEN - 4).
    patch_u32(&mut h, HEADER_LEN - 4, data_bytes as u32);
    h
}

fn finalize_rf64(fmt: &WavFormat, data_bytes: u64, riff_size: u64) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    // RF64 chunk: "RF64", size 0xFFFFFFFF, "WAVE".
    h.extend_from_slice(b"RF64");
    put_u32(&mut h, U32_MAX);
    h.extend_from_slice(b"WAVE");
    // ds64 chunk replacing JUNK (8 + 28 = 36 B).
    h.extend_from_slice(b"ds64");
    put_u32(&mut h, 28);
    put_u64(&mut h, riff_size);
    put_u64(&mut h, data_bytes);
    let sample_count = if fmt.block_align() > 0 {
        data_bytes / u64::from(fmt.block_align())
    } else {
        0
    };
    put_u64(&mut h, sample_count);
    put_u32(&mut h, 0); // tableLength
                        // fmt chunk (same 48 B as the standard header).
    h.extend_from_slice(b"fmt ");
    put_u32(&mut h, 40);
    put_u16(&mut h, WAVE_FORMAT_EXTENSIBLE);
    put_u16(&mut h, fmt.channels);
    put_u32(&mut h, fmt.sample_rate);
    put_u32(&mut h, fmt.avg_bytes_per_sec());
    put_u16(&mut h, fmt.block_align());
    put_u16(&mut h, fmt.bits_per_sample);
    put_u16(&mut h, 22);
    put_u16(&mut h, fmt.valid_bits);
    put_u32(&mut h, 0);
    h.extend_from_slice(&SUBTYPE_PCM);
    // data chunk: "data", size 0xFFFFFFFF (real size is in ds64).
    h.extend_from_slice(b"data");
    put_u32(&mut h, U32_MAX);
    debug_assert_eq!(h.len(), HEADER_LEN);
    h
}

fn patch_u32(h: &mut [u8], off: usize, v: u32) {
    if let Some(slot) = h.get_mut(off..off + 4) {
        slot.copy_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn fmt_8ch_48k_24() -> WavFormat {
        WavFormat {
            channels: 8,
            sample_rate: 48_000,
            bits_per_sample: 32,
            valid_bits: 24,
        }
    }

    #[test]
    fn header_is_104_bytes_and_well_formed() {
        let f = fmt_8ch_48k_24();
        let h = initial_header(&f);
        assert_eq!(h.len(), 104);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"JUNK");
        assert_eq!(u32::from_le_bytes([h[16], h[17], h[18], h[19]]), 28);
        assert_eq!(&h[48..52], b"fmt ");
        assert_eq!(u32::from_le_bytes([h[52], h[53], h[54], h[55]]), 40);
        assert_eq!(u16::from_le_bytes([h[56], h[57]]), WAVE_FORMAT_EXTENSIBLE);
        assert_eq!(u16::from_le_bytes([h[58], h[59]]), 8); // channels
        assert_eq!(u32::from_le_bytes([h[60], h[61], h[62], h[63]]), 48_000);
        // nAvgBytesPerSec = 48000 * 32 = 1_536_000.
        assert_eq!(u32::from_le_bytes([h[64], h[65], h[66], h[67]]), 1_536_000);
        assert_eq!(u16::from_le_bytes([h[68], h[69]]), 32); // block align 8*4
        assert_eq!(u16::from_le_bytes([h[70], h[71]]), 32); // bits
        assert_eq!(u16::from_le_bytes([h[72], h[73]]), 22); // cbSize
        assert_eq!(u16::from_le_bytes([h[74], h[75]]), 24); // valid bits
        assert_eq!(&h[80..96], &SUBTYPE_PCM); // after dwChannelMask at 76
        assert_eq!(&h[96..100], b"data");
    }

    #[test]
    fn finalize_standard_wav_patches_sizes() {
        let f = fmt_8ch_48k_24();
        let data_bytes = 48_000u64 * 32 * 10; // 10 s, 15_360_000 B
        let h = finalize_header(&f, data_bytes);
        assert_eq!(&h[0..4], b"RIFF");
        let riff = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert_eq!(u64::from(riff), 104 - 8 + data_bytes);
        let data = u32::from_le_bytes([h[100], h[101], h[102], h[103]]);
        assert_eq!(u64::from(data), data_bytes);
    }

    #[test]
    fn finalize_rf64_above_4gib_without_writing() {
        let f = fmt_8ch_48k_24();
        // > 4 GiB of data (compute only, no file).
        let data_bytes = 5_000_000_000u64;
        let h = finalize_header(&f, data_bytes);
        assert_eq!(&h[0..4], b"RF64");
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 0xFFFF_FFFF);
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"ds64");
        let riff_size = u64::from_le_bytes(h[20..28].try_into().unwrap());
        assert_eq!(riff_size, 104 - 8 + data_bytes);
        let ds64_data = u64::from_le_bytes(h[28..36].try_into().unwrap());
        assert_eq!(ds64_data, data_bytes);
        let sample_count = u64::from_le_bytes(h[36..44].try_into().unwrap());
        assert_eq!(sample_count, data_bytes / 32);
        // data chunk size is the sentinel.
        assert_eq!(
            u32::from_le_bytes([h[100], h[101], h[102], h[103]]),
            0xFFFF_FFFF
        );
        assert_eq!(h.len(), 104);
    }

    #[test]
    fn rf64_boundary_is_4gib() {
        let f = fmt_8ch_48k_24();
        // riff_size = 96 + data_bytes; standard while riff_size <= u32::MAX.
        let data_at_max = u64::from(U32_MAX) - (HEADER_LEN as u64 - 8);
        assert_eq!(&finalize_header(&f, data_at_max)[0..4], b"RIFF");
        assert_eq!(&finalize_header(&f, data_at_max + 1)[0..4], b"RF64");
    }
}
