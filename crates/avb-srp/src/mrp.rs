// SPDX-License-Identifier: MIT OR Apache-2.0

//! MRP state machines (IEEE 802.1Q-2018 §10.7), hand-written from the
//! standard's tables:
//! - applicant: Table 10-3 (§10.7.7), 12 states VO..LO;
//! - registrar: Table 10-4 (§10.7.8), IN / LV / MT;
//! - LeaveAll: Table 10-5 (§10.7.9), Active / Passive.
//!
//! Every cell is pinned by a unit test below. We are a full participant
//! (applicant + registrar) on a point-to-point link: `operPointToPointMAC`
//! is TRUE (design §8.1), which selects the point-to-point variants in the
//! table notes:
//! - note 4: rJoinIn! in VO / VP is ignored (no transition);
//! - note 5: rIn! in AA moves to QA.
//!
//! Note added by IEEE 802.1 maintenance item 0044 (applied to the VO, AO and
//! QO columns *(verify the note numbers in the -2018 text)*): the rLA! and
//! txLA!/txLAF! transitions to LO are ignored when the attribute's
//! registrar is MT, so a LeaveAll does not make us send Mt for every value
//! we have state for.
//!
//! The periodic transmission timer is disabled (design §8.1; 802.1Q-2018
//! §10.7.4.2 / Table 10-7 *(verify)*), so periodic! never occurs; its row is
//! still implemented and tested.

use serde::Serialize;

/// JoinTime (802.1Q-2018 Table 10-7: 200 ms).
pub const JOIN_TIME_NS: u64 = 200_000_000;
/// LeaveTime (design §8.1: 1000 ms).
pub const LEAVE_TIME_NS: u64 = 1_000_000_000;
/// LeaveAllTime is randomized in [10, 15] s (802.1Q-2018 §10.7.4.3: a value
/// in LeaveAllTime .. 1.5 × LeaveAllTime with LeaveAllTime = 10 s).
pub const LEAVE_ALL_MIN_NS: u64 = 10_000_000_000;
pub const LEAVE_ALL_MAX_NS: u64 = 15_000_000_000;
/// Point-to-point rate limit: at most 3 transmit opportunities per
/// 1.5 × JoinTime (802.1Q-2018 §10.7.4.1 *(verify)*).
pub const TX_BURST: usize = 3;
pub const TX_BURST_WINDOW_NS: u64 = JOIN_TIME_NS * 3 / 2;

/// Applicant states (802.1Q-2018 §10.7.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum Applicant {
    /// Very anxious Observer.
    VO,
    /// Very anxious Passive.
    VP,
    /// Very anxious New.
    VN,
    /// Anxious New.
    AN,
    /// Anxious Active.
    AA,
    /// Quiet Active.
    QA,
    /// Leaving Active.
    LA,
    /// Anxious Observer.
    AO,
    /// Quiet Observer.
    QO,
    /// Anxious Passive.
    AP,
    /// Quiet Passive.
    QP,
    /// Leaving Observer.
    LO,
}

impl Applicant {
    pub const ALL: [Applicant; 12] = [
        Self::VO,
        Self::VP,
        Self::VN,
        Self::AN,
        Self::AA,
        Self::QA,
        Self::LA,
        Self::AO,
        Self::QO,
        Self::AP,
        Self::QP,
        Self::LO,
    ];

    /// A state whose tx! entry is a mandatory send (sJ, sN, sL, s): a
    /// transmit opportunity must be requested.
    pub const fn wants_tx(self) -> bool {
        matches!(
            self,
            Self::VP | Self::VN | Self::AN | Self::AA | Self::LA | Self::LO
        )
    }

    /// The participant is declaring the attribute in this state.
    pub const fn declaring(self) -> bool {
        matches!(
            self,
            Self::VP | Self::VN | Self::AN | Self::AA | Self::QA | Self::AP | Self::QP
        )
    }
}

/// Applicant events (802.1Q-2018 §10.7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppEvent {
    Begin,
    New,
    Join,
    Lv,
    RNew,
    RJoinIn,
    RIn,
    RJoinMt,
    RMt,
    RLv,
    RLa,
    ReDeclare,
    Periodic,
    Tx,
    TxLa,
    TxLaf,
}

/// What a transmit-opportunity transition sends (802.1Q-2018 §10.7.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendAction {
    None,
    /// sN: New.
    New,
    /// sJ: JoinIn if the registrar is IN, else JoinMt.
    Join,
    /// sL: Lv.
    Leave,
    /// s: In if the registrar is IN, else Mt (Empty).
    InOrMt,
    /// [s] / [sJ]: optional, only to complete a packed vector. With one value
    /// per vector (design §8.1) we never need it, so nothing is sent.
    Optional,
}

/// Table 10-3. `reg_mt`: the attribute's registrar is MT (maintenance item
/// 0044 note). Returns the next state and, for tx!/txLA!/txLAF!, the send.
pub fn applicant(s: Applicant, e: AppEvent, reg_mt: bool) -> (Applicant, SendAction) {
    use AppEvent as E;
    use Applicant::*;
    let n = SendAction::None;
    let next = match e {
        E::Begin => VO,
        E::New => match s {
            VN | AN => s,
            _ => VN,
        },
        E::Join => match s {
            VO | LO => VP,
            LA => AA,
            AO => AP,
            QO => QP,
            _ => s,
        },
        E::Lv => match s {
            VP => VO,
            VN | AN | AA | QA => LA,
            AP => AO,
            QP => QO,
            _ => s,
        },
        E::RNew => s,
        E::RJoinIn => match s {
            // Note 4: VO→AO and VP→AP are ignored with operPointToPointMAC.
            AA => QA,
            AO => QO,
            AP => QP,
            _ => s,
        },
        // Note 5: applies because operPointToPointMAC is TRUE.
        E::RIn => match s {
            AA => QA,
            _ => s,
        },
        E::RJoinMt | E::RMt => match s {
            QA => AA,
            QO => AO,
            QP => AP,
            LO => VO,
            _ => s,
        },
        E::RLv | E::RLa | E::ReDeclare => match s {
            VO | AO | QO if e == E::RLa && reg_mt => s,
            VO | AO | QO => LO,
            AN => VN,
            AA | QA | AP | QP => VP,
            _ => s,
        },
        E::Periodic => match s {
            QA => AA,
            QP => AP,
            _ => s,
        },
        E::Tx => {
            return match s {
                VO | AO | QO => (s, SendAction::Optional),
                VP => (AA, SendAction::Join),
                VN => (AN, SendAction::New),
                AN => (QA, SendAction::New), // note 8 *(verify)*
                AA => (QA, SendAction::Join),
                QA => (QA, SendAction::Optional),
                LA => (VO, SendAction::Leave),
                AP => (QA, SendAction::Join),
                QP => (QP, SendAction::Optional),
                LO => (VO, SendAction::InOrMt),
            };
        }
        E::TxLa => {
            return match s {
                VO | AO | QO if reg_mt => (s, SendAction::Optional),
                VO | LA | AO | QO => (LO, SendAction::Optional),
                VP => (AA, SendAction::InOrMt),
                VN => (AN, SendAction::New),
                AN => (QA, SendAction::New),
                AA | AP | QP => (QA, SendAction::Join),
                QA => (QA, SendAction::Join),
                LO => (LO, SendAction::Optional),
            }
        }
        E::TxLaf => match s {
            VO | AO | QO if reg_mt => s,
            VO | LA | AO | QO => LO,
            AN => VN,
            AA | QA | AP | QP => VP,
            _ => s,
        },
    };
    (next, n)
}

/// Registrar states (802.1Q-2018 §10.7.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum Registrar {
    IN,
    LV,
    MT,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegEvent {
    Begin,
    RNew,
    RJoinIn,
    RJoinMt,
    RLv,
    RLa,
    TxLa,
    ReDeclare,
    Flush,
    LeaveTimer,
}

/// Indication to the MRP application and leave-timer action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RegAction {
    /// MAD_Join.indication with new = true.
    pub new: bool,
    /// MAD_Join.indication.
    pub join: bool,
    /// MAD_Leave.indication.
    pub leave: bool,
    pub start_leave_timer: bool,
    pub stop_leave_timer: bool,
}

/// Table 10-4.
pub fn registrar(s: Registrar, e: RegEvent) -> (Registrar, RegAction) {
    use RegEvent as E;
    use Registrar::*;
    let a = RegAction::default();
    match (e, s) {
        (E::Begin, _) => (MT, a),
        (E::RNew, IN) | (E::RNew, MT) => (IN, RegAction { new: true, ..a }),
        (E::RNew, LV) => (
            IN,
            RegAction {
                new: true,
                stop_leave_timer: true,
                ..a
            },
        ),
        (E::RJoinIn | E::RJoinMt, IN) => (IN, a),
        (E::RJoinIn | E::RJoinMt, LV) => (
            IN,
            RegAction {
                stop_leave_timer: true,
                ..a
            },
        ),
        (E::RJoinIn | E::RJoinMt, MT) => (IN, RegAction { join: true, ..a }),
        (E::RLv | E::RLa | E::TxLa | E::ReDeclare, IN) => (
            LV,
            RegAction {
                start_leave_timer: true,
                ..a
            },
        ),
        (E::RLv | E::RLa | E::TxLa | E::ReDeclare, LV | MT) => (s, a),
        (E::Flush, IN | MT) => (MT, a),
        (E::Flush, LV) => (MT, RegAction { leave: true, ..a }),
        (E::LeaveTimer, IN) => (IN, a),
        (E::LeaveTimer, LV) => (MT, RegAction { leave: true, ..a }),
        (E::LeaveTimer, MT) => (MT, a),
    }
}

/// LeaveAll state machine (Table 10-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum LeaveAllState {
    Active,
    Passive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaEvent {
    Begin,
    Tx,
    RLa,
    LeaveAllTimer,
}

/// Returns (next, send LeaveAll, restart leavealltimer).
pub fn leave_all(s: LeaveAllState, e: LaEvent) -> (LeaveAllState, bool, bool) {
    use LeaveAllState::*;
    match (e, s) {
        (LaEvent::Begin, _) => (Passive, false, true),
        (LaEvent::Tx, Active) => (Passive, true, false),
        (LaEvent::Tx, Passive) => (Passive, false, false),
        (LaEvent::RLa, _) => (Passive, false, true),
        (LaEvent::LeaveAllTimer, _) => (Active, false, true),
    }
}

/// Deterministic xorshift64* for the LeaveAllTime randomization (the seed is
/// injected; no clock or OS entropy is read).
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// LeaveAllTime in [10, 15] s, ns.
    pub fn leave_all_ns(&mut self) -> u64 {
        LEAVE_ALL_MIN_NS + self.next_u64() % (LEAVE_ALL_MAX_NS - LEAVE_ALL_MIN_NS + 1)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::Applicant::*;
    use super::*;
    use AppEvent as E;

    /// Table 10-3, one row per event, columns VO VP VN AN AA QA LA AO QO AP
    /// QP LO (802.1Q-2018, operPointToPointMAC TRUE, registrar not MT).
    #[test]
    fn applicant_table_state_rows() {
        #[rustfmt::skip]
        let rows: [(AppEvent, [Applicant; 12]); 13] = [
            (E::Begin,     [VO, VO, VO, VO, VO, VO, VO, VO, VO, VO, VO, VO]),
            (E::New,       [VN, VN, VN, AN, VN, VN, VN, VN, VN, VN, VN, VN]),
            (E::Join,      [VP, VP, VN, AN, AA, QA, AA, AP, QP, AP, QP, VP]),
            (E::Lv,        [VO, VO, LA, LA, LA, LA, LA, AO, QO, AO, QO, LO]),
            (E::RNew,      [VO, VP, VN, AN, AA, QA, LA, AO, QO, AP, QP, LO]),
            (E::RJoinIn,   [VO, VP, VN, AN, QA, QA, LA, QO, QO, QP, QP, LO]),
            (E::RIn,       [VO, VP, VN, AN, QA, QA, LA, AO, QO, AP, QP, LO]),
            (E::RJoinMt,   [VO, VP, VN, AN, AA, AA, LA, AO, AO, AP, AP, VO]),
            (E::RMt,       [VO, VP, VN, AN, AA, AA, LA, AO, AO, AP, AP, VO]),
            (E::RLv,       [LO, VP, VN, VN, VP, VP, LA, LO, LO, VP, VP, LO]),
            (E::RLa,       [LO, VP, VN, VN, VP, VP, LA, LO, LO, VP, VP, LO]),
            (E::ReDeclare, [LO, VP, VN, VN, VP, VP, LA, LO, LO, VP, VP, LO]),
            (E::Periodic,  [VO, VP, VN, AN, AA, AA, LA, AO, QO, AP, AP, LO]),
        ];
        for (e, want) in rows {
            for (s, w) in Applicant::ALL.iter().zip(want) {
                assert_eq!(
                    applicant(*s, e, false),
                    (w, SendAction::None),
                    "{s:?} {e:?}"
                );
            }
        }
    }

    #[test]
    fn applicant_table_tx_rows() {
        use SendAction::*;
        #[rustfmt::skip]
        let tx: [(Applicant, SendAction); 12] = [
            (VO, Optional), (AA, Join), (AN, New), (QA, New), (QA, Join), (QA, Optional),
            (VO, Leave), (AO, Optional), (QO, Optional), (QA, Join), (QP, Optional), (VO, InOrMt),
        ];
        #[rustfmt::skip]
        let txla: [(Applicant, SendAction); 12] = [
            (LO, Optional), (AA, InOrMt), (AN, New), (QA, New), (QA, Join), (QA, Join),
            (LO, Optional), (LO, Optional), (LO, Optional), (QA, Join), (QA, Join), (LO, Optional),
        ];
        #[rustfmt::skip]
        let txlaf: [Applicant; 12] = [LO, VP, VN, VN, VP, VP, LO, LO, LO, VP, VP, LO];
        for (i, s) in Applicant::ALL.iter().enumerate() {
            assert_eq!(applicant(*s, E::Tx, false), tx[i], "tx! {s:?}");
            assert_eq!(applicant(*s, E::TxLa, false), txla[i], "txLA! {s:?}");
            assert_eq!(
                applicant(*s, E::TxLaf, false),
                (txlaf[i], SendAction::None),
                "txLAF! {s:?}"
            );
            // Registrar state only matters for the 0044 note cells.
            assert_eq!(applicant(*s, E::Tx, true), tx[i]);
        }
    }

    /// Maintenance item 0044: VO/AO/QO ignore rLA!, txLA!, txLAF! when the
    /// registrar is MT; rLv!/Re-declare! are unaffected.
    #[test]
    fn applicant_registrar_mt_note() {
        for s in [VO, AO, QO] {
            assert_eq!(applicant(s, E::RLa, true), (s, SendAction::None));
            assert_eq!(applicant(s, E::TxLa, true), (s, SendAction::Optional));
            assert_eq!(applicant(s, E::TxLaf, true), (s, SendAction::None));
            assert_eq!(applicant(s, E::RLv, true), (LO, SendAction::None));
            assert_eq!(applicant(s, E::ReDeclare, true), (LO, SendAction::None));
        }
        // Other columns ignore the registrar state.
        for s in [VP, VN, AN, AA, QA, LA, AP, QP, LO] {
            for e in [E::RLa, E::TxLa, E::TxLaf] {
                assert_eq!(applicant(s, e, true), applicant(s, e, false), "{s:?} {e:?}");
            }
        }
    }

    #[test]
    fn wants_tx_and_declaring() {
        let wants: Vec<_> = Applicant::ALL
            .into_iter()
            .filter(|s| s.wants_tx())
            .collect();
        assert_eq!(wants, [VP, VN, AN, AA, LA, LO]);
        let decl: Vec<_> = Applicant::ALL
            .into_iter()
            .filter(|s| s.declaring())
            .collect();
        assert_eq!(decl, [VP, VN, AN, AA, QA, AP, QP]);
    }

    /// A full declaration on a point-to-point link: Join! → VP, two
    /// transmissions (JoinMt, then JoinIn once registered) unless the peer's
    /// In arrives first; Lv! → LA → sL → VO.
    #[test]
    fn applicant_join_leave_sequence() {
        let (s, _) = applicant(VO, E::Join, true);
        assert_eq!(s, VP);
        let (s, send) = applicant(s, E::Tx, true);
        assert_eq!((s, send), (AA, SendAction::Join));
        let (s, _) = applicant(s, E::RIn, false);
        assert_eq!(s, QA);
        let (s, _) = applicant(s, E::Lv, false);
        assert_eq!(s, LA);
        assert_eq!(applicant(s, E::Tx, false), (VO, SendAction::Leave));
    }

    #[test]
    fn registrar_table() {
        use RegEvent as R;
        use Registrar::*;
        let none = RegAction::default();
        let new = RegAction { new: true, ..none };
        let join = RegAction { join: true, ..none };
        let leave = RegAction {
            leave: true,
            ..none
        };
        let start = RegAction {
            start_leave_timer: true,
            ..none
        };
        let stop = RegAction {
            stop_leave_timer: true,
            ..none
        };
        let new_stop = RegAction {
            new: true,
            stop_leave_timer: true,
            ..none
        };
        #[rustfmt::skip]
        let rows: [(RegEvent, [(Registrar, RegAction); 3]); 10] = [
            (R::Begin,      [(MT, none), (MT, none), (MT, none)]),
            (R::RNew,       [(IN, new), (IN, new_stop), (IN, new)]),
            (R::RJoinIn,    [(IN, none), (IN, stop), (IN, join)]),
            (R::RJoinMt,    [(IN, none), (IN, stop), (IN, join)]),
            (R::RLv,        [(LV, start), (LV, none), (MT, none)]),
            (R::RLa,        [(LV, start), (LV, none), (MT, none)]),
            (R::TxLa,       [(LV, start), (LV, none), (MT, none)]),
            (R::ReDeclare,  [(LV, start), (LV, none), (MT, none)]),
            (R::Flush,      [(MT, none), (MT, leave), (MT, none)]),
            (R::LeaveTimer, [(IN, none), (MT, leave), (MT, none)]),
        ];
        for (e, want) in rows {
            for (s, w) in [IN, LV, MT].into_iter().zip(want) {
                assert_eq!(registrar(s, e), w, "{s:?} {e:?}");
            }
        }
    }

    #[test]
    fn leave_all_table() {
        use LeaveAllState::*;
        assert_eq!(leave_all(Active, LaEvent::Begin), (Passive, false, true));
        assert_eq!(leave_all(Passive, LaEvent::Begin), (Passive, false, true));
        assert_eq!(leave_all(Active, LaEvent::Tx), (Passive, true, false));
        assert_eq!(leave_all(Passive, LaEvent::Tx), (Passive, false, false));
        assert_eq!(leave_all(Active, LaEvent::RLa), (Passive, false, true));
        assert_eq!(leave_all(Passive, LaEvent::RLa), (Passive, false, true));
        assert_eq!(
            leave_all(Active, LaEvent::LeaveAllTimer),
            (Active, false, true)
        );
        assert_eq!(
            leave_all(Passive, LaEvent::LeaveAllTimer),
            (Active, false, true)
        );
    }

    #[test]
    fn leave_all_time_randomized_in_range() {
        let mut r = Rng::new(42);
        let v: Vec<u64> = (0..1000).map(|_| r.leave_all_ns()).collect();
        assert!(v
            .iter()
            .all(|&x| (LEAVE_ALL_MIN_NS..=LEAVE_ALL_MAX_NS).contains(&x)));
        assert!(v.iter().any(|&x| x < 11_000_000_000) && v.iter().any(|&x| x > 14_000_000_000));
        // Same seed, same sequence.
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert_eq!(a.leave_all_ns(), b.leave_all_ns());
    }
}
