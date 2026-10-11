//! Participant tests: declarations, registrations, timers, LeaveAll, Domain
//! adoption, TalkerChanged, TA_WAIT-style Listener declarations.
#![allow(clippy::indexing_slicing)]

use avb_core::{EthHeader, MacAddr, MachTicks, Now, Outbox, RxMeta, StreamId, Timebase, TxFrame};

use crate::attr::*;
use crate::mrp::{Applicant, LeaveAllState, Registrar};
use crate::participant::*;
use crate::pdu::{self, AttrEvent, PduBuilder};

const OUR_MAC: MacAddr = MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]);
const PEER_MAC: MacAddr = MacAddr([0x00, 0x0a, 0x92, 0x03, 0x3b, 0xa9]);
const MS: u64 = 1_000_000;
const S: u64 = 1_000_000_000;
const SID: StreamId = StreamId(0x000a_9203_3ba9_0000);

fn part() -> Participant {
    Participant::new(
        ParticipantConfig {
            mac: OUR_MAC,
            timebase: Timebase::IDENTITY,
            seed: 1,
        },
        MachTicks(0),
    )
}

fn now(t: u64) -> Now {
    Now::mach_only(MachTicks(t))
}

fn ta(vid: u16) -> TalkerAdvertise {
    TalkerAdvertise {
        stream_id: SID,
        dest_mac: MacAddr([0x91, 0xE0, 0xF0, 0x00, 0xFE, 0x00]),
        vlan_id: vid,
        max_frame_size: 224,
        max_interval_frames: 1,
        priority: 3,
        rank: true,
        accumulated_latency_ns: 125_000,
    }
}

fn peer_pdu(app: App, la: bool, items: &[(Attribute, AttrEvent)]) -> Vec<u8> {
    let mut b = PduBuilder::new(app, la);
    for (a, e) in items {
        assert!(b.push(a, *e, a.four_packed().unwrap_or(ListenerDecl::Ignore)));
    }
    let (dst, et) = match app {
        App::Msrp => (MSRP_DEST, ETHERTYPE_MSRP),
        App::Mvrp => (MVRP_DEST, ETHERTYPE_MVRP),
    };
    let hdr = EthHeader {
        dst,
        src: PEER_MAC,
        vlan: None,
        ethertype: et,
    };
    TxFrame::build(&hdr, &b.finish(), false, 0)
        .unwrap()
        .bytes()
        .to_vec()
}

fn rx(p: &mut Participant, t: u64, f: &[u8]) {
    p.handle_rx(now(t), f, RxMeta::default()).unwrap();
}

/// Poll at `t`; decode what was sent as (app, events).
fn poll(p: &mut Participant, t: u64) -> Vec<(App, pdu::Pdu)> {
    let mut out = Box::new(Outbox::<16>::new());
    p.poll(now(t), &mut out);
    let mut v = Vec::new();
    while let Some(f) = out.pop() {
        let b = f.bytes();
        assert_eq!(&b[6..12], &OUR_MAC.0);
        let app = match u16::from_be_bytes([b[12], b[13]]) {
            ETHERTYPE_MSRP => {
                assert_eq!(&b[..6], &MSRP_DEST.0);
                App::Msrp
            }
            ETHERTYPE_MVRP => {
                assert_eq!(&b[..6], &MVRP_DEST.0);
                App::Mvrp
            }
            x => panic!("ethertype {x:#x}"),
        };
        v.push((app, pdu::decode(app, &b[14..]).unwrap()));
    }
    v
}

/// Run polls every 10 ms over [from, to) and collect everything sent.
fn run(p: &mut Participant, from: u64, to: u64) -> Vec<(u64, App, pdu::Pdu)> {
    let mut v = Vec::new();
    let mut t = from;
    while t < to {
        for (a, d) in poll(p, t) {
            v.push((t, a, d));
        }
        t += 10 * MS;
    }
    v
}

fn events_of(sent: &[(u64, App, pdu::Pdu)], attr: Attribute) -> Vec<(u64, AttrEvent)> {
    sent.iter()
        .flat_map(|(t, _, d)| {
            d.events
                .iter()
                .filter(|e| e.attr.key() == attr.key())
                .map(move |e| (*t, e.event))
        })
        .collect()
}

#[test]
fn default_domain_declared_at_start() {
    let mut p = part();
    assert_eq!(p.domains_declared(), [DEFAULT_DOMAIN]);
    let sent = run(&mut p, 0, 600 * MS);
    let ev = events_of(&sent, Attribute::Domain(DEFAULT_DOMAIN));
    // VP → (sJ) AA → (sJ, one JoinTime later) QA: two JoinMt, then quiet.
    assert_eq!(ev, [(0, AttrEvent::JoinMt), (200 * MS, AttrEvent::JoinMt)]);
    assert_eq!(
        p.applicant_state(App::Msrp, AttrKey::Domain(DEFAULT_DOMAIN)),
        Some(Applicant::QA)
    );
    // The peer's In after our first Join quiets it after one send.
    let mut p = part();
    poll(&mut p, 0);
    let f = peer_pdu(
        App::Msrp,
        false,
        &[(Attribute::Domain(DEFAULT_DOMAIN), AttrEvent::In)],
    );
    rx(&mut p, 50 * MS, &f);
    assert_eq!(
        p.applicant_state(App::Msrp, AttrKey::Domain(DEFAULT_DOMAIN)),
        Some(Applicant::QA)
    );
    assert!(run(&mut p, 60 * MS, 600 * MS).is_empty());
}

#[test]
fn talker_advertise_registration_change_and_leave() {
    let mut p = part();
    poll(&mut p, 0);
    let a = Attribute::Talker(Talker::Advertise(ta(2)));
    rx(
        &mut p,
        10 * MS,
        &peer_pdu(App::Msrp, false, &[(a, AttrEvent::JoinMt)]),
    );
    assert_eq!(
        p.pop_event(),
        Some(SrpEvent::TalkerRegistered {
            stream_id: SID,
            talker: Talker::Advertise(ta(2))
        })
    );
    let r = p.talker(SID).unwrap();
    assert_eq!((r.first_seen, r.last_seen), (10 * MS, 10 * MS));
    assert_eq!(
        p.registrar_state(App::Msrp, AttrKey::Talker(SID)),
        Some(Registrar::IN)
    );
    // Same value again: no event, last_seen moves.
    rx(
        &mut p,
        20 * MS,
        &peer_pdu(App::Msrp, false, &[(a, AttrEvent::JoinIn)]),
    );
    assert_eq!(p.pop_event(), None);
    assert_eq!(p.talker(SID).unwrap().last_seen, 20 * MS);
    assert_eq!(p.talker(SID).unwrap().first_seen, 10 * MS);
    // VID changed: TalkerChanged with both values.
    let b = Attribute::Talker(Talker::Advertise(ta(3)));
    rx(
        &mut p,
        30 * MS,
        &peer_pdu(App::Msrp, false, &[(b, AttrEvent::JoinIn)]),
    );
    assert_eq!(
        p.pop_event(),
        Some(SrpEvent::TalkerChanged {
            stream_id: SID,
            old: Talker::Advertise(ta(2)),
            new: Talker::Advertise(ta(3))
        })
    );
    // Accumulated latency only.
    let mut t4 = ta(3);
    t4.accumulated_latency_ns = 200_000;
    rx(
        &mut p,
        35 * MS,
        &peer_pdu(
            App::Msrp,
            false,
            &[(Attribute::Talker(Talker::Advertise(t4)), AttrEvent::JoinIn)],
        ),
    );
    assert!(matches!(
        p.pop_event(),
        Some(SrpEvent::TalkerChanged { .. })
    ));
    assert_eq!(p.counters().talker_changed, 2);
    // Advertise → Failed without a Leave (§35.2.6): a registration event.
    let tf = Talker::Failed(TalkerFailed {
        advertise: t4,
        failure_bridge_id: 0x8000_0000_0000_0001,
        failure_code: 1,
    });
    rx(
        &mut p,
        40 * MS,
        &peer_pdu(
            App::Msrp,
            false,
            &[(Attribute::Talker(tf), AttrEvent::JoinIn)],
        ),
    );
    assert_eq!(
        p.pop_event(),
        Some(SrpEvent::TalkerRegistered {
            stream_id: SID,
            talker: tf
        })
    );
    assert!(is_a5_failure_code(
        p.talker(SID).unwrap().attr_talker().failure_code().unwrap()
    ));
    // A stale Lv for the Advertise type is ignored; the Failed Lv starts the
    // leave timer and the registration goes after LeaveTime.
    rx(
        &mut p,
        50 * MS,
        &peer_pdu(
            App::Msrp,
            false,
            &[(Attribute::Talker(Talker::Advertise(t4)), AttrEvent::Lv)],
        ),
    );
    assert_eq!(
        p.registrar_state(App::Msrp, AttrKey::Talker(SID)),
        Some(Registrar::IN)
    );
    rx(
        &mut p,
        60 * MS,
        &peer_pdu(App::Msrp, false, &[(Attribute::Talker(tf), AttrEvent::Lv)]),
    );
    assert_eq!(
        p.registrar_state(App::Msrp, AttrKey::Talker(SID)),
        Some(Registrar::LV)
    );
    run(&mut p, 70 * MS, 1050 * MS);
    assert!(p.talker(SID).is_some(), "still registered before LeaveTime");
    run(&mut p, 1060 * MS, 1080 * MS);
    assert_eq!(
        p.pop_event(),
        Some(SrpEvent::TalkerWithdrawn {
            stream_id: SID,
            last: tf
        })
    );
    assert!(p.talker(SID).is_none());
    // A Join while leaving keeps it (LV → IN, timer stopped).
    rx(
        &mut p,
        2 * S,
        &peer_pdu(App::Msrp, false, &[(a, AttrEvent::JoinIn)]),
    );
    rx(
        &mut p,
        2 * S + MS,
        &peer_pdu(App::Msrp, false, &[(a, AttrEvent::Lv)]),
    );
    rx(
        &mut p,
        2 * S + 500 * MS,
        &peer_pdu(App::Msrp, false, &[(a, AttrEvent::JoinIn)]),
    );
    run(&mut p, 2 * S + 510 * MS, 4 * S);
    assert!(p.talker(SID).is_some());
    let log: Vec<_> = p.event_log().map(|l| l.what).collect();
    assert!(
        log.contains(&"registered")
            && log.contains(&"talker_changed")
            && log.contains(&"deregistered")
    );
}

trait AttrTalker {
    fn attr_talker(&self) -> Talker;
}

impl AttrTalker for Registration {
    fn attr_talker(&self) -> Talker {
        match self.attr {
            Attribute::Talker(t) => t,
            _ => panic!(),
        }
    }
}

#[test]
fn domain_adoption_on_mismatch() {
    let mut p = part();
    run(&mut p, 0, 500 * MS);
    let theirs = Domain {
        class_id: 6,
        priority: 2,
        vid: 3,
    };
    rx(
        &mut p,
        500 * MS,
        &peer_pdu(
            App::Msrp,
            false,
            &[(Attribute::Domain(theirs), AttrEvent::JoinIn)],
        ),
    );
    assert_eq!(p.pop_event(), Some(SrpEvent::DomainRegistered(theirs)));
    assert_eq!(
        p.pop_event(),
        Some(SrpEvent::DomainMismatch {
            ours: DEFAULT_DOMAIN,
            theirs
        })
    );
    assert_eq!(p.domains_declared(), [theirs]);
    assert_eq!(p.counters().domain_mismatch, 1);
    let sent = run(&mut p, 510 * MS, 1000 * MS);
    let ours = events_of(&sent, Attribute::Domain(DEFAULT_DOMAIN));
    assert_eq!(ours.first().map(|e| e.1), Some(AttrEvent::Lv));
    let new = events_of(&sent, Attribute::Domain(theirs));
    // Registrar IN for their value: JoinIn.
    assert_eq!(new.first().map(|e| e.1), Some(AttrEvent::JoinIn));
    // A Domain for another class does not touch ours; the same values again
    // are not a mismatch.
    let other = Domain {
        class_id: 5,
        priority: 2,
        vid: 2,
    };
    rx(
        &mut p,
        S,
        &peer_pdu(
            App::Msrp,
            false,
            &[
                (Attribute::Domain(other), AttrEvent::JoinIn),
                (Attribute::Domain(theirs), AttrEvent::JoinIn),
            ],
        ),
    );
    assert_eq!(p.counters().domain_mismatch, 1);
    assert_eq!(p.domains_declared(), [theirs]);
}

#[test]
fn listener_declarations_ready_asking_failed_and_withdraw() {
    let mut p = part();
    run(&mut p, 0, 500 * MS);
    // TA_WAIT expiry with no talker registered: Asking Failed is declared
    // (no registration precondition, 802.1Q-2018 §35.2.4.3 / §35.2.6).
    assert!(p.talker(SID).is_none());
    let af = Attribute::Listener(Listener {
        stream_id: SID,
        decl: ListenerDecl::AskingFailed,
    });
    p.declare(MachTicks(500 * MS), af);
    let sent = run(&mut p, 500 * MS, 1000 * MS);
    let l: Vec<_> = sent
        .iter()
        .flat_map(|(_, _, d)| d.events.iter())
        .filter(|e| matches!(e.attr, Attribute::Listener(_)))
        .collect();
    assert_eq!(l.len(), 2);
    assert_eq!(l[0].attr, af, "four-packed Asking Failed");
    assert_eq!(l[0].event, AttrEvent::JoinMt);
    // The TA arrives: re-declare Ready (a value change → New).
    let ready = Attribute::Listener(Listener {
        stream_id: SID,
        decl: ListenerDecl::Ready,
    });
    p.declare(MachTicks(S), ready);
    let sent = run(&mut p, S, 1500 * MS);
    let ev = events_of(&sent, ready);
    assert_eq!(
        ev.iter().map(|e| e.1).collect::<Vec<_>>(),
        [AttrEvent::New, AttrEvent::New]
    );
    let decls: Vec<_> = sent
        .iter()
        .flat_map(|(_, _, d)| d.events.iter())
        .filter_map(|e| e.attr.four_packed())
        .collect();
    assert_eq!(decls, [ListenerDecl::Ready, ListenerDecl::Ready]);
    // Re-declaring the same value is a no-op.
    p.declare(MachTicks(1500 * MS), ready);
    assert!(run(&mut p, 1500 * MS, 1700 * MS).is_empty());
    // Withdraw: one Lv.
    p.withdraw(MachTicks(2 * S), App::Msrp, AttrKey::Listener(SID));
    let sent = run(&mut p, 2 * S, 2500 * MS);
    assert_eq!(
        events_of(&sent, ready)
            .iter()
            .map(|e| e.1)
            .collect::<Vec<_>>(),
        [AttrEvent::Lv]
    );
    assert_eq!(p.declared(App::Msrp, AttrKey::Listener(SID)), None);
}

#[test]
fn talker_advertise_can_be_declared_for_talker_readiness() {
    let mut p = part();
    let a = Attribute::Talker(Talker::Advertise(ta(2)));
    p.declare(MachTicks(0), a);
    let sent = run(&mut p, 0, 100 * MS);
    assert_eq!(events_of(&sent, a)[0].1, AttrEvent::JoinMt);
    assert!(sent[0].2.events.iter().any(|e| e.attr == a));
}

#[test]
fn declare_new_forces_new_event_on_first_declaration() {
    // declare() emits Join on a first declaration; declare_new() forces New!,
    // which a strict Milan talker requires to react and advertise its stream.
    let ready = Attribute::Listener(Listener {
        stream_id: SID,
        decl: ListenerDecl::Ready,
    });
    let mut p = part();
    p.declare_new(MachTicks(0), ready);
    let sent = run(&mut p, 0, 100 * MS);
    let ev = events_of(&sent, ready);
    assert!(!ev.is_empty(), "declare_new transmits");
    assert_eq!(ev[0].1, AttrEvent::New, "first declare_new is New!");
    // A plain declare() of the same key is a Join (not New).
    let mut p2 = part();
    p2.declare(MachTicks(0), ready);
    let sent2 = run(&mut p2, 0, 100 * MS);
    let plain = events_of(&sent2, ready)[0].1;
    assert!(
        matches!(plain, AttrEvent::JoinIn | AttrEvent::JoinMt),
        "plain declare is a Join, got {plain:?}"
    );
    assert_ne!(plain, AttrEvent::New);
}

#[test]
fn mvrp_vid_declaration_and_registration_log() {
    let mut p = part();
    p.declare(MachTicks(0), Attribute::Vid { vid: 2 });
    let sent = run(&mut p, 0, 100 * MS);
    let mv: Vec<_> = sent.iter().filter(|s| s.1 == App::Mvrp).collect();
    assert_eq!(mv.len(), 1);
    assert_eq!(mv[0].2.events[0].attr, Attribute::Vid { vid: 2 });
    rx(
        &mut p,
        150 * MS,
        &peer_pdu(
            App::Mvrp,
            false,
            &[(Attribute::Vid { vid: 2 }, AttrEvent::JoinIn)],
        ),
    );
    assert_eq!(p.pop_event(), Some(SrpEvent::VidRegistered(2)));
    // One VID attribute per distinct VID.
    p.declare(MachTicks(200 * MS), Attribute::Vid { vid: 2 });
    p.declare(MachTicks(200 * MS), Attribute::Vid { vid: 3 });
    let st = p.status();
    assert_eq!(st.mvrp.iter().filter(|a| a.declared).count(), 2);
}

#[test]
fn leave_all_sent_and_registrations_refreshed() {
    let mut p = part();
    let a = Attribute::Talker(Talker::Advertise(ta(2)));
    rx(
        &mut p,
        0,
        &peer_pdu(App::Msrp, false, &[(a, AttrEvent::JoinIn)]),
    );
    let sent = run(&mut p, 0, 16 * S);
    let la: Vec<_> = sent
        .iter()
        .filter(|s| s.1 == App::Msrp && !s.2.leave_all.is_empty())
        .collect();
    assert_eq!(la.len(), 1, "one LeaveAll within 10–15 s");
    let t = la[0].0;
    assert!((10 * S..=15 * S + 10 * MS).contains(&t), "{t}");
    // Our LeaveAll moved the TA registration to LV; the 16R did not
    // re-declare, so it leaves after LeaveTime.
    assert_eq!(p.counters().leave_all_tx, 1);
    assert!(p.talker(SID).is_none());
    assert!(p.event_log().any(|l| l.what == "deregistered"));
    // Our Domain was re-declared in the LeaveAll PDU (txLA!: QA → sJ).
    assert!(la[0]
        .2
        .events
        .iter()
        .any(|e| e.attr == Attribute::Domain(DEFAULT_DOMAIN)));
    assert_eq!(p.leave_all_state(App::Msrp), LeaveAllState::Passive);
}

#[test]
fn received_leave_all_redeclares_within_join_time() {
    let mut p = part();
    run(&mut p, 0, S);
    let k = AttrKey::Domain(DEFAULT_DOMAIN);
    assert_eq!(p.applicant_state(App::Msrp, k), Some(Applicant::QA));
    // The peer's LeaveAll for Domain (carried on its own Domain vector).
    let theirs = Attribute::Domain(DEFAULT_DOMAIN);
    rx(
        &mut p,
        S,
        &peer_pdu(App::Msrp, true, &[(theirs, AttrEvent::JoinIn)]),
    );
    assert_eq!(p.counters().leave_all_rx, 1);
    let sent = run(&mut p, S, S + 250 * MS);
    let ev = events_of(&sent, theirs);
    assert!(!ev.is_empty() && ev[0].0 <= S + 10 * MS, "{ev:?}");
    assert_eq!(ev[0].1, AttrEvent::JoinIn);
    // Our own LeaveAll timer restarted: none sent for ≥ 10 s.
    let sent = run(&mut p, S + 250 * MS, 10 * S + 900 * MS);
    assert!(sent.iter().all(|s| s.2.leave_all.is_empty()));
}

#[test]
fn withdraw_all_sends_leave_for_everything() {
    let mut p = part();
    p.declare(MachTicks(0), Attribute::Vid { vid: 2 });
    p.declare(
        MachTicks(0),
        Attribute::Listener(Listener {
            stream_id: SID,
            decl: ListenerDecl::Ready,
        }),
    );
    run(&mut p, 0, S);
    p.withdraw_all(MachTicks(S));
    let sent = run(&mut p, S, S + 100 * MS);
    let mut lv: Vec<_> = sent
        .iter()
        .flat_map(|(_, _, d)| d.events.iter())
        .filter(|e| e.event == AttrEvent::Lv)
        .map(|e| e.attr.key())
        .collect();
    lv.sort();
    assert_eq!(
        lv,
        [
            AttrKey::Listener(SID),
            AttrKey::Domain(DEFAULT_DOMAIN),
            AttrKey::Vid(2)
        ]
    );
    assert!(p.domains_declared().is_empty());
    assert_eq!(p.counters().withdrawals, 3);
}

#[test]
fn tx_rate_limited_to_three_per_300ms() {
    let mut p = part();
    let mut times = Vec::new();
    for i in 0..40u64 {
        let t = i * 20 * MS;
        p.declare(
            MachTicks(t),
            Attribute::Listener(Listener {
                stream_id: StreamId(i),
                decl: ListenerDecl::Ready,
            }),
        );
        if !poll(&mut p, t).is_empty() {
            times.push(t);
        }
    }
    // New declarations go out immediately, at most 3 per 1.5 × JoinTime.
    assert_eq!(&times[..3], &[0, 20 * MS, 40 * MS]);
    for w in times.windows(4) {
        assert!(w[3] - w[0] >= 300 * MS, "{times:?}");
    }
    assert!(times.len() >= 9, "{times:?}");
}

#[test]
fn rx_filters() {
    let mut p = part();
    let f = peer_pdu(
        App::Msrp,
        false,
        &[(Attribute::Domain(DEFAULT_DOMAIN), AttrEvent::JoinIn)],
    );
    let m = RxMeta {
        loopback: true,
        ..RxMeta::default()
    };
    assert_eq!(
        p.handle_rx(now(0), &f, m),
        Err(avb_core::DropReason::OwnFrame)
    );
    let mut g = f.clone();
    g[6..12].copy_from_slice(&OUR_MAC.0);
    assert_eq!(
        p.handle_rx(now(0), &g, RxMeta::default()),
        Err(avb_core::DropReason::OwnFrame)
    );
    let mut g = f.clone();
    g[5] = 0x21;
    assert_eq!(
        p.handle_rx(now(0), &g, RxMeta::default()),
        Err(avb_core::DropReason::WrongDestination)
    );
    let mut g = f.clone();
    g[12] = 0x08;
    assert_eq!(
        p.handle_rx(now(0), &g, RxMeta::default()),
        Err(avb_core::DropReason::WrongEtherType)
    );
    let mut g = f.clone();
    g[16] = 9; // AttributeLength 9 for Domain
    assert!(p.handle_rx(now(0), &g, RxMeta::default()).is_err());
    assert_eq!(p.counters().pdu_dropped, 1);
    assert_eq!(p.counters().own_frames, 2);
    assert!(p.handle_rx(now(0), &f, RxMeta::default()).is_ok());
}

#[test]
fn next_deadline_and_status_serialize() {
    let mut p = part();
    assert_eq!(p.next_deadline(), Some(MachTicks(0)));
    run(&mut p, 0, 500 * MS);
    let d = p.next_deadline().unwrap();
    assert!(d.0 >= 10 * S, "only the LeaveAll timer remains: {d:?}");
    fn is_serialize<T: serde::Serialize>(_: &T) {}
    is_serialize(&p.status());
    is_serialize(&p.event_log().next().unwrap());
}
