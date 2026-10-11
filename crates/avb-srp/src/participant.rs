//! The end-station MRP participant for MSRP and MVRP (design §8): one
//! attribute table per application, each attribute with an applicant and a
//! registrar (802.1Q-2018 §10.7), one LeaveAll machine per application,
//! JoinTime / LeaveTime / LeaveAllTime timers, and the MSRP rules of §8.2
//! (Domain adoption, TalkerChanged, event log).
//!
//! Sans-IO: `handle_rx` / `poll` / `next_deadline`, time injected as mach
//! ticks with the caller's [`Timebase`], LeaveAllTime randomized from an
//! injected seed.
//!
//! [`Participant::declare`] takes any [`Attribute`], including a Talker
//! Advertise (the talker-ready path, design §12), and may declare a Listener
//! for a StreamID with no registered talker attribute (Asking Failed after
//! TA_WAIT, design §8.2): 802.1Q-2018 does not make an end station's Listener
//! declaration conditional on a Talker registration (see the FEAT-003 entry
//! in phase1a-impl-verification.md).

use std::collections::{BTreeMap, VecDeque};

use avb_core::{
    DropReason, EthFrame, EthHeader, MacAddr, MachTicks, Now, Outbox, RxMeta, StreamId, Timebase,
    TxFrame,
};
use serde::Serialize;

use crate::attr::{
    self, App, AttrKey, Attribute, Domain, ListenerDecl, Talker, DEFAULT_DOMAIN, ETHERTYPE_MSRP,
    ETHERTYPE_MVRP, MSRP_DEST, MVRP_DEST,
};
use crate::mrp::{
    self, AppEvent, Applicant, LaEvent, LeaveAllState, RegEvent, Registrar, Rng, SendAction,
    JOIN_TIME_NS, LEAVE_TIME_NS, TX_BURST, TX_BURST_WINDOW_NS,
};
use crate::pdu::{self, AttrEvent, PduBuilder};

#[derive(Clone, Copy, Debug)]
pub struct ParticipantConfig {
    pub mac: MacAddr,
    pub timebase: Timebase,
    /// Seed for the LeaveAllTime randomization.
    pub seed: u64,
}

/// A peer's registered attribute value with its first/last seen times.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Registration {
    pub attr: Attribute,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// What the MSRP/MVRP registrations did, for the ACMP listener machine and
/// the logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SrpEvent {
    /// A talker attribute registered, or switched between Advertise and
    /// Failed (802.1Q-2018 §35.2.6).
    TalkerRegistered {
        stream_id: StreamId,
        talker: Talker,
    },
    /// Same kind, different fields (dest MAC, VID, TSpec, accumulated latency, …).
    TalkerChanged {
        stream_id: StreamId,
        old: Talker,
        new: Talker,
    },
    /// The registration left (leave timer expired).
    TalkerWithdrawn {
        stream_id: StreamId,
        last: Talker,
    },
    ListenerRegistered {
        stream_id: StreamId,
        decl: ListenerDecl,
    },
    ListenerWithdrawn {
        stream_id: StreamId,
    },
    DomainRegistered(Domain),
    DomainWithdrawn(Domain),
    /// The peer's Domain for one of our SR classes differs: we re-declared
    /// with its values (design §8.2, SRP-1).
    DomainMismatch {
        ours: Domain,
        theirs: Domain,
    },
    VidRegistered(u16),
    VidWithdrawn(u16),
}

/// One MSRP/MVRP event-log line (SRP-6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LogEntry {
    pub mach: u64,
    pub what: &'static str,
    #[serde(flatten)]
    pub attribute: Attribute,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SrpCounters {
    pub pdu_rx: u64,
    pub pdu_tx: u64,
    pub pdu_dropped: u64,
    pub unknown_attribute_types: u64,
    pub own_frames: u64,
    pub tx_full: u64,
    pub leave_all_rx: u64,
    pub leave_all_tx: u64,
    pub declarations: u64,
    pub withdrawals: u64,
    pub registrations: u64,
    pub deregistrations: u64,
    pub talker_changed: u64,
    pub domain_mismatch: u64,
}

/// A declaration or registration in the status snapshot.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AttrStatus {
    pub attribute: Attribute,
    pub declared: bool,
    pub applicant: Applicant,
    pub registrar: Registrar,
    pub registration: Option<Registration>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SrpStatus {
    pub counters: SrpCounters,
    pub msrp: Vec<AttrStatus>,
    pub mvrp: Vec<AttrStatus>,
    pub domain_declared: Vec<Domain>,
}

#[derive(Clone, Debug)]
struct Entry {
    decl: Option<Attribute>,
    /// Value to encode our sends with (the last declared one, kept for Lv).
    tx_value: Option<Attribute>,
    reg: Option<Registration>,
    applicant: Applicant,
    registrar: Registrar,
    leave_at: Option<MachTicks>,
}

impl Entry {
    fn new() -> Self {
        Self {
            decl: None,
            tx_value: None,
            reg: None,
            applicant: Applicant::VO,
            registrar: Registrar::MT,
            leave_at: None,
        }
    }

    fn value(&self) -> Option<Attribute> {
        self.decl.or(self.tx_value).or(self.reg.map(|r| r.attr))
    }

    fn idle(&self) -> bool {
        self.decl.is_none()
            && self.reg.is_none()
            && self.applicant == Applicant::VO
            && self.registrar == Registrar::MT
            && self.leave_at.is_none()
    }
}

#[derive(Clone, Debug)]
struct AppState {
    entries: BTreeMap<AttrKey, Entry>,
    la: LeaveAllState,
    la_at: MachTicks,
    next_tx: Option<MachTicks>,
    tx_hist: VecDeque<MachTicks>,
}

/// Does a LeaveAll in a message of `attr_type` cover this key?
fn key_in_type(key: &AttrKey, app: App, attr_type: u8) -> bool {
    match (app, key) {
        (App::Msrp, AttrKey::Talker(_)) => matches!(
            attr_type,
            attr::MSRP_TALKER_ADVERTISE | attr::MSRP_TALKER_FAILED
        ),
        (App::Msrp, AttrKey::Listener(_)) => attr_type == attr::MSRP_LISTENER,
        (App::Msrp, AttrKey::Domain(_)) => attr_type == attr::MSRP_DOMAIN,
        (App::Mvrp, AttrKey::Vid(_)) => attr_type == attr::MVRP_VID,
        _ => false,
    }
}

const LOG_CAP: usize = 4096;

pub struct Participant {
    mac: MacAddr,
    tb: Timebase,
    rng: Rng,
    msrp: AppState,
    mvrp: AppState,
    events: VecDeque<SrpEvent>,
    log: VecDeque<LogEntry>,
    c: SrpCounters,
}

impl Participant {
    /// Begin! for both applications, and the default Domain {6, 3, 2}
    /// declared (design §8.2).
    pub fn new(cfg: ParticipantConfig, now: MachTicks) -> Self {
        let mut rng = Rng::new(cfg.seed);
        let mut mk = || AppState {
            entries: BTreeMap::new(),
            la: LeaveAllState::Passive,
            la_at: now.add_ticks(cfg.timebase.ns_to_ticks(rng.leave_all_ns())),
            next_tx: None,
            tx_hist: VecDeque::new(),
        };
        let msrp = mk();
        let mvrp = mk();
        let mut p = Self {
            mac: cfg.mac,
            tb: cfg.timebase,
            rng,
            msrp,
            mvrp,
            events: VecDeque::new(),
            log: VecDeque::new(),
            c: SrpCounters::default(),
        };
        p.declare(now, Attribute::Domain(DEFAULT_DOMAIN));
        p
    }

    fn st(&mut self, app: App) -> &mut AppState {
        match app {
            App::Msrp => &mut self.msrp,
            App::Mvrp => &mut self.mvrp,
        }
    }

    fn st_ref(&self, app: App) -> &AppState {
        match app {
            App::Msrp => &self.msrp,
            App::Mvrp => &self.mvrp,
        }
    }

    fn ticks(&self, ns: u64) -> u64 {
        self.tb.ns_to_ticks(ns)
    }

    fn log(&mut self, now: MachTicks, what: &'static str, attribute: Attribute) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry {
            mach: now.0,
            what,
            attribute,
        });
    }

    fn request_tx(&mut self, app: App, now: MachTicks) {
        let window = self.ticks(TX_BURST_WINDOW_NS);
        let st = self.st(app);
        // Point-to-point: transmit as soon as the burst limit allows, even if
        // a later opportunity (the JoinTime repeat) is already scheduled.
        let at = match st.tx_hist.front() {
            Some(first) if st.tx_hist.len() >= TX_BURST => now.max(first.add_ticks(window)),
            _ => now,
        };
        st.next_tx = Some(st.next_tx.map_or(at, |t| t.min(at)));
    }

    fn app_event(&mut self, app: App, key: AttrKey, e: AppEvent, now: MachTicks) {
        let st = self.st(app);
        let entry = st.entries.entry(key).or_insert_with(Entry::new);
        let (next, _) = mrp::applicant(entry.applicant, e, entry.registrar == Registrar::MT);
        entry.applicant = next;
        if next.wants_tx() {
            self.request_tx(app, now);
        }
    }

    // --- declarations ---

    /// Declare (or re-declare with a new value) an attribute. A first
    /// declaration is MAD_Join (Join!); a changed value of a declared
    /// attribute (Ready ↔ Asking Failed, Advertise ↔ Failed, changed TSpec)
    /// is sent as New! so a quiet applicant transmits it (802.1Q-2018
    /// §35.2.6 *(verify)*).
    pub fn declare(&mut self, now: MachTicks, attr: Attribute) {
        let app = attr.app();
        let key = attr.key();
        let st = self.st(app);
        let entry = st.entries.entry(key).or_insert_with(Entry::new);
        let ev = match entry.decl {
            Some(old) if old == attr && entry.applicant.declaring() => return,
            Some(_) if entry.applicant.declaring() => AppEvent::New,
            _ => AppEvent::Join,
        };
        entry.decl = Some(attr);
        entry.tx_value = Some(attr);
        self.c.declarations += 1;
        self.log(now, "declare", attr);
        self.app_event(app, key, ev, now);
    }

    /// Declare an attribute, forcing a `New!` event even on first declaration
    /// or re-declaration of the same value. 802.1Q-2018 §35.2.6: `New!`
    /// signals a fresh registration and forces propagation, where a quiet
    /// `Join!` may be ignored by a peer that already holds state. A strict
    /// Milan talker only advertises its stream in response to a Listener
    /// `New!` declaration (observed against a PreSonus StudioLive 16R: a
    /// `Join!` Listener Ready drew only an empty LeaveAll, a `New!` drew a
    /// real Talker Advertise).
    pub fn declare_new(&mut self, now: MachTicks, attr: Attribute) {
        let app = attr.app();
        let key = attr.key();
        let st = self.st(app);
        let entry = st.entries.entry(key).or_insert_with(Entry::new);
        entry.decl = Some(attr);
        entry.tx_value = Some(attr);
        self.c.declarations += 1;
        self.log(now, "declare_new", attr);
        self.app_event(app, key, AppEvent::New, now);
    }

    /// Withdraw a declaration (Lv!). No effect if it isn't declared.
    pub fn withdraw(&mut self, now: MachTicks, app: App, key: AttrKey) {
        let st = self.st(app);
        let Some(entry) = st.entries.get_mut(&key) else {
            return;
        };
        let Some(attr) = entry.decl.take() else {
            return;
        };
        self.c.withdrawals += 1;
        self.log(now, "withdraw", attr);
        self.app_event(app, key, AppEvent::Lv, now);
    }

    /// Withdraw every MSRP and MVRP declaration (clean shutdown, SRP-7).
    pub fn withdraw_all(&mut self, now: MachTicks) {
        for app in [App::Msrp, App::Mvrp] {
            let keys: Vec<AttrKey> = self
                .st_ref(app)
                .entries
                .iter()
                .filter(|(_, e)| e.decl.is_some())
                .map(|(k, _)| *k)
                .collect();
            for k in keys {
                self.withdraw(now, app, k);
            }
        }
    }

    pub fn declared(&self, app: App, key: AttrKey) -> Option<Attribute> {
        self.st_ref(app).entries.get(&key).and_then(|e| e.decl)
    }

    /// The talker attribute registered for `stream_id` (IN or leaving).
    pub fn talker(&self, stream_id: StreamId) -> Option<Registration> {
        self.msrp
            .entries
            .get(&AttrKey::Talker(stream_id))
            .and_then(|e| e.reg)
    }

    /// Our declared Domain values.
    pub fn domains_declared(&self) -> Vec<Domain> {
        self.msrp
            .entries
            .values()
            .filter_map(|e| match e.decl {
                Some(Attribute::Domain(d)) => Some(d),
                _ => None,
            })
            .collect()
    }

    pub fn pop_event(&mut self) -> Option<SrpEvent> {
        self.events.pop_front()
    }

    pub fn event_log(&self) -> impl Iterator<Item = &LogEntry> {
        self.log.iter()
    }

    pub fn counters(&self) -> &SrpCounters {
        &self.c
    }

    pub fn applicant_state(&self, app: App, key: AttrKey) -> Option<Applicant> {
        self.st_ref(app).entries.get(&key).map(|e| e.applicant)
    }

    pub fn registrar_state(&self, app: App, key: AttrKey) -> Option<Registrar> {
        self.st_ref(app).entries.get(&key).map(|e| e.registrar)
    }

    pub fn leave_all_state(&self, app: App) -> LeaveAllState {
        self.st_ref(app).la
    }

    // --- RX ---

    pub fn handle_rx(&mut self, now: Now, frame: &[u8], meta: RxMeta) -> Result<(), DropReason> {
        let eth = EthFrame::parse_with_desc_vlan(frame, meta.desc_vlan_tci)?;
        let app = match (eth.hdr.ethertype, eth.hdr.dst) {
            (ETHERTYPE_MSRP, MSRP_DEST) => App::Msrp,
            (ETHERTYPE_MVRP, MVRP_DEST) => App::Mvrp,
            (ETHERTYPE_MSRP | ETHERTYPE_MVRP, _) => return Err(DropReason::WrongDestination),
            _ => return Err(DropReason::WrongEtherType),
        };
        if meta.loopback || eth.hdr.src == self.mac {
            self.c.own_frames += 1;
            return Err(DropReason::OwnFrame);
        }
        let p = match pdu::decode(app, eth.payload) {
            Ok(p) => p,
            Err(e) => {
                self.c.pdu_dropped += 1;
                return Err(e);
            }
        };
        self.c.pdu_rx += 1;
        self.c.unknown_attribute_types += u64::from(p.unknown_types);
        let now = now.mach;
        for t in &p.leave_all {
            self.c.leave_all_rx += 1;
            self.rx_leave_all(app, *t, now);
        }
        for e in &p.events {
            self.rx_event(app, e.attr, e.event, now);
        }
        Ok(())
    }

    fn rx_leave_all(&mut self, app: App, attr_type: u8, now: MachTicks) {
        let leave = self.ticks(LEAVE_TIME_NS);
        let la_ns = self.rng.leave_all_ns();
        let la = self.ticks(la_ns);
        let st = self.st(app);
        let (s, _, restart) = mrp::leave_all(st.la, LaEvent::RLa);
        st.la = s;
        if restart {
            st.la_at = now.add_ticks(la);
        }
        let mut want_tx = false;
        for (k, e) in st.entries.iter_mut() {
            if !key_in_type(k, app, attr_type) {
                continue;
            }
            e.applicant =
                mrp::applicant(e.applicant, AppEvent::RLa, e.registrar == Registrar::MT).0;
            want_tx |= e.applicant.wants_tx();
            let (r, a) = mrp::registrar(e.registrar, RegEvent::RLa);
            e.registrar = r;
            if a.start_leave_timer {
                e.leave_at = Some(now.add_ticks(leave));
            }
        }
        if want_tx {
            self.request_tx(app, now);
        }
    }

    fn rx_event(&mut self, app: App, attr: Attribute, ev: AttrEvent, now: MachTicks) {
        let key = attr.key();
        let app_ev = match ev {
            AttrEvent::New => AppEvent::RNew,
            AttrEvent::JoinIn => AppEvent::RJoinIn,
            AttrEvent::In => AppEvent::RIn,
            AttrEvent::JoinMt => AppEvent::RJoinMt,
            AttrEvent::Mt => AppEvent::RMt,
            AttrEvent::Lv => AppEvent::RLv,
        };
        let reg_ev = match ev {
            AttrEvent::New => Some(RegEvent::RNew),
            AttrEvent::JoinIn => Some(RegEvent::RJoinIn),
            AttrEvent::JoinMt => Some(RegEvent::RJoinMt),
            AttrEvent::Lv => Some(RegEvent::RLv),
            AttrEvent::In | AttrEvent::Mt => None,
        };
        // The applicant first, with the registrar state before this event.
        self.app_event(app, key, app_ev, now);
        let Some(reg_ev) = reg_ev else { return };
        let leave = self.ticks(LEAVE_TIME_NS);
        let st = self.st(app);
        let Some(e) = st.entries.get_mut(&key) else {
            return;
        };
        // A Leave for the talker type that is not the registered one (after
        // an Advertise ↔ Failed switch, §35.2.6) is stale: ignore it.
        if reg_ev == RegEvent::RLv {
            if let (Some(r), Attribute::Talker(t)) = (e.reg, attr) {
                if let Attribute::Talker(rt) = r.attr {
                    if rt.is_failed() != t.is_failed() {
                        return;
                    }
                }
            }
        }
        let (r, a) = mrp::registrar(e.registrar, reg_ev);
        e.registrar = r;
        if a.start_leave_timer {
            e.leave_at = Some(now.add_ticks(leave));
        }
        if a.stop_leave_timer {
            e.leave_at = None;
        }
        if r != Registrar::IN || reg_ev == RegEvent::RLv {
            return;
        }
        let old = e.reg;
        let first_seen = old.map_or(now.0, |o| o.first_seen);
        e.reg = Some(Registration {
            attr,
            first_seen,
            last_seen: now.0,
        });
        match old {
            None => self.registered(app, attr, now),
            Some(o) if o.attr != attr => self.reg_changed(o.attr, attr, now),
            Some(_) => {}
        }
    }

    fn registered(&mut self, _app: App, attr: Attribute, now: MachTicks) {
        self.c.registrations += 1;
        self.log(now, "registered", attr);
        let ev = match attr {
            Attribute::Talker(t) => SrpEvent::TalkerRegistered {
                stream_id: t.stream_id(),
                talker: t,
            },
            Attribute::Listener(l) => SrpEvent::ListenerRegistered {
                stream_id: l.stream_id,
                decl: l.decl,
            },
            Attribute::Domain(d) => {
                self.events.push_back(SrpEvent::DomainRegistered(d));
                self.adopt_domain(d, now);
                return;
            }
            Attribute::Vid { vid } => SrpEvent::VidRegistered(vid),
        };
        self.events.push_back(ev);
    }

    fn reg_changed(&mut self, old: Attribute, new: Attribute, now: MachTicks) {
        match (old, new) {
            (Attribute::Talker(o), Attribute::Talker(n)) if o.is_failed() == n.is_failed() => {
                self.c.talker_changed += 1;
                self.log(now, "talker_changed", new);
                self.events.push_back(SrpEvent::TalkerChanged {
                    stream_id: n.stream_id(),
                    old: o,
                    new: n,
                });
            }
            (Attribute::Talker(_), Attribute::Talker(n)) => {
                self.log(now, "registered", new);
                self.events.push_back(SrpEvent::TalkerRegistered {
                    stream_id: n.stream_id(),
                    talker: n,
                });
            }
            (_, Attribute::Listener(l)) => {
                self.log(now, "registered", new);
                self.events.push_back(SrpEvent::ListenerRegistered {
                    stream_id: l.stream_id,
                    decl: l.decl,
                });
            }
            _ => {}
        }
    }

    /// The peer's Domain for a class we declare with other values: adopt
    /// them (design §8.2).
    fn adopt_domain(&mut self, theirs: Domain, now: MachTicks) {
        let ours: Vec<Domain> = self
            .domains_declared()
            .into_iter()
            .filter(|d| d.class_id == theirs.class_id && *d != theirs)
            .collect();
        for d in ours {
            self.c.domain_mismatch += 1;
            self.events
                .push_back(SrpEvent::DomainMismatch { ours: d, theirs });
            self.log(now, "domain_mismatch", Attribute::Domain(theirs));
            self.withdraw(now, App::Msrp, AttrKey::Domain(d));
            self.declare(now, Attribute::Domain(theirs));
        }
    }

    fn deregistered(&mut self, last: Attribute, now: MachTicks) {
        self.c.deregistrations += 1;
        self.log(now, "deregistered", last);
        self.events.push_back(match last {
            Attribute::Talker(t) => SrpEvent::TalkerWithdrawn {
                stream_id: t.stream_id(),
                last: t,
            },
            Attribute::Listener(l) => SrpEvent::ListenerWithdrawn {
                stream_id: l.stream_id,
            },
            Attribute::Domain(d) => SrpEvent::DomainWithdrawn(d),
            Attribute::Vid { vid } => SrpEvent::VidWithdrawn(vid),
        });
    }

    // --- timers / TX ---

    pub fn poll<const N: usize>(&mut self, now: Now, out: &mut Outbox<N>) {
        let now = now.mach;
        for app in [App::Msrp, App::Mvrp] {
            self.leave_timers(app, now);
            if now >= self.st_ref(app).la_at {
                let la_ns = self.rng.leave_all_ns();
                let la_ticks = self.ticks(la_ns);
                let st = self.st(app);
                let (s, _, _) = mrp::leave_all(st.la, LaEvent::LeaveAllTimer);
                st.la = s;
                st.la_at = now.add_ticks(la_ticks);
                self.request_tx(app, now);
            }
            if self.st_ref(app).next_tx.is_some_and(|t| now >= t) {
                // Burst limit applies to every opportunity, scheduled repeats too.
                let window = self.ticks(TX_BURST_WINDOW_NS);
                let st = self.st(app);
                let free_at = match st.tx_hist.front() {
                    Some(first) if st.tx_hist.len() >= TX_BURST => first.add_ticks(window),
                    _ => now,
                };
                if now < free_at {
                    st.next_tx = Some(free_at);
                } else {
                    self.transmit(app, now, out);
                }
            }
            self.st(app).entries.retain(|_, e| !e.idle());
        }
    }

    fn leave_timers(&mut self, app: App, now: MachTicks) {
        let mut gone = Vec::new();
        let st = self.st(app);
        for e in st.entries.values_mut() {
            if e.leave_at.is_some_and(|t| now >= t) {
                e.leave_at = None;
                let (r, a) = mrp::registrar(e.registrar, RegEvent::LeaveTimer);
                e.registrar = r;
                if a.leave {
                    if let Some(reg) = e.reg.take() {
                        gone.push(reg.attr);
                    }
                }
            }
        }
        for g in gone {
            self.deregistered(g, now);
        }
    }

    fn transmit<const N: usize>(&mut self, app: App, now: MachTicks, out: &mut Outbox<N>) {
        let leave = self.ticks(LEAVE_TIME_NS);
        let join = self.ticks(JOIN_TIME_NS);
        let mac = self.mac;
        let st = self.st(app);
        st.next_tx = None;
        let (la_next, send_la, _) = mrp::leave_all(st.la, LaEvent::Tx);
        st.la = la_next;
        let mut b = PduBuilder::new(app, send_la);
        for e in st.entries.values_mut() {
            let reg_mt = e.registrar == Registrar::MT;
            let ev = if send_la {
                AppEvent::TxLa
            } else {
                AppEvent::Tx
            };
            let (next, send) = mrp::applicant(e.applicant, ev, reg_mt);
            let reg_in = e.registrar == Registrar::IN;
            let encoded = match send {
                SendAction::None | SendAction::Optional => None,
                SendAction::New => Some(AttrEvent::New),
                SendAction::Join => Some(if reg_in {
                    AttrEvent::JoinIn
                } else {
                    AttrEvent::JoinMt
                }),
                SendAction::Leave => Some(AttrEvent::Lv),
                SendAction::InOrMt => Some(if reg_in { AttrEvent::In } else { AttrEvent::Mt }),
            };
            match (encoded, e.value()) {
                (Some(ae), Some(v)) => {
                    let decl = match (ae, e.decl) {
                        (AttrEvent::New | AttrEvent::JoinIn | AttrEvent::JoinMt, Some(d)) => {
                            d.four_packed().unwrap_or(ListenerDecl::Ignore)
                        }
                        _ => ListenerDecl::Ignore,
                    };
                    if b.push(&v, ae, decl) {
                        e.applicant = next;
                    } else if send_la {
                        // PDU full: txLAF! for what did not fit.
                        e.applicant = mrp::applicant(e.applicant, AppEvent::TxLaf, reg_mt).0;
                    }
                }
                _ => e.applicant = next,
            }
            if send_la {
                let (r, a) = mrp::registrar(e.registrar, RegEvent::TxLa);
                e.registrar = r;
                if a.start_leave_timer {
                    e.leave_at = Some(now.add_ticks(leave));
                }
            }
        }
        if st.tx_hist.len() >= TX_BURST {
            st.tx_hist.pop_front();
        }
        st.tx_hist.push_back(now);
        if st.entries.values().any(|e| e.applicant.wants_tx()) || st.la == LeaveAllState::Active {
            st.next_tx = Some(now.add_ticks(join));
        }
        if b.is_empty() {
            return;
        }
        let payload = b.finish();
        let (dst, ethertype) = match app {
            App::Msrp => (MSRP_DEST, ETHERTYPE_MSRP),
            App::Mvrp => (MVRP_DEST, ETHERTYPE_MVRP),
        };
        let hdr = EthHeader {
            dst,
            src: mac,
            vlan: None,
            ethertype,
        };
        let Ok(f) = TxFrame::build(&hdr, &payload, false, 0) else {
            return;
        };
        if out.push(f).is_err() {
            self.c.tx_full += 1;
            return;
        }
        self.c.pdu_tx += 1;
        if send_la {
            self.c.leave_all_tx += 1;
        }
    }

    /// Earliest time `poll` has work.
    pub fn next_deadline(&self) -> Option<MachTicks> {
        [&self.msrp, &self.mvrp]
            .iter()
            .flat_map(|st| {
                let leaves = st.entries.values().filter_map(|e| e.leave_at);
                std::iter::once(Some(st.la_at))
                    .chain(std::iter::once(st.next_tx))
                    .flatten()
                    .chain(leaves)
                    .collect::<Vec<_>>()
            })
            .min()
    }

    pub fn status(&self) -> SrpStatus {
        let snap = |st: &AppState| {
            st.entries
                .values()
                .filter_map(|e| {
                    Some(AttrStatus {
                        attribute: e.value()?,
                        declared: e.decl.is_some(),
                        applicant: e.applicant,
                        registrar: e.registrar,
                        registration: e.reg,
                    })
                })
                .collect()
        };
        SrpStatus {
            counters: self.c,
            msrp: snap(&self.msrp),
            mvrp: snap(&self.mvrp),
            domain_declared: self.domains_declared(),
        }
    }
}
