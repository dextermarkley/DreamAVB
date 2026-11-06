//! What the daemon, acting as a controller, does with one of a device's
//! audio STREAM_INPUT sinks. Pure, so the policy is testable on its own.
//!
//! Milan v1.3 §5.5.1.2 / §5.5.1.4: a sink is BOUND by a controller and only
//! becomes unbound when a controller unbinds it; after the bind the Listener
//! owns re-establishing the stream. The device's own routing (the 16R's
//! stream selection page, UC Surface, another controller) is therefore the
//! authority, and we only ever fill a sink nobody bound.
//!
//! Two inputs decide it, both read from the device:
//! - GET_STREAM_INFO's CONNECTED flag (Table 5.8): bound or not. NOT the
//!   stream_id: that is non-zero only once the sink is SETTLED, so a sink
//!   bound to a talker that is currently offline reports CONNECTED with
//!   stream_id 0, and reading 0 as "empty" overwrote device-owned routes.
//! - GET_RX_STATE (ACMP): which talker a bound sink is bound to. Used for
//!   exactly one exception: a binding to one of THIS Mac's other entity ids
//!   (the other NIC, whose entity id derives from its MAC). Such a sink is
//!   bound to a talker that can no longer exist while we run on this NIC, so
//!   it is released and rebound to us. Any other talker is left alone.

/// A device's answer to GET_RX_STATE for a sink: the talker it is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxState {
    pub talker_entity: u64,
    pub talker_uid: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkAction {
    /// No GET_STREAM_INFO answer yet: do nothing. Binding before the device
    /// has said anything is how a blanket bind sneaks back in.
    Wait,
    /// Bound to someone: the device's choice. Leave it.
    Leave,
    /// Bound, but we do not yet know to whom: ask GET_RX_STATE first.
    AskRxState,
    /// Unbound: fill it with our default stream.
    Bind,
    /// Bound to one of this Mac's other entities: release that binding,
    /// then bind ours.
    ReleaseThenBind(RxState),
}

/// Decide for one sink.
///
/// `connected`: the CONNECTED bit from the last GET_STREAM_INFO answer, if
/// any. `rx`: the last GET_RX_STATE answer for a bound sink, if any.
/// `our_entity`: our entity id on THIS NIC. `own_entities`: every entity id
/// this Mac has used (one per NIC), from [`avb_config::own_entities`].
pub fn decide(
    connected: Option<bool>,
    rx: Option<RxState>,
    our_entity: u64,
    own_entities: &[u64],
) -> SinkAction {
    match connected {
        None => SinkAction::Wait,
        Some(false) => SinkAction::Bind,
        Some(true) => match rx {
            None => SinkAction::AskRxState,
            Some(r) if r.talker_entity != our_entity && own_entities.contains(&r.talker_entity) => {
                SinkAction::ReleaseThenBind(r)
            }
            Some(_) => SinkAction::Leave,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: u64 = 0x38c9_86ff_fe46_7e8b; // BCM entity
    const OTHER_NIC: u64 = 0x644b_f0ff_fe12_73b8; // I210 entity, same Mac
    const MIXER: u64 = 0x000a_92ff_fe00_713d; // a 24R

    #[test]
    fn no_answer_means_no_action() {
        assert_eq!(decide(None, None, US, &[US, OTHER_NIC]), SinkAction::Wait);
    }

    #[test]
    fn only_an_unbound_sink_is_filled() {
        assert_eq!(decide(Some(false), None, US, &[US]), SinkAction::Bind);
    }

    /// The bound-but-unsettled case: CONNECTED with stream_id 0. Must be
    /// left alone, which the stream_id test got wrong.
    #[test]
    fn a_bound_sink_is_asked_about_then_left_alone() {
        assert_eq!(decide(Some(true), None, US, &[US]), SinkAction::AskRxState);
        let to_mixer = RxState {
            talker_entity: MIXER,
            talker_uid: 2,
        };
        assert_eq!(
            decide(Some(true), Some(to_mixer), US, &[US, OTHER_NIC]),
            SinkAction::Leave
        );
        let to_us = RxState {
            talker_entity: US,
            talker_uid: 5,
        };
        assert_eq!(
            decide(Some(true), Some(to_us), US, &[US, OTHER_NIC]),
            SinkAction::Leave
        );
    }

    /// After a NIC switch the device is still bound to the other NIC's
    /// entity: release it and bind ours. Only for an entity this Mac used.
    #[test]
    fn a_binding_to_our_other_nic_is_moved() {
        let to_old = RxState {
            talker_entity: OTHER_NIC,
            talker_uid: 3,
        };
        assert_eq!(
            decide(Some(true), Some(to_old), US, &[US, OTHER_NIC]),
            SinkAction::ReleaseThenBind(to_old)
        );
        // Unknown to us -> someone else's talker, even if it is offline.
        assert_eq!(
            decide(Some(true), Some(to_old), US, &[US]),
            SinkAction::Leave
        );
    }
}
