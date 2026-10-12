// SPDX-License-Identifier: MIT OR Apache-2.0

//! Control state shared by both entity directions (design §9.3): the
//! LOCK_ENTITY holder with its auto-release deadline, and the registered
//! unsolicited-notification controllers.
//!
//! This exists so the 60 s lock release and the 16-controller cap are written
//! once. They were previously private to the listener entity; the talker
//! responder needs exactly the same rules, and two copies of a timeout drift.

use avb_core::{Eui64, MacAddr};

use crate::constants::aecp_status;

/// LOCK_ENTITY auto-release (§9.3: 60 s).
pub const LOCK_TIMEOUT_NS: u64 = 60_000_000_000;
/// Max registered unsolicited-notification controllers (§9.3: 16).
pub const MAX_UNSOL_CONTROLLERS: usize = 16;

/// A registered unsolicited-notification controller (§9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnsolController {
    pub entity_id: Eui64,
    pub mac: MacAddr,
}

/// The lock holder and the unsolicited-notification registrations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ControlState {
    /// Current controller holding the lock, with its deadline.
    lock: Option<(Eui64, u64)>,
    unsol: Vec<UnsolController>,
}

impl ControlState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop the lock if its deadline has passed. Call this before reading or
    /// writing lock state, so a controller that went away cannot hold the
    /// entity forever.
    pub fn expire_lock(&mut self, now_ns: u64) {
        if let Some((_, deadline)) = self.lock {
            if now_ns >= deadline {
                self.lock = None;
            }
        }
    }

    /// LOCK_ENTITY: take or refresh the lock. `ENTITY_LOCKED` when another
    /// controller holds it.
    pub fn lock(&mut self, controller: Eui64, now_ns: u64) -> u8 {
        match self.lock {
            Some((owner, _)) if owner != controller => aecp_status::ENTITY_LOCKED,
            _ => {
                self.lock = Some((controller, now_ns.saturating_add(LOCK_TIMEOUT_NS)));
                aecp_status::SUCCESS
            }
        }
    }

    /// Is the entity locked by someone other than `controller`?
    pub fn locked_by_other(&self, controller: Eui64) -> bool {
        matches!(self.lock, Some((owner, _)) if owner != controller)
    }

    /// Register (or refresh) an unsolicited-notification controller (§9.3:
    /// ≤ 16; NO_RESOURCES above that).
    pub fn register_unsol(&mut self, entity_id: Eui64, mac: MacAddr) -> u8 {
        if let Some(c) = self.unsol.iter_mut().find(|c| c.entity_id == entity_id) {
            c.mac = mac;
            return aecp_status::SUCCESS;
        }
        if self.unsol.len() >= MAX_UNSOL_CONTROLLERS {
            return aecp_status::NO_RESOURCES;
        }
        self.unsol.push(UnsolController { entity_id, mac });
        aecp_status::SUCCESS
    }

    pub fn deregister_unsol(&mut self, entity_id: Eui64) -> u8 {
        self.unsol.retain(|c| c.entity_id != entity_id);
        aecp_status::SUCCESS
    }

    /// The controllers to send unsolicited notifications to.
    pub fn unsol_controllers(&self) -> &[UnsolController] {
        &self.unsol
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C1: Eui64 = Eui64([1, 0, 0, 0, 0, 0, 0, 1]);
    const C2: Eui64 = Eui64([2, 0, 0, 0, 0, 0, 0, 2]);
    const MAC: MacAddr = MacAddr([0, 1, 2, 3, 4, 5]);

    #[test]
    fn lock_is_exclusive_and_refreshable() {
        let mut s = ControlState::new();
        assert_eq!(s.lock(C1, 0), aecp_status::SUCCESS);
        assert_eq!(s.lock(C2, 0), aecp_status::ENTITY_LOCKED);
        // The holder may refresh its own lock.
        assert_eq!(s.lock(C1, 1_000), aecp_status::SUCCESS);
        assert!(s.locked_by_other(C2));
        assert!(!s.locked_by_other(C1));
    }

    #[test]
    fn lock_auto_releases_after_the_timeout() {
        let mut s = ControlState::new();
        s.lock(C1, 0);
        // One ns before the deadline it is still held.
        s.expire_lock(LOCK_TIMEOUT_NS - 1);
        assert!(s.locked_by_other(C2));
        // At the deadline it is released, so another controller can take it.
        s.expire_lock(LOCK_TIMEOUT_NS);
        assert!(!s.locked_by_other(C2));
        assert_eq!(s.lock(C2, LOCK_TIMEOUT_NS), aecp_status::SUCCESS);
    }

    #[test]
    fn unsol_caps_at_sixteen_and_refresh_is_not_a_new_slot() {
        let mut s = ControlState::new();
        for i in 0..MAX_UNSOL_CONTROLLERS {
            let id = Eui64([0, 0, 0, 0, 0, 0, 0, i as u8]);
            assert_eq!(s.register_unsol(id, MAC), aecp_status::SUCCESS);
        }
        assert_eq!(s.unsol_controllers().len(), MAX_UNSOL_CONTROLLERS);
        // The 17th is refused, not silently dropped on top of another.
        assert_eq!(s.register_unsol(C1, MAC), aecp_status::NO_RESOURCES);
        // Re-registering an existing controller updates its MAC in place.
        let existing = Eui64([0, 0, 0, 0, 0, 0, 0, 0]);
        let other_mac = MacAddr([9, 9, 9, 9, 9, 9]);
        assert_eq!(s.register_unsol(existing, other_mac), aecp_status::SUCCESS);
        assert_eq!(s.unsol_controllers().len(), MAX_UNSOL_CONTROLLERS);
        assert_eq!(
            s.unsol_controllers().first().map(|c| c.mac),
            Some(other_mac)
        );
        // Freeing a slot lets a new one in.
        s.deregister_unsol(existing);
        assert_eq!(s.register_unsol(C1, MAC), aecp_status::SUCCESS);
    }
}
