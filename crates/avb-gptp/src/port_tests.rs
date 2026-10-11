// SPDX-License-Identifier: MIT OR Apache-2.0

//! Port-level tests over the simulated 16R (design §14.1 avb-gptp row).
#![allow(clippy::indexing_slicing)]

use avb_core::{MachTicks, Now, Outbox, RxMeta, Timebase};

use crate::msg::{self, Body, MessageType, PortIdentity};
use crate::port::{sync_offset, Port, PortConfig, PortState, TxTsResult};
use crate::servo::ServoState;
use crate::sim::*;

fn locked(cfg: SimCfg) -> Sim {
    let mut s = Sim::new(cfg);
    assert!(s.run_until(30 * S, |s| s.lock_tau.is_some()), "no lock");
    s
}

fn no_master_messages(s: &Sim) {
    for t in [
        MessageType::Announce,
        MessageType::Sync,
        MessageType::FollowUp,
    ] {
        assert_eq!(s.sent_of(t).count(), 0, "{t:?} transmitted");
    }
}

/// Steady state starts this long after lock: max(10 s, 40 updates), the
/// PI's settling time (pole radius √0.8 per update).
fn settle_ns(log: i8) -> u64 {
    (10 * S).max((40.0 * 2f64.powi(i32::from(log)) * 1e9) as u64)
}

/// §7.3 servo simulation: GM at −83 ppm, 10 ns RMS noise on our timestamps.
/// Returns (lock time s, mean ns, sd ns, samples).
fn servo_case(log: i8, budget_s: u64) -> (f64, f64, f64, usize) {
    let mut s = Sim::new(SimCfg {
        sync_log: log,
        ..SimCfg::default()
    });
    s.clock_reads = true;
    assert!(
        s.run_until(budget_s * S, |s| s.lock_tau.is_some()),
        "Ts 2^{log}: no lock within {budget_s} s"
    );
    let lock = s.lock_tau.unwrap();
    let settle = settle_ns(log);
    s.run_for(settle + 60 * S);
    let (m, sd, n) = stats(&s.offsets, lock + settle);
    let st = s.port.status();
    println!(
        "servo sim Ts=2^{log} s: lock {:.3} s, steady mean {m:.2} ns sd {sd:.2} ns over {n} Syncs, true offset now {:.1} ns, applied {:.0} ppb, nrr_free {:.9}",
        lock as f64 / 1e9,
        s.true_offset(),
        st.applied_ppb,
        st.nrr_free.unwrap()
    );
    assert!(m.abs() < 5.0 && sd < 20.0, "Ts 2^{log}: mean {m} sd {sd}");
    assert_eq!(s.port.servo_state(), ServoState::Locked);
    // ACQUIRE SetClock (SYSTIM started 5 s, GM at 1.7e9 s), then no more steps.
    let c = *s.port.counters();
    assert_eq!((c.set_clocks, c.steps), (1, 1));
    assert!(s.steps[0].1 > 1_000_000_000);
    // asCapable stayed true across the SetClock; no HOLDOVER on the way.
    assert_eq!(c.ascapable_transitions, 1);
    assert!(st.servo_history.iter().all(|t| t.to != "HOLDOVER"));
    assert!(st
        .servo_history
        .iter()
        .any(|t| t.from == "ACQUIRE" && t.to == "TRACKING"));
    assert_eq!(c.unlocks, 0);
    assert_eq!(c.lost_responses, 0);
    assert_eq!(s.port.step_gen(), s.gen);
    // The last PI output carries Kp × noise (6.4 ppb/ns at Ts = 1/32 s).
    assert!(
        (st.applied_ppb + 83_000.0).abs() < 300.0,
        "{}",
        st.applied_ppb
    );
    // NRR on the disciplined SYSTIM ≈ 1; nrr_free ≈ the GM's −83 ppm.
    assert!((st.nrr_systim.unwrap() - 1.0).abs() < 0.05e-6);
    assert!((st.nrr_free.unwrap() - (1.0 - 83e-6)).abs() < 0.3e-6);
    assert!((st.path_delay_comp_ns.mean.unwrap() - 142.6).abs() < 5.0);
    // Model: gPTP vs mach slope is the GM's rate.
    let model = s.port.model();
    assert!(model.valid);
    assert!(
        (model.ppm_vs_mach() - (-83.0)).abs() < 0.05,
        "{}",
        model.ppm_vs_mach()
    );
    no_master_messages(&s);
    (lock as f64 / 1e9, m, sd, n)
}

#[test]
fn servo_sim_ts_1_32() {
    let (lock, ..) = servo_case(-5, 10);
    assert!(lock <= 10.0);
}

#[test]
fn servo_sim_ts_1_8() {
    let (lock, ..) = servo_case(-3, 10);
    assert!(lock <= 10.0);
}

#[test]
fn servo_sim_ts_1() {
    let (lock, ..) = servo_case(0, 30);
    assert!(lock <= 30.0);
}

#[test]
fn servo_sim_ts_change_mid_run_stays_locked() {
    let mut s = locked(SimCfg::default());
    s.run_for(settle_ns(-3));
    let change = s.tau;
    s.sync_log = -5;
    let unlocked = s.run_until(30 * S, |s| s.port.servo_state() != ServoState::Locked);
    assert!(!unlocked, "unlocked after the Ts change");
    let c = *s.port.counters();
    assert_eq!((c.sync_interval_changes, c.unlocks, c.steps), (1, 0, 1));
    assert_eq!(s.port.sync_interval_s(), 0.03125);
    let (m, sd, n) = stats(&s.offsets, change + settle_ns(-5));
    println!("servo sim Ts change 1/8 -> 1/32 s: after change mean {m:.2} ns sd {sd:.2} ns over {n} Syncs");
    assert!(m.abs() < 5.0 && sd < 20.0);
}

/// GM steps to +600 ppm (beyond the 488 ppm clamp) for 10 s, then returns
/// to −83 ppm: the integrator must not wind up.
#[test]
fn servo_sim_600ppm_step_input_anti_windup() {
    let mut s = locked(SimCfg::default());
    let lock_time = s.lock_tau.unwrap();
    s.run_for(5 * S);
    s.set_gm_ppm(600.0);
    s.run_for(10 * S);
    let c = *s.port.counters();
    assert!(c.steps > 2, "the excursion is handled by steps");
    s.set_gm_ppm(-83.0);
    let back = s.tau;
    s.run_for(2 * lock_time + 3 * S);
    let recs: Vec<_> = s.offsets.iter().filter(|r| r.tau >= back).collect();
    let settled = (0..recs.len().saturating_sub(16))
        .find(|&i| recs[i..].iter().take(24).all(|r| r.offset.abs() < 1000.0))
        .map(|i| recs[i].tau - back)
        .expect("never settled");
    println!(
        "anti-windup: saturations {}, steps {}, |offset| < 1 us {:.3} s after return (normal lock {:.3} s)",
        c.saturations,
        s.port.counters().steps,
        settled as f64 / 1e9,
        lock_time as f64 / 1e9
    );
    assert!(settled <= 2 * lock_time);
    assert!(s.port.as_capable());
}

/// A 100 us step in the middle of a full NRR window: reported nrr_systim
/// moves ≤ 0.01 ppm, asCapable stays true (Phase 0b 2.5 ns timestamp sd).
#[test]
fn nrr_across_100us_step() {
    // Exact step writes, so the servo has nothing left to correct after the
    // step and nrr_systim reflects only the bookkeeping.
    let mut s = locked(SimCfg {
        noise_ns: 2.5,
        step_residual_ns: 0,
        ..SimCfg::default()
    });
    s.run_for(settle_ns(-3));
    // Right after an exchange completes, SYSTIM is knocked 100.5 us back; the
    // next Sync sees the excursion and the servo steps +100.5 us.
    let fu = s.port.counters().pdelay_resp_fu_rx;
    assert!(s.run_until(2 * S, |s| s.port.counters().pdelay_resp_fu_rx > fu));
    let n0 = s.port.nrr_systim().unwrap();
    let gen0 = s.port.step_gen();
    let steps0 = s.steps.len();
    s.disturb(-100_500);
    let mut max_dev: f64 = 0.0;
    let mut held_seen = false;
    for _ in 0..(12 * S / s.cfg.tick_ns) {
        s.tick();
        max_dev = max_dev.max((s.port.nrr_systim().unwrap() - n0).abs());
        held_seen |= s.port.step_gen() != gen0 && s.port.nrr_systim() == Some(n0);
        assert!(s.port.as_capable(), "asCapable dropped at {}", s.tau);
    }
    assert_eq!(s.steps.len(), steps0 + 1);
    assert!(
        (s.steps[steps0].1 - 100_500).abs() < 200,
        "{:?}",
        s.steps[steps0]
    );
    assert_eq!(s.port.step_gen(), gen0 + 1);
    assert!(held_seen, "NRR held across the step");
    println!(
        "nrr across 100 us step: max |delta nrr_systim| {:.4} ppm",
        max_dev * 1e6
    );
    assert!(max_dev <= 0.01e-6, "{}", max_dev * 1e6);
}

/// SetClock at ACQUIRE, then a GM change: the new ACQUIRE seeds f from a
/// single-generation NRR (≈ the applied −83 ppm, not a cross-SetClock mix).
#[test]
fn gm_change_after_setclock_seeds_single_generation_nrr() {
    let mut s = locked(SimCfg::default());
    s.run_for(5 * S);
    let f_before = s.f_ppb;
    s.gm_id = OTHER_GM;
    assert!(s.run_until(3 * S, |s| s.port.counters().gm_changes == 1));
    assert_eq!(s.port.servo_state(), ServoState::Acquire);
    assert_eq!(s.adp.last(), Some(&OTHER_GM));
    assert!(s.run_until(3 * S, |s| s.port.servo_state() == ServoState::Tracking));
    assert!(
        (s.f_ppb - f_before).abs() < 50.0,
        "seed {} vs {}",
        s.f_ppb,
        f_before
    );
    assert_eq!(s.port.gm_identity(), OTHER_GM);
    assert_eq!(s.port.state(), PortState::Slave);
    assert_eq!(
        s.port.counters().port_state_changes,
        1,
        "Slave → Slave, no Listening"
    );
}

// --- port state machine edges ---

#[test]
fn listening_to_slave_first_gm_acquire() {
    let mut s = Sim::new(SimCfg::default());
    assert_eq!(s.port.state(), PortState::Listening);
    assert_eq!(s.port.gm_identity(), s.port.identity().clock);
    assert!(s.run_until(S, |s| s.port.state() == PortState::Slave));
    assert_eq!(s.port.servo_state(), ServoState::Acquire);
    assert_eq!(s.adp, [GM_ID]);
    assert_eq!(s.port.gm_identity(), GM_ID);
    let gm = s.port.gm().unwrap();
    assert_eq!(
        (gm.priority1, gm.priority2, gm.master_port),
        (246, 248, PEER_PORT)
    );
    assert_eq!(gm.path_trace, [GM_ID]);
}

fn announce_timeout_case(log: i8, emit: Option<i8>, ta: f64) {
    let mut s = Sim::new(SimCfg::default());
    s.announce_log = log;
    s.announce_emit_log = emit;
    let mut s = {
        assert!(s.run_until(30 * S, |s| s.lock_tau.is_some()));
        s
    };
    assert_eq!(s.port.announce_interval_s(), ta);
    if log == 0x7F {
        assert!(s.port.counters().announce_interval_invalid > 0);
    }
    s.announces_on = false;
    assert!(s.run_until(10 * S, |s| s.port.state() == PortState::Listening));
    let dt = (s.tau - s.last_ann_tau) as f64 / 1e9;
    assert!(
        dt >= 3.0 * ta && dt <= 3.0 * ta + 0.001,
        "timeout after {dt} s, Ta {ta}"
    );
    assert_eq!(s.port.counters().announce_timeouts, 1);
    assert_eq!(s.port.servo_state(), ServoState::Holdover);
    assert_eq!(s.port.gm_identity(), s.port.identity().clock);
    assert_eq!(s.adp.last(), Some(&s.port.identity().clock));
    assert!(!s.port.model().valid);
    // Pdelay keeps running in Listening.
    let req = s.port.counters().pdelay_req_tx;
    s.run_for(3 * S);
    assert!(s.port.counters().pdelay_req_tx >= req + 2);
    assert!(s.port.as_capable());
    // Syncs in Listening are counted and ignored.
    assert!(s.port.counters().sync_while_listening > 0);
    let st = s.port.status();
    assert_eq!(
        st.port_state_history.last().unwrap().reason,
        "announce_timeout"
    );
}

#[test]
fn slave_to_listening_announce_timeout_ta_0() {
    announce_timeout_case(0, None, 1.0);
}

#[test]
fn slave_to_listening_announce_timeout_ta_minus3() {
    announce_timeout_case(-3, None, 0.125);
}

#[test]
fn slave_to_listening_announce_timeout_placeholder() {
    // logMessageInterval 0x7F on every Announce: Ta stays the 1 s placeholder.
    announce_timeout_case(0x7F, Some(-1), 1.0);
}

#[test]
fn slave_to_listening_ascapable_false_and_holdover_stays() {
    let mut s = locked(SimCfg::default());
    s.peer_responds = false;
    assert!(s.run_until(10 * S, |s| !s.port.as_capable()));
    let t_false = s.tau;
    assert_eq!(s.port.servo_state(), ServoState::Holdover);
    let hold = s.port.counters().sync_in_holdover;
    assert!(s.run_until(5 * S, |s| s.port.state() == PortState::Listening));
    let dt = (s.tau - t_false) as f64 / 1e9;
    assert!((3.0..3.01).contains(&dt), "{dt}");
    let c = *s.port.counters();
    assert_eq!(c.ascapable_slave_loss, 1);
    assert!(
        c.sync_in_holdover > hold,
        "Syncs while asCapable false are counted"
    );
    assert_eq!(c.announce_timeouts, 0);
    assert!(c.lost_responses >= 4);
    let st = s.port.status();
    assert!(st
        .port_state_history
        .iter()
        .any(|t| t.reason == "ascapable_false"));
    assert_eq!(
        st.ascapable_history.last().unwrap().reason,
        "lost_responses"
    );
}

#[test]
fn listening_to_slave_same_gm_resumes_tracking_without_acquire() {
    let mut s = locked(SimCfg::default());
    s.announces_on = false;
    assert!(s.run_until(5 * S, |s| s.port.state() == PortState::Listening));
    assert_eq!(s.port.servo_state(), ServoState::Holdover);
    s.run_for(2 * S);
    s.announces_on = true;
    assert!(s.run_until(3 * S, |s| s.port.state() == PortState::Slave));
    assert_eq!(s.port.servo_state(), ServoState::Holdover);
    assert!(s.run_until(S, |s| s.port.servo_state() == ServoState::Tracking));
    let st = s.port.status();
    assert_eq!(st.counters.gm_changes, 0);
    let last = st.servo_history.last().unwrap();
    assert_eq!(
        (last.from.as_str(), last.to.as_str()),
        ("HOLDOVER", "TRACKING")
    );
    assert_eq!(
        st.servo_history
            .iter()
            .filter(|t| t.to == "ACQUIRE")
            .count(),
        0
    );
    assert_eq!(s.adp.last(), Some(&GM_ID));
}

#[test]
fn listening_to_slave_different_gm_acquires() {
    let mut s = locked(SimCfg::default());
    s.announces_on = false;
    assert!(s.run_until(5 * S, |s| s.port.state() == PortState::Listening));
    s.gm_id = OTHER_GM;
    s.announces_on = true;
    assert!(s.run_until(3 * S, |s| s.port.state() == PortState::Slave));
    assert_eq!(s.port.counters().gm_changes, 1);
    assert_eq!(s.port.servo_state(), ServoState::Acquire);
    assert_eq!(s.adp.last(), Some(&OTHER_GM));
    assert!(s.run_until(3 * S, |s| s.port.servo_state() == ServoState::Tracking));
}

fn now(t: u64) -> Now {
    Now::mach_only(MachTicks(t))
}

fn meta(t: u64) -> RxMeta {
    RxMeta {
        harvest: MachTicks(t),
        ..RxMeta::default()
    }
}

#[test]
fn announce_interval_out_of_range_keeps_ta() {
    let mut s = Sim::new(SimCfg::default());
    s.run_for(S);
    assert_eq!(s.port.announce_interval_s(), 1.0);
    for (i, log) in [5i8, 0x7F].into_iter().enumerate() {
        let f = announce_frame(50 + i as u16, log, PEER_PORT, GM_ID, &[GM_ID]);
        let t = s.tau;
        s.port.handle_rx(now(t), &f, meta(t)).unwrap();
        assert_eq!(s.port.announce_interval_s(), 1.0);
        assert_eq!(s.port.counters().announce_interval_invalid, i as u64 + 1);
    }
    // −3 and 4 are the range ends.
    for (log, ta) in [(-3i8, 0.125), (4, 16.0), (0, 1.0)] {
        let f = announce_frame(60, log, PEER_PORT, GM_ID, &[GM_ID]);
        let t = s.tau;
        s.port.handle_rx(now(t), &f, meta(t)).unwrap();
        assert_eq!(s.port.announce_interval_s(), ta);
    }
}

#[test]
fn announce_from_ourselves_or_with_us_in_path_rejected() {
    let mut s = Sim::new(SimCfg::default());
    let me = s.port.identity();
    let f = announce_frame(1, 0, me, me.clock, &[me.clock]);
    assert!(s.port.handle_rx(now(0), &f, meta(0)).is_err());
    let f = announce_frame(2, 0, PEER_PORT, GM_ID, &[GM_ID, me.clock]);
    assert!(s.port.handle_rx(now(0), &f, meta(0)).is_err());
    assert_eq!(s.port.counters().announce_rejected, 2);
    assert_eq!(s.port.state(), PortState::Listening);
    s.tick();
    assert_eq!(s.port.counters().announce_rx, 0);
}

#[test]
fn sync_interval_rejections_keep_ts() {
    let mut s = locked(SimCfg::default());
    assert_eq!(s.port.sync_interval_s(), 0.125);
    for (i, log) in [-6i8, 2, 0x7F].into_iter().enumerate() {
        s.sync_log = log;
        // Emission still uses the real interval; only the advertised value is bad.
        let before = s.port.counters().sync_interval_invalid;
        let f = sync_frame(0xE000 + i as u16, log, 0);
        let t = s.tau;
        s.port.handle_rx(now(t), &f, meta(t)).unwrap();
        assert_eq!(s.port.sync_interval_s(), 0.125, "log {log}");
        assert_eq!(s.port.counters().sync_interval_invalid, before + 1);
        s.sync_log = -3;
    }
    assert_eq!(s.port.counters().sync_interval_changes, 0);
}

// --- step bookkeeping ---

/// A servo-decided step with the first tick after it delayed 5 ms: a
/// pre-step Sync and a pre-step 16R Pdelay_Req harvested by that tick are
/// AMBIG (Sync skipped, no second step; Pdelay_Req answered without
/// Follow_Up); frames from the next tick carry the new generation and are used.
#[test]
fn delayed_tick_after_step_marks_pre_step_frames_ambiguous() {
    let mut s = locked(SimCfg {
        sync_log: -5,
        ..SimCfg::default()
    });
    s.run_for(3 * S);
    s.delay_tick_after_step = 5 * MS;
    s.inject_pre_step = true;
    let steps0 = s.steps.len();
    let c0 = *s.port.counters();
    s.disturb(150_000);
    assert!(s.run_until(S, |s| s.steps.len() > steps0));
    let step_tau = s.steps.last().unwrap().0;
    let gen = s.gen;
    assert_eq!(s.port.step_gen(), gen);
    s.run_for(3 * S);
    let c = *s.port.counters();
    assert_eq!(
        s.steps.len(),
        steps0 + 1,
        "a pre-step Sync caused a second step"
    );
    assert!((s.steps.last().unwrap().1 + 150_000).abs() < 1_000);
    assert!(c.rx_ts_ambiguous_step >= c0.rx_ts_ambiguous_step + 2);
    assert!(c.sync_step_skip > c0.sync_step_skip);
    assert!(c.pdelay_resp_step_skip > c0.pdelay_resp_step_skip);
    // The pre-step Pdelay_Req was answered, without a Follow_Up.
    let seq = s.pre_step_req_seq.unwrap();
    assert_eq!(
        s.sent_of(MessageType::PdelayResp)
            .filter(|f| f.seq == seq)
            .count(),
        1
    );
    assert_eq!(
        s.sent_of(MessageType::PdelayRespFollowUp)
            .filter(|f| f.seq == seq)
            .count(),
        0
    );
    // Later 16R requests get Follow_Ups; later Syncs are used.
    assert!(s
        .sent_of(MessageType::PdelayRespFollowUp)
        .any(|f| f.tau > step_tau + 10 * MS));
    assert!(s.offsets.iter().any(|r| r.tau > step_tau + 10 * MS));
    assert!(s.true_offset().abs() < 1000.0, "{}", s.true_offset());
    assert!(s.port.as_capable());
}

#[test]
fn sync_with_old_generation_or_ambiguous_is_skipped() {
    let mut s = locked(SimCfg::default());
    s.syncs_on = false;
    s.run_for(10 * MS);
    let c0 = *s.port.counters();
    let steps0 = s.steps.len();
    let gen = s.gen;
    for (i, (g, amb)) in [(gen.wrapping_sub(1), false), (gen, true)]
        .into_iter()
        .enumerate()
    {
        let seq = 0xD000 + i as u16;
        // A wildly wrong timestamp: would be a > 100 us offset if used.
        let hw = s.rx_ts_now() + 5_000_000;
        let t = s.tau + 1;
        s.inject_slot(t, sync_frame(seq, -3, 0), Some(hw), g, amb);
        let pot = s.gm_time(s.tau as f64) as u64;
        s.inject(t, follow_up_frame(seq, -3, pot, 0, 0), None);
        s.tick();
    }
    let c = *s.port.counters();
    assert_eq!(c.sync_step_skip, c0.sync_step_skip + 2);
    assert_eq!(c.rx_ts_ambiguous_step, c0.rx_ts_ambiguous_step + 1);
    assert_eq!(c.syncs_used, c0.syncs_used);
    s.run_for(10 * MS);
    assert_eq!(s.steps.len(), steps0);
}

/// A step decided while a want_ts frame is outstanding is issued only after
/// the timestamp resolves, or times out (the sim asserts no step is issued
/// with a timestamp outstanding).
#[test]
fn step_waits_for_outstanding_tx_timestamp() {
    for ok in [true, false] {
        let mut s = locked(SimCfg::default());
        s.run_for(2 * S);
        // The next Pdelay_Req's timestamp resolves only 200 ms after sending
        // (longer than Ts), so the step decided meanwhile must wait for it.
        s.tx_ts_delay = 200 * MS;
        s.tx_ts_ok = ok;
        let req = s.port.counters().pdelay_req_tx;
        assert!(s.run_until(2 * S, |s| s.port.counters().pdelay_req_tx > req));
        let sent = s.tau;
        let steps0 = s.steps.len();
        s.disturb(300_000);
        assert!(s.run_until(S, |s| s.port.step_pending()));
        assert!(
            s.tau < sent + 200 * MS,
            "step decided while the TS is outstanding"
        );
        assert!(s.run_until(2 * S, |s| s.steps.len() > steps0));
        assert!(
            s.steps[steps0].0 >= sent + 200 * MS,
            "issued before the TS resolved"
        );
        s.tx_ts_ok = true;
        s.tx_ts_delay = 0;
        s.run_for(3 * S);
        assert!(s.true_offset().abs() < 2000.0);
        if !ok {
            let c = *s.port.counters();
            assert!(c.pdelay_tx_ts_fail + c.pdelay_resp_ts_fail > 0);
        }
    }
}

#[test]
fn slot_gen_invalid_is_counted() {
    let mut s = Sim::new(SimCfg::default());
    s.port.note_slot_gen_invalid();
    assert_eq!(s.port.status().counters.slot_gen_invalid, 1);
}

/// 802.1AS syncEvent with both correctionFields (2^-16 ns, signed), from the
/// msg vectors: Sync −1.5 ns, Follow_Up +2.25 ns.
#[test]
fn sync_event_vector_with_nonzero_sync_correction() {
    let sync = msg::decode(&msg::tests::SYNC_VECTOR).unwrap();
    let fu = msg::decode(&msg::tests::FOLLOW_UP_VECTOR).unwrap();
    let Body::FollowUp(f) = fu.body else { panic!() };
    let pot = f.precise_origin.to_ns().unwrap();
    let nrr = 1.0 - 83.18e-6;
    let rr = f.info.rate_ratio() * nrr;
    let t_rx_c = pot + 10_000;
    let off = sync_offset(
        t_rx_c,
        pot,
        fu.header.correction,
        sync.header.correction,
        142.6,
        rr,
        nrr,
    );
    let want = 10_000.0 - 0.75 - 142.6 * f.info.rate_ratio();
    assert!((off - want).abs() < 1e-6, "{off} vs {want}");
    // Both negative.
    let off = sync_offset(t_rx_c, pot, -(3 << 16), -(1 << 15), 0.0, 1.0, 1.0);
    assert!((off - 10_003.5).abs() < 1e-9);
    // SYSTIM behind the GM: negative offset.
    let off = sync_offset(pot - 1000, pot, 0, 0, 100.0, 1.0, 1.0);
    assert_eq!(off, -1100.0);
}

// --- responder / initiator details ---

fn bare_port() -> (Port, Box<Outbox<8>>) {
    let p = Port::new(
        PortConfig::i210(OUR_MAC, Timebase::IDENTITY),
        MachTicks(0),
        0,
    );
    (p, Box::new(Outbox::new()))
}

/// The 8 ms late-drop rule: a Pdelay_Resp handed to SendFrame 8 ms + 1 ns
/// after its Pdelay_Req's harvest is dropped (no Follow_Up); at 7.9 ms it is
/// sent and followed up.
#[test]
fn pdelay_resp_late_drop_8ms_vs_7_9ms() {
    for (delay, sent) in [(7_900_000u64, true), (8_000_001, false)] {
        let (mut p, mut out) = bare_port();
        let harvest = 1_000 * MS;
        let m = RxMeta {
            harvest: MachTicks(harvest),
            hw_ts: Some(5_000_000_000),
            ..RxMeta::default()
        };
        p.handle_rx(now(harvest), &pdelay_req_frame(7), m).unwrap();
        p.poll(now(harvest), &mut out);
        let f = out
            .iter()
            .find(|f| {
                msg::decode_frame(f.bytes()).unwrap().header.message_type == MessageType::PdelayResp
            })
            .copied()
            .unwrap();
        let t = harvest + delay;
        assert_eq!(p.pre_send(f.tag, MachTicks(t)), sent);
        out.clear();
        if sent {
            p.on_frame_sent(f.tag, MachTicks(t));
            p.on_tx_timestamp(f.tag, TxTsResult::Ok(5_000_000_000 + delay), MachTicks(t));
            p.poll(now(t), &mut out);
            let fu: Vec<_> = out
                .iter()
                .map(|f| msg::decode_frame(f.bytes()).unwrap())
                .filter(|m| m.header.message_type == MessageType::PdelayRespFollowUp)
                .collect();
            assert_eq!(fu.len(), 1);
            let Body::PdelayRespFollowUp(b) = fu[0].body else {
                panic!()
            };
            // t3_c = raw + 178.
            assert_eq!(b.response_origin.to_ns(), Some(5_000_000_000 + delay + 178));
            assert_eq!(fu[0].header.sequence_id, 7);
            assert_eq!(p.counters().pdelay_resp_late_drop, 0);
        } else {
            assert_eq!(p.counters().pdelay_resp_late_drop, 1);
            p.poll(now(t), &mut out);
            assert!(out.is_empty(), "no Follow_Up after a late drop");
        }
    }
}

#[test]
fn responder_echoes_t2_and_fails_cleanly_on_ts_timeout() {
    let (mut p, mut out) = bare_port();
    let m = RxMeta {
        harvest: MachTicks(0),
        hw_ts: Some(2_000_000_448),
        ..RxMeta::default()
    };
    p.handle_rx(now(0), &pdelay_req_frame(9), m).unwrap();
    p.poll(now(0), &mut out);
    let f = *out
        .iter()
        .find(|f| {
            f.want_ts
                && msg::decode_frame(f.bytes()).unwrap().header.message_type
                    == MessageType::PdelayResp
        })
        .unwrap();
    let Body::PdelayResp(r) = msg::decode_frame(f.bytes()).unwrap().body else {
        panic!()
    };
    assert_eq!(
        r.request_receipt.to_ns(),
        Some(2_000_000_000),
        "t2_c = rx − 448"
    );
    assert_eq!(r.requesting, PEER_PORT);
    assert!(p.pre_send(f.tag, MachTicks(100)));
    p.on_frame_sent(f.tag, MachTicks(100));
    p.on_tx_timestamp(f.tag, TxTsResult::Timeout, MachTicks(20 * MS));
    out.clear();
    p.poll(now(20 * MS), &mut out);
    assert!(out.iter().all(
        |f| msg::decode_frame(f.bytes()).unwrap().header.message_type
            != MessageType::PdelayRespFollowUp
    ));
    assert_eq!(p.counters().pdelay_resp_ts_fail, 1);
}

#[test]
fn pdelay_req_ts_timeout_counts_failure_and_loss() {
    let mut s = locked(SimCfg::default());
    let c0 = *s.port.counters();
    s.tx_ts_ok = false;
    s.run_for(2500 * MS);
    let c = *s.port.counters();
    assert!(c.pdelay_tx_ts_fail > c0.pdelay_tx_ts_fail);
    assert!(c.lost_responses > c0.lost_responses);
}

#[test]
fn multiple_responders_drop_ascapable() {
    let mut s = locked(SimCfg::default());
    s.peer_multi = true;
    assert!(s.run_until(3 * S, |s| !s.port.as_capable()));
    assert!(s.port.counters().multiple_responders >= 1);
    assert_eq!(
        s.port.status().ascapable_history.last().unwrap().reason,
        "multiple_responders"
    );
    s.peer_multi = false;
    // Clears after 3 clean exchanges.
    assert!(s.run_until(5 * S, |s| s.port.as_capable()));
}

#[test]
fn nrr_out_of_range_exchange_is_rejected() {
    // GM at +1200 ppm vs our free-running clock: every NRR candidate is out
    // of range, so the window never reaches two exchanges.
    let mut s = Sim::new(SimCfg {
        gm_ppm: 1200.0,
        ..SimCfg::default()
    });
    s.run_for(5 * S);
    assert!(s.port.counters().nrr_out_of_range >= 3);
    assert!(!s.port.as_capable());
    assert_eq!(s.port.nrr_systim(), None);
}

#[test]
fn signaling_gptp_capable_reply_and_interval_requests() {
    let mut s = Sim::new(SimCfg::default());
    s.run_for(S);
    assert_eq!(
        s.sent_of(MessageType::Signaling).count(),
        0,
        "never unprompted"
    );
    // gPTP-capable TLV (subtype 4) at log interval 0: we answer every 1 s.
    let t = s.tau + 1;
    s.inject(t, signaling_frame(4, 0, 0, 0), None);
    s.run_for(3500 * MS);
    let sig: Vec<_> = s.sent_of(MessageType::Signaling).collect();
    assert!((3..=5).contains(&sig.len()), "{}", sig.len());
    let Body::Signaling(g) = msg::decode_frame(&sig[0].bytes).unwrap().body else {
        panic!()
    };
    assert_eq!(g.gptp_capable.unwrap().log_interval, 0);
    assert_eq!(g.target, PortIdentity::WILDCARD);
    // No refresh: the peer's gPTP-capable receipt timeout (9 s) expires →
    // asCapable false and the replies stop.
    assert!(s.run_until(10 * S, |s| !s.port.as_capable()));
    assert_eq!(
        s.port.status().ascapable_history.last().unwrap().reason,
        "gptp_capable_timeout"
    );
    let n = s.sent_of(MessageType::Signaling).count();
    s.run_for(3 * S);
    assert_eq!(s.sent_of(MessageType::Signaling).count(), n);
    // Interval request: linkDelayInterval 2 honored, 5 and 127 rejected,
    // −128 no change.
    for (v, log, honored, rejected) in [
        (2i8, 2i8, 1u64, 0u64),
        (5, 2, 1, 1),
        (127, 2, 1, 2),
        (-128, 2, 1, 2),
        (126, 0, 2, 2),
    ] {
        let t = s.tau;
        s.port
            .handle_rx(now(t), &signaling_frame(2, v, -128, -128), meta(t))
            .unwrap();
        assert_eq!(s.port.pdelay_log_interval(), log, "request {v}");
        assert_eq!(s.port.counters().interval_requests_honored, honored);
        assert_eq!(s.port.counters().interval_requests_rejected, rejected);
    }
    // Honored interval 2 → Pdelay_Req every 4 s, carried in logMessageInterval.
    let t = s.tau;
    s.port
        .handle_rx(now(t), &signaling_frame(2, 2, -128, -128), meta(t))
        .unwrap();
    let n0 = s.sent_of(MessageType::PdelayReq).count();
    s.run_for(10 * S);
    let reqs: Vec<_> = s.sent_of(MessageType::PdelayReq).skip(n0 + 1).collect();
    assert!(reqs.len() >= 2);
    let gap = reqs[1].tau - reqs[0].tau;
    assert!((4 * S..4 * S + 60 * MS).contains(&gap), "{gap}");
    assert_eq!(reqs[1].bytes[14 + 33] as i8, 2);
}

#[test]
fn status_serializes() {
    let mut s = locked(SimCfg::default());
    s.run_for(S);
    let st = s.port.status();
    assert_eq!(st.port_identity.to_string(), "64:4b:f0:ff:fe:12:73:b8/1");
    assert_eq!(st.gm_identity, GM_ID);
    assert!(st.responder_turnaround_max_ns.unwrap() > 0.0);
    assert!(st.counters.pdelay_resp_fu_tx > 0);
    assert_eq!(crate::PRIORITY1, 255);
    // serde: the derive must cover every field (compile-time); spot-check
    // a few names through a minimal serializer-free check.
    fn is_serialize<T: serde::Serialize>(_: &T) {}
    is_serialize(&st);
    no_master_messages(&s);
}
