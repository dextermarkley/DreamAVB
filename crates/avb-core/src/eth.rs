// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ethernet II frames with at most one IEEE 802.1Q C-tag: parse and build.
//!
//! Frames arrive without FCS. A VLAN tag is either in the frame bytes
//! (`0x8100` at offset 12) or, when the NIC stripped it, supplied by the RX
//! descriptor. The in-frame tag always wins (design §3.4 rule 1, U-12).

use crate::mac::MacAddr;
use crate::rx::DropReason;

pub const ETH_HDR_LEN: usize = 14;
pub const VLAN_TAG_LEN: usize = 4;
pub const ETHERTYPE_VLAN: u16 = 0x8100;
pub const ETHERTYPE_QINQ: u16 = 0x88A8;

/// An 802.1Q tag control information field, decoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VlanTag {
    /// Priority code point, 0..=7.
    pub pcp: u8,
    /// Drop eligible indicator.
    pub dei: bool,
    /// VLAN identifier, 0..=4095.
    pub vid: u16,
}

impl VlanTag {
    pub const fn from_tci(tci: u16) -> Self {
        Self {
            pcp: (tci >> 13) as u8,
            dei: tci & 0x1000 != 0,
            vid: tci & 0x0fff,
        }
    }

    pub const fn tci(self) -> u16 {
        ((self.pcp as u16 & 7) << 13) | ((self.dei as u16) << 12) | (self.vid & 0x0fff)
    }
}

/// Where a parsed frame's VLAN tag came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VlanSource {
    #[default]
    None,
    /// `0x8100` tag in the frame bytes.
    InFrame,
    /// Tag stripped by the NIC and reported in the RX descriptor.
    Descriptor,
}

/// Addressing part of an Ethernet frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EthHeader {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub vlan: Option<VlanTag>,
    /// The (inner, if tagged) EtherType.
    pub ethertype: u16,
}

/// Bounds-checked sequential writer over a byte slice.
struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.pos.checked_add(bytes.len())?;
        self.buf.get_mut(self.pos..end)?.copy_from_slice(bytes);
        self.pos = end;
        Some(())
    }
}

impl EthHeader {
    /// Encoded header length: 14, or 18 with a VLAN tag.
    pub const fn len(&self) -> usize {
        if self.vlan.is_some() {
            ETH_HDR_LEN + VLAN_TAG_LEN
        } else {
            ETH_HDR_LEN
        }
    }

    /// Always false; a header is never empty (present for clippy's
    /// `len_without_is_empty`).
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Write the header to the front of `out`; returns its length, or `None`
    /// if `out` is too short.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Writer { buf: out, pos: 0 };
        w.put(&self.dst.0)?;
        w.put(&self.src.0)?;
        if let Some(tag) = self.vlan {
            w.put(&ETHERTYPE_VLAN.to_be_bytes())?;
            w.put(&tag.tci().to_be_bytes())?;
        }
        w.put(&self.ethertype.to_be_bytes())?;
        Some(w.pos)
    }

    /// Header followed by `payload` into `out`; returns the frame length.
    pub fn encode_frame(&self, payload: &[u8], out: &mut [u8]) -> Option<usize> {
        let n = self.encode(out)?;
        let mut w = Writer { buf: out, pos: n };
        w.put(payload)?;
        Some(w.pos)
    }
}

/// A parsed frame borrowing its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EthFrame<'a> {
    pub hdr: EthHeader,
    pub vlan_source: VlanSource,
    /// Bytes after the (inner) EtherType.
    pub payload: &'a [u8],
}

impl<'a> EthFrame<'a> {
    /// Parse a frame whose tag, if any, is in the frame bytes.
    pub fn parse(frame: &'a [u8]) -> Result<Self, DropReason> {
        Self::parse_with_desc_vlan(frame, None)
    }

    /// Parse a frame, using `desc_tci` (the descriptor's VLAN field, when the
    /// NIC reported a stripped tag) only if the frame carries no tag itself.
    ///
    /// Errors: shorter than a header → `TooShort`; a second tag inside the
    /// first (`0x8100`/`0x88A8` as the inner EtherType) → `Malformed`.
    pub fn parse_with_desc_vlan(
        frame: &'a [u8],
        desc_tci: Option<u16>,
    ) -> Result<Self, DropReason> {
        let (dst, rest) = frame.split_first_chunk::<6>().ok_or(DropReason::TooShort)?;
        let (src, rest) = rest.split_first_chunk::<6>().ok_or(DropReason::TooShort)?;
        let (et, rest) = rest.split_first_chunk::<2>().ok_or(DropReason::TooShort)?;
        let et = u16::from_be_bytes(*et);
        let (vlan, vlan_source, ethertype, payload) = if et == ETHERTYPE_VLAN {
            let (tci, rest) = rest.split_first_chunk::<2>().ok_or(DropReason::TooShort)?;
            let (inner, payload) = rest.split_first_chunk::<2>().ok_or(DropReason::TooShort)?;
            let inner = u16::from_be_bytes(*inner);
            if inner == ETHERTYPE_VLAN || inner == ETHERTYPE_QINQ {
                return Err(DropReason::Malformed);
            }
            (
                Some(VlanTag::from_tci(u16::from_be_bytes(*tci))),
                VlanSource::InFrame,
                inner,
                payload,
            )
        } else if let Some(tci) = desc_tci {
            (
                Some(VlanTag::from_tci(tci)),
                VlanSource::Descriptor,
                et,
                rest,
            )
        } else {
            (None, VlanSource::None, et, rest)
        };
        Ok(Self {
            hdr: EthHeader {
                dst: MacAddr(*dst),
                src: MacAddr(*src),
                vlan,
                ethertype,
            },
            vlan_source,
            payload,
        })
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    const DST: [u8; 6] = [0x91, 0xE0, 0xF0, 0x00, 0x12, 0x34];
    const SRC: [u8; 6] = [0x00, 0x0a, 0x92, 0x01, 0x02, 0x03];

    fn frame(tag: Option<u16>, et: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&DST);
        v.extend_from_slice(&SRC);
        if let Some(tci) = tag {
            v.extend_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
            v.extend_from_slice(&tci.to_be_bytes());
        }
        v.extend_from_slice(&et.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn parse_untagged() {
        let f = frame(None, 0x22F0, &[0xFA, 1, 2]);
        let p = EthFrame::parse(&f).unwrap();
        assert_eq!(p.hdr.dst, MacAddr(DST));
        assert_eq!(p.hdr.src, MacAddr(SRC));
        assert_eq!(p.hdr.vlan, None);
        assert_eq!(p.vlan_source, VlanSource::None);
        assert_eq!(p.hdr.ethertype, 0x22F0);
        assert_eq!(p.payload, &[0xFA, 1, 2]);
    }

    #[test]
    fn parse_tagged() {
        // PCP 3, DEI 0, VID 2 (class A defaults).
        let f = frame(Some(0x6002), 0x22F0, &[0x02, 0xAA]);
        let p = EthFrame::parse(&f).unwrap();
        assert_eq!(
            p.hdr.vlan,
            Some(VlanTag {
                pcp: 3,
                dei: false,
                vid: 2
            })
        );
        assert_eq!(p.vlan_source, VlanSource::InFrame);
        assert_eq!(p.hdr.ethertype, 0x22F0);
        assert_eq!(p.payload, &[0x02, 0xAA]);
        // DEI and VID 4095 decode.
        let f = frame(Some(0xFFFF), 0x0800, &[]);
        let p = EthFrame::parse(&f).unwrap();
        assert_eq!(
            p.hdr.vlan,
            Some(VlanTag {
                pcp: 7,
                dei: true,
                vid: 4095
            })
        );
        assert!(p.payload.is_empty());
    }

    #[test]
    fn parse_descriptor_tag_and_in_frame_wins() {
        let f = frame(None, 0x22F0, &[0x02]);
        let p = EthFrame::parse_with_desc_vlan(&f, Some(0x6002)).unwrap();
        assert_eq!(p.vlan_source, VlanSource::Descriptor);
        assert_eq!(p.hdr.vlan.unwrap().vid, 2);
        assert_eq!(p.payload, &[0x02]);
        let f = frame(Some(0x6005), 0x22F0, &[0x02]);
        let p = EthFrame::parse_with_desc_vlan(&f, Some(0x6002)).unwrap();
        assert_eq!(p.vlan_source, VlanSource::InFrame);
        assert_eq!(p.hdr.vlan.unwrap().vid, 5);
    }

    #[test]
    fn parse_rejects() {
        assert_eq!(EthFrame::parse(&[]), Err(DropReason::TooShort));
        assert_eq!(EthFrame::parse(&[0; 13]), Err(DropReason::TooShort));
        assert!(EthFrame::parse(&[0; 14]).is_ok());
        // Tagged but cut inside the tag or the inner EtherType.
        let f = frame(Some(0x6002), 0x22F0, &[]);
        assert_eq!(f.len(), 18);
        assert!(EthFrame::parse(&f).is_ok());
        assert_eq!(EthFrame::parse(&f[..17]), Err(DropReason::TooShort));
        assert_eq!(EthFrame::parse(&f[..15]), Err(DropReason::TooShort));
        // Double tags.
        let f = frame(Some(0x6002), ETHERTYPE_VLAN, &[0, 2, 0x22, 0xF0]);
        assert_eq!(EthFrame::parse(&f), Err(DropReason::Malformed));
        let f = frame(Some(0x6002), ETHERTYPE_QINQ, &[0, 2, 0x22, 0xF0]);
        assert_eq!(EthFrame::parse(&f), Err(DropReason::Malformed));
    }

    #[test]
    fn build_round_trips() {
        for vlan in [
            None,
            Some(VlanTag {
                pcp: 3,
                dei: false,
                vid: 2,
            }),
        ] {
            let hdr = EthHeader {
                dst: MacAddr(DST),
                src: MacAddr(SRC),
                vlan,
                ethertype: 0x22EA,
            };
            let mut buf = [0u8; 64];
            let n = hdr.encode_frame(&[9, 8, 7], &mut buf).unwrap();
            assert_eq!(n, hdr.len() + 3);
            assert_eq!(
                &buf[..n],
                frame(vlan.map(VlanTag::tci), 0x22EA, &[9, 8, 7]).as_slice()
            );
            let p = EthFrame::parse(&buf[..n]).unwrap();
            assert_eq!(p.hdr, hdr);
            assert_eq!(p.payload, &[9, 8, 7]);
            // Too-small output buffers.
            assert_eq!(hdr.encode(&mut buf[..hdr.len() - 1]), None);
            assert_eq!(hdr.encode_frame(&[1, 2], &mut buf[..hdr.len() + 1]), None);
        }
        assert_eq!(VlanTag::from_tci(0x6002).tci(), 0x6002);
    }
}
