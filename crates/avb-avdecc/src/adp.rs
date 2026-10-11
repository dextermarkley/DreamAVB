//! ADP: AVDECC Discovery Protocol (IEEE 1722.1-2021 §6; design §9.2, §9.3).
//!
//! The ADPDU is a 68-byte AVTP control PDU: the AVTP control header
//! (`cd=1 | subtype=0xFA | sv=0 | version=0 | message_type(4) |
//! valid_time(5) | control_data_length(11)`), `entity_id(64)`, then the
//! common entity fields (§6.2.1).

use avb_core::{Eui64, MacAddr, StreamId};

use crate::constants::{adp_msg, subtype};
use crate::wire::{Rd, Wr};

/// ADPDU wire length (IEEE 1722.1-2021 §6.2.1): control header (12) + 56.
pub const ADPDU_LEN: usize = 68;
/// `control_data_length` of an ADPDU (everything after the first 12 bytes).
pub const ADP_CONTROL_DATA_LENGTH: u16 = 56;

/// A decoded ADPDU (IEEE 1722.1-2021 §6.2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Adpdu {
    pub message_type: u8,
    /// Advertisement validity in 2-second units (expiry = `valid_time*2 s`).
    pub valid_time: u8,
    pub entity_id: Eui64,
    pub entity_model_id: u64,
    pub entity_capabilities: u32,
    pub talker_stream_sources: u16,
    pub talker_capabilities: u16,
    pub listener_stream_sinks: u16,
    pub listener_capabilities: u16,
    pub controller_capabilities: u32,
    pub available_index: u32,
    pub gptp_grandmaster_id: u64,
    pub gptp_domain_number: u8,
    pub identify_control_index: u16,
    pub interface_index: u16,
    pub association_id: u64,
}

impl Adpdu {
    /// Decode the AVTP payload (after the EtherType). Returns `None` on a
    /// short buffer or a PDU that is not an ADPDU.
    pub fn decode(pdu: &[u8]) -> Option<Self> {
        let mut r = Rd::new(pdu);
        let st = r.u8()?; // byte0 = subtype
        let b1 = r.u8()?; // byte1 = sv(1)|version(3)|message_type(4)
        if st != subtype::ADP {
            return None;
        }
        let b2 = r.u8()?;
        let message_type = b1 & 0x0F;
        let valid_time = (b2 >> 3) & 0x1F;
        let _version = (b1 >> 4) & 0x07;
        r.skip(1)?; // low byte of control_data_length
        let entity_id = Eui64::from_u64(r.u64()?);
        let entity_model_id = r.u64()?;
        let entity_capabilities = r.u32()?;
        let talker_stream_sources = r.u16()?;
        let talker_capabilities = r.u16()?;
        let listener_stream_sinks = r.u16()?;
        let listener_capabilities = r.u16()?;
        let controller_capabilities = r.u32()?;
        let available_index = r.u32()?;
        let gptp_grandmaster_id = r.u64()?;
        let gptp_domain_number = r.u8()?;
        r.skip(3)?; // reserved
        let identify_control_index = r.u16()?;
        let interface_index = r.u16()?;
        let association_id = r.u64()?;
        Some(Self {
            message_type,
            valid_time,
            entity_id,
            entity_model_id,
            entity_capabilities,
            talker_stream_sources,
            talker_capabilities,
            listener_stream_sinks,
            listener_capabilities,
            controller_capabilities,
            available_index,
            gptp_grandmaster_id,
            gptp_domain_number,
            identify_control_index,
            interface_index,
            association_id,
        })
    }

    /// Encode to the AVTP payload (goes to `91:E0:F0:01:00:00`).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Wr::new();
        // IEEE 1722 common control header: byte0 = subtype; byte1 =
        // sv(1)|version(3)|message_type(4). (ADP has no cd bit; the subtype
        // identifies the control PDU.)
        w.u8(subtype::ADP);
        w.u8(adp_msg_nibble(self.message_type));
        // valid_time(5) | control_data_length high (3).
        w.u8((self.valid_time & 0x1F) << 3);
        w.u8(ADP_CONTROL_DATA_LENGTH as u8);
        w.u64(self.entity_id.to_u64());
        w.u64(self.entity_model_id);
        w.u32(self.entity_capabilities);
        w.u16(self.talker_stream_sources);
        w.u16(self.talker_capabilities);
        w.u16(self.listener_stream_sinks);
        w.u16(self.listener_capabilities);
        w.u32(self.controller_capabilities);
        w.u32(self.available_index);
        w.u64(self.gptp_grandmaster_id);
        w.u8(self.gptp_domain_number);
        w.zeros(3);
        w.u16(self.identify_control_index);
        w.u16(self.interface_index);
        w.u64(self.association_id);
        w.zeros(4); // reserved(32) (IEEE 1722.1-2021 §6.2.1)
        debug_assert_eq!(w.len(), ADPDU_LEN);
        w.into_vec()
    }

    pub fn is_available(&self) -> bool {
        self.message_type == adp_msg::ENTITY_AVAILABLE
    }

    pub fn is_departing(&self) -> bool {
        self.message_type == adp_msg::ENTITY_DEPARTING
    }

    pub fn is_discover(&self) -> bool {
        self.message_type == adp_msg::ENTITY_DISCOVER
    }
}

fn adp_msg_nibble(mt: u8) -> u8 {
    mt & 0x0F
}

/// A tracked remote entity (design §9.2): the last ADPDU, the source MAC, and
/// the bookkeeping for expiry and reboot detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntityEntry {
    pub adp: Adpdu,
    pub mac: MacAddr,
    /// Monotonic time (ns) the entry was last refreshed.
    pub last_seen_ns: u64,
    /// Set when the last update was our own looped-back advertisement.
    pub loopback: bool,
}

impl EntityEntry {
    /// Expiry instant in ns: `last_seen + valid_time*2 s` (§9.2).
    pub fn expires_at_ns(&self) -> u64 {
        self.last_seen_ns
            .saturating_add(u64::from(self.adp.valid_time) * 2 * 1_000_000_000)
    }

    pub fn is_expired(&self, now_ns: u64) -> bool {
        now_ns >= self.expires_at_ns()
    }
}

/// The result of folding one received ADPDU into the entity table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdpUpdate {
    /// A new entity appeared.
    New,
    /// An existing entity refreshed with no reboot.
    Refreshed,
    /// A reboot was detected (`available_index` went backwards, or
    /// AVAILABLE after DEPARTING). The entry is refreshed.
    Reboot,
    /// ENTITY_DEPARTING removed the entity.
    Departed,
    /// A DISCOVER message (not an advertisement); nothing tracked.
    Discover,
    /// Our own looped-back advertisement; recorded with `loopback=true`.
    Loopback,
}

/// The discovery table (§9.2): every remote entity by `entity_id`, with
/// expiry and reboot detection. Sans-IO: the caller supplies `now_ns` and
/// prunes with [`EntityTable::prune`].
#[derive(Clone, Debug, Default)]
pub struct EntityTable {
    /// Our own entity id, to flag loopback.
    our_id: Option<Eui64>,
    entries: Vec<EntityEntry>,
    pub reboots: u64,
}

impl EntityTable {
    pub fn new(our_id: Eui64) -> Self {
        Self {
            our_id: Some(our_id),
            entries: Vec::new(),
            reboots: 0,
        }
    }

    pub fn entries(&self) -> &[EntityEntry] {
        &self.entries
    }

    pub fn get(&self, id: Eui64) -> Option<&EntityEntry> {
        self.entries.iter().find(|e| e.adp.entity_id == id)
    }

    /// The talker's current `available_index`, if tracked (§9.4 BIND_RX
    /// initialization of `last_available_index`).
    pub fn available_index(&self, id: Eui64) -> Option<u32> {
        self.get(id).map(|e| e.adp.available_index)
    }

    /// Fold one received ADPDU into the table.
    pub fn observe(&mut self, adp: &Adpdu, mac: MacAddr, now_ns: u64, loopback: bool) -> AdpUpdate {
        if adp.is_discover() {
            return AdpUpdate::Discover;
        }
        let is_ours = self.our_id == Some(adp.entity_id);
        if adp.is_departing() {
            self.entries.retain(|e| e.adp.entity_id != adp.entity_id);
            return if is_ours {
                AdpUpdate::Loopback
            } else {
                AdpUpdate::Departed
            };
        }
        // ENTITY_AVAILABLE.
        let pos = self
            .entries
            .iter()
            .position(|e| e.adp.entity_id == adp.entity_id);
        let update = match pos {
            None => AdpUpdate::New,
            Some(i) => {
                let prev = match self.entries.get(i) {
                    Some(e) => e,
                    None => return AdpUpdate::New,
                };
                let rebooted =
                    adp.available_index < prev.adp.available_index || prev.adp.is_departing();
                if rebooted {
                    AdpUpdate::Reboot
                } else {
                    AdpUpdate::Refreshed
                }
            }
        };
        let entry = EntityEntry {
            adp: *adp,
            mac,
            last_seen_ns: now_ns,
            loopback,
        };
        match pos {
            Some(i) => {
                if let Some(slot) = self.entries.get_mut(i) {
                    *slot = entry;
                }
            }
            None => self.entries.push(entry),
        }
        if matches!(update, AdpUpdate::Reboot) {
            self.reboots += 1;
        }
        if is_ours {
            AdpUpdate::Loopback
        } else {
            update
        }
    }

    /// Remove expired entries; returns the ids removed.
    pub fn prune(&mut self, now_ns: u64) -> Vec<Eui64> {
        let mut removed = Vec::new();
        self.entries.retain(|e| {
            if e.is_expired(now_ns) {
                removed.push(e.adp.entity_id);
                false
            } else {
                true
            }
        });
        removed
    }

    /// Talker selection (§9.2): the single entity with
    /// `talker_stream_sources > 0`. `Ok(None)` means zero candidates;
    /// `Err(list)` means more than one (the daemon maps both to exit 6).
    pub fn sole_talker(&self) -> Result<Option<Eui64>, Vec<Eui64>> {
        let talkers: Vec<Eui64> = self
            .entries
            .iter()
            .filter(|e| e.adp.talker_stream_sources > 0 && !e.loopback)
            .map(|e| e.adp.entity_id)
            .collect();
        match talkers.len() {
            0 => Ok(None),
            1 => Ok(talkers.first().copied()),
            _ => Err(talkers),
        }
    }
}

/// The stream id a talker's STREAM_OUTPUT advertises: talker MAC + unique id.
pub fn stream_id_for(talker_mac: MacAddr, unique_id: u16) -> StreamId {
    StreamId::from_parts(talker_mac, unique_id)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::constants::{entity_caps, listener_caps};

    fn sample() -> Adpdu {
        Adpdu {
            message_type: adp_msg::ENTITY_AVAILABLE,
            valid_time: 10,
            entity_id: Eui64::from_u64(0x000A_9203_3BA9_0001),
            entity_model_id: 0x0241_5601_1234_5678,
            entity_capabilities: entity_caps::AEM_SUPPORTED | entity_caps::GPTP_SUPPORTED,
            talker_stream_sources: 8,
            talker_capabilities: 0,
            listener_stream_sinks: 0,
            listener_capabilities: listener_caps::IMPLEMENTED,
            controller_capabilities: 0,
            available_index: 42,
            gptp_grandmaster_id: 0x000A_9203_3BA9_0000,
            gptp_domain_number: 0,
            identify_control_index: 0,
            interface_index: 0,
            association_id: 0,
        }
    }

    #[test]
    fn adpdu_round_trip() {
        let a = sample();
        let bytes = a.encode();
        assert_eq!(bytes.len(), ADPDU_LEN);
        assert_eq!(bytes.first(), Some(&subtype::ADP)); // byte0 = subtype
        assert_eq!(bytes.get(1), Some(&0x00)); // byte1 = message_type (ENTITY_AVAILABLE)
        let d = Adpdu::decode(&bytes).unwrap();
        assert_eq!(d, a);
        assert!(d.is_available());
    }

    #[test]
    fn adpdu_rejects_non_adp() {
        let mut bytes = sample().encode();
        if let Some(b) = bytes.get_mut(0) {
            *b = 0xFB; // byte0 = subtype; AECP subtype
        }
        assert_eq!(Adpdu::decode(&bytes), None);
        assert_eq!(Adpdu::decode(&bytes[..10]), None);
    }

    #[test]
    fn decodes_real_16r_wire_order() {
        // Captured from a PreSonus StudioLive 16R: the IEEE 1722 common header
        // is byte0 = subtype (0xFA = ADP), byte1 = sv|version|message_type
        // (0x00 = ENTITY_AVAILABLE). Locks the on-wire byte order so the
        // encode/decode inversion that hardware exposed cannot regress.
        let mut pdu = vec![0u8; ADPDU_LEN];
        pdu[0] = subtype::ADP; // 0xFA
        pdu[1] = adp_msg::ENTITY_AVAILABLE; // 0x00
        pdu[2] = 0x50; // valid_time=10 (<<3)
        pdu[3] = ADP_CONTROL_DATA_LENGTH as u8;
        // entity_id = 00:0a:92:ff:fe:03:3b:a9 (PreSonus OUI 00:0a:92).
        pdu[4..12].copy_from_slice(&[0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9]);
        let d = Adpdu::decode(&pdu).expect("real 16R ADPDU must decode");
        assert!(d.is_available());
        assert_eq!(d.entity_id.to_u64(), 0x000a_92ff_fe03_3ba9);
    }

    #[test]
    fn table_tracks_expiry() {
        let our = Eui64::from_u64(0x644b_f0ff_fe12_73b8);
        let mut t = EntityTable::new(our);
        let a = sample();
        let mac = MacAddr([0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9]);
        assert_eq!(t.observe(&a, mac, 0, false), AdpUpdate::New);
        assert_eq!(
            t.observe(&a, mac, 1_000_000_000, false),
            AdpUpdate::Refreshed
        );
        // valid_time 10 -> expiry 20 s after last_seen (1 s).
        assert!(!t.get(a.entity_id).unwrap().is_expired(20_000_000_000));
        assert!(t.get(a.entity_id).unwrap().is_expired(21_000_000_000));
        assert_eq!(t.prune(21_000_000_000), vec![a.entity_id]);
        assert!(t.get(a.entity_id).is_none());
    }

    #[test]
    fn reboot_detection() {
        let our = Eui64::from_u64(0x644b_f0ff_fe12_73b8);
        let mut t = EntityTable::new(our);
        let mut a = sample();
        let mac = MacAddr([0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9]);
        t.observe(&a, mac, 0, false);
        a.available_index = 5; // went backwards from 42
        assert_eq!(t.observe(&a, mac, 1, false), AdpUpdate::Reboot);
        assert_eq!(t.reboots, 1);
        // DEPARTING then AVAILABLE is also a reboot.
        let mut dep = a;
        dep.message_type = adp_msg::ENTITY_DEPARTING;
        assert_eq!(t.observe(&dep, mac, 2, false), AdpUpdate::Departed);
    }

    #[test]
    fn loopback_flagged() {
        let our = Eui64::from_u64(0x644b_f0ff_fe12_73b8);
        let mut t = EntityTable::new(our);
        let mut a = sample();
        a.entity_id = our;
        let mac = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
        assert_eq!(t.observe(&a, mac, 0, true), AdpUpdate::Loopback);
    }

    #[test]
    fn sole_talker_selection() {
        let our = Eui64::from_u64(0x644b_f0ff_fe12_73b8);
        let mut t = EntityTable::new(our);
        let mac = MacAddr([0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9]);
        assert_eq!(t.sole_talker(), Ok(None));
        let a = sample();
        t.observe(&a, mac, 0, false);
        assert_eq!(t.sole_talker(), Ok(Some(a.entity_id)));
        let mut b = sample();
        b.entity_id = Eui64::from_u64(0xDEAD_BEEF_0000_0001);
        t.observe(&b, MacAddr([0, 0, 0, 0, 0, 1]), 0, false);
        assert_eq!(t.sole_talker(), Err(vec![a.entity_id, b.entity_id]));
    }
}
