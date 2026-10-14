// SPDX-License-Identifier: MIT OR Apache-2.0

//! BTCA: the Best time-aware system Clock Algorithm (IEEE 802.1AS-2020
//! Clause 10.3), which is 1588's BMCA with the dataset comparison restricted
//! to what gPTP carries.
//!
//! Milan v1.3 §4.3.1.1 requires a Talker PAAD to be *"capable of operating as
//! a gPTP grandmaster"*, and a port cannot be a grandmaster candidate without
//! this: becoming master is the outcome of OUR OWN clock dataset winning the
//! comparison against every Announce we receive. Until now `rx_announce`
//! accepted any Announce and went straight to Slave, so our clock was never
//! a candidate at all.
//!
//! Pure and sans-IO, like the rest of this crate: [`compare`] is a total
//! order over [`ClockDataset`] and does no I/O, holds no state, and reads no
//! clock. That is what makes the ladder testable, and the ladder is where a
//! subtle mistake would otherwise show up as two devices fighting over which
//! is grandmaster while audio is running.

use crate::msg::{ClockIdentity, ClockQuality, PortIdentity};
use core::cmp::Ordering;

/// The announced clock dataset a port compares: either the one we would
/// announce ourselves, or the one decoded from a received Announce.
///
/// `steps_removed` and `sender` matter only for the tiebreak between two
/// announcements of THE SAME grandmaster arriving by different paths; the
/// fields above them decide everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockDataset {
    pub priority1: u8,
    pub quality: ClockQuality,
    pub priority2: u8,
    pub identity: ClockIdentity,
    pub steps_removed: u16,
    /// `sourcePortIdentity` of the Announce, or our own port for our dataset.
    pub sender: PortIdentity,
}

/// Which of two datasets is the better clock.
///
/// [`Ordering::Less`] means `a` is BETTER — the comparison is "smaller value
/// wins" at every rung, which is 1588's convention and the opposite of the
/// intuitive reading. A grandmaster with `priority1 = 248` beats one with
/// `255`.
///
/// The ladder (802.1AS Clause 10.3, 1588 §9.3.4), in order, stopping at the
/// first difference:
///
/// 1. `grandmasterPriority1`
/// 2. `grandmasterClockQuality.clockClass`
/// 3. `grandmasterClockQuality.clockAccuracy`
/// 4. `grandmasterClockQuality.offsetScaledLogVariance`
/// 5. `grandmasterPriority2`
/// 6. `grandmasterIdentity`
///
/// Only when the grandmaster IDENTITIES are equal — the same grandmaster
/// reached two ways — do `stepsRemoved` and then the sending port decide. A
/// comparison that reached `stepsRemoved` with different identities would be
/// preferring a topologically closer clock over a better one, which inverts
/// the whole point of the algorithm.
pub fn compare(a: &ClockDataset, b: &ClockDataset) -> Ordering {
    // Rungs 1-6: the grandmaster's own merit.
    let merit = a
        .priority1
        .cmp(&b.priority1)
        .then(a.quality.clock_class.cmp(&b.quality.clock_class))
        .then(a.quality.clock_accuracy.cmp(&b.quality.clock_accuracy))
        .then(
            a.quality
                .offset_scaled_log_variance
                .cmp(&b.quality.offset_scaled_log_variance),
        )
        .then(a.priority2.cmp(&b.priority2))
        .then(a.identity.0.cmp(&b.identity.0));
    if merit != Ordering::Equal {
        return merit;
    }
    // Same grandmaster, different paths: prefer fewer hops, then the lower
    // sending port identity as the final deterministic tiebreak. Without the
    // last rung two paths of equal length would compare Equal and the caller
    // could oscillate between them.
    a.steps_removed
        .cmp(&b.steps_removed)
        .then(a.sender.clock.0.cmp(&b.sender.clock.0))
        .then(a.sender.port.cmp(&b.sender.port))
}

/// Whether `ours` should take the master role over `theirs`.
pub fn we_are_better(ours: &ClockDataset, theirs: &ClockDataset) -> bool {
    compare(ours, theirs) == Ordering::Less
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn qual(class: u8, acc: u8, var: u16) -> ClockQuality {
        ClockQuality {
            clock_class: class,
            clock_accuracy: acc,
            offset_scaled_log_variance: var,
        }
    }

    fn ident(b: u8) -> ClockIdentity {
        ClockIdentity([b, 0, 0, 0, 0, 0, 0, 0])
    }

    fn port(b: u8, p: u16) -> PortIdentity {
        PortIdentity {
            clock: ident(b),
            port: p,
        }
    }

    fn ds(p1: u8, class: u8, acc: u8, var: u16, p2: u8, id: u8) -> ClockDataset {
        ClockDataset {
            priority1: p1,
            quality: qual(class, acc, var),
            priority2: p2,
            identity: ident(id),
            steps_removed: 0,
            sender: port(id, 1),
        }
    }

    /// The real pairing this exists for: the 16R announces priority1 248 and
    /// we currently announce 255, so BTCA must keep us Slave. This is what
    /// makes it safe to reach the Master state before raising our priority.
    #[test]
    fn the_16r_beats_us_while_our_priority1_is_255() {
        let r16 = ds(248, 248, 0x20, 0x436a, 248, 0x0a);
        let us = ds(255, 248, 0x20, 0x436a, 255, 0x64);
        assert!(!we_are_better(&us, &r16), "we must not win with 255");
        assert!(we_are_better(&r16, &us));
    }

    /// And at the Milan value we become a genuine candidate, decided below
    /// priority1 rather than by it.
    #[test]
    fn at_priority1_248_the_tie_falls_through_to_later_rungs() {
        let r16 = ds(248, 248, 0x20, 0x436a, 248, 0x0a);
        // Identical merit except a lower identity, which is rung 6.
        let us = ds(248, 248, 0x20, 0x436a, 248, 0x01);
        assert!(we_are_better(&us, &r16));
        // A WORSE clockClass loses even with the lower identity, because
        // class is rung 2 and identity is rung 6.
        let worse_class = ds(248, 250, 0x20, 0x436a, 248, 0x01);
        assert!(!we_are_better(&worse_class, &r16));
    }

    /// Each rung must be decisive on its own, and must not be reachable when
    /// an earlier rung already differs. Built as a table so a reordered
    /// ladder fails rather than quietly preferring the wrong clock.
    #[test]
    fn every_rung_decides_in_order() {
        let base = ds(248, 248, 0x20, 0x436a, 248, 0x10);
        // (better, worse, which rung)
        let cases: [(ClockDataset, ClockDataset, &str); 6] = [
            (ds(247, 255, 0xFE, 0xFFFF, 255, 0xFF), base, "priority1"),
            (ds(248, 247, 0xFE, 0xFFFF, 255, 0xFF), base, "clockClass"),
            (ds(248, 248, 0x1F, 0xFFFF, 255, 0xFF), base, "clockAccuracy"),
            (ds(248, 248, 0x20, 0x4369, 255, 0xFF), base, "variance"),
            (ds(248, 248, 0x20, 0x436a, 247, 0xFF), base, "priority2"),
            (ds(248, 248, 0x20, 0x436a, 248, 0x0F), base, "identity"),
        ];
        for (better, worse, rung) in cases {
            assert!(
                we_are_better(&better, &worse),
                "{rung} must decide: every earlier rung is equal and this one is lower"
            );
            assert!(!we_are_better(&worse, &better), "{rung} must be asymmetric");
        }
    }

    /// `stepsRemoved` is for the SAME grandmaster reached two ways. It must
    /// never override merit: a closer but worse clock loses.
    #[test]
    fn steps_removed_never_beats_merit() {
        let far_and_good = ClockDataset {
            steps_removed: 7,
            ..ds(248, 248, 0x20, 0x436a, 248, 0x10)
        };
        let near_and_bad = ClockDataset {
            steps_removed: 0,
            ..ds(255, 248, 0x20, 0x436a, 248, 0x20)
        };
        assert!(
            we_are_better(&far_and_good, &near_and_bad),
            "a better clock seven hops away still wins"
        );
    }

    /// Same grandmaster, two paths: now stepsRemoved decides, and the sending
    /// port breaks an exact tie so the caller cannot oscillate.
    #[test]
    fn same_grandmaster_two_paths_resolves_deterministically() {
        let near = ClockDataset {
            steps_removed: 1,
            sender: port(0x30, 1),
            ..ds(248, 248, 0x20, 0x436a, 248, 0x10)
        };
        let far = ClockDataset {
            steps_removed: 2,
            sender: port(0x20, 1),
            ..ds(248, 248, 0x20, 0x436a, 248, 0x10)
        };
        assert!(we_are_better(&near, &far), "fewer hops to the same GM");

        let a = ClockDataset {
            sender: port(0x20, 1),
            ..near
        };
        let b = ClockDataset {
            sender: port(0x30, 1),
            ..near
        };
        assert_eq!(compare(&a, &b), Ordering::Less, "lower sender wins");
        assert_eq!(compare(&b, &a), Ordering::Greater);

        // Identical sender clock, different port numbers: the last rung.
        let p1 = ClockDataset {
            sender: port(0x20, 1),
            ..near
        };
        let p2 = ClockDataset {
            sender: port(0x20, 2),
            ..near
        };
        assert_eq!(compare(&p1, &p2), Ordering::Less);
    }

    /// A dataset compares Equal only to itself, and `we_are_better` is false
    /// for an exact tie — otherwise a port would claim master against its own
    /// announcement echoed back.
    #[test]
    fn a_dataset_ties_only_with_itself() {
        let d = ds(248, 248, 0x20, 0x436a, 248, 0x10);
        assert_eq!(compare(&d, &d), Ordering::Equal);
        assert!(!we_are_better(&d, &d));
    }

    /// Antisymmetry and transitivity across the whole table, so the result is
    /// a real total order rather than a ladder that happens to work pairwise.
    #[test]
    fn compare_is_a_total_order() {
        let all = [
            ds(247, 248, 0x20, 0x436a, 248, 0x10),
            ds(248, 247, 0x20, 0x436a, 248, 0x10),
            ds(248, 248, 0x1F, 0x436a, 248, 0x10),
            ds(248, 248, 0x20, 0x4369, 248, 0x10),
            ds(248, 248, 0x20, 0x436a, 247, 0x10),
            ds(248, 248, 0x20, 0x436a, 248, 0x0F),
            ds(255, 255, 0xFE, 0xFFFF, 255, 0xFF),
        ];
        for a in &all {
            for b in &all {
                assert_eq!(compare(a, b).reverse(), compare(b, a), "antisymmetry");
                for c in &all {
                    if compare(a, b) == Ordering::Less && compare(b, c) == Ordering::Less {
                        assert_eq!(compare(a, c), Ordering::Less, "transitivity");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod hardware_outcome {
    use super::*;
    use crate::msg::{ClockIdentity, PortIdentity};

    /// The 16R's announced dataset, transcribed from the captured Announce in
    /// `docs/gptp-master-capture.md` (octets 47-60), not from a spec table.
    fn studiolive_16r() -> ClockDataset {
        let id = ClockIdentity([0x00, 0x0a, 0x92, 0xff, 0xfe, 0x03, 0x3b, 0xa9]);
        ClockDataset {
            priority1: 248,
            quality: crate::msg::ClockQuality {
                clock_class: 248,
                clock_accuracy: 0x20,
                offset_scaled_log_variance: 0x436a,
            },
            priority2: 248,
            identity: id,
            steps_removed: 0,
            sender: PortIdentity { clock: id, port: 1 },
        }
    }

    /// Our dataset as the live daemon announces it: the I210's MAC
    /// 64:4b:f0:12:73:b8 as an EUI-64, with PRIORITY1 and our_clock_quality.
    fn ours() -> ClockDataset {
        let id = ClockIdentity::from_mac(avb_core::MacAddr([0x64, 0x4b, 0xf0, 0x12, 0x73, 0xb8]));
        ClockDataset {
            priority1: crate::PRIORITY1,
            quality: crate::our_clock_quality(),
            priority2: crate::PRIORITY2,
            identity: id,
            steps_removed: 0,
            sender: PortIdentity { clock: id, port: 1 },
        }
    }

    /// What raising PRIORITY1 to 248 actually does on this link.
    ///
    /// Every merit rung is now a TIE -- the two datasets are identical
    /// through priority1, clockClass, clockAccuracy, variance and priority2,
    /// because our_clock_quality() was transcribed from this very device. So
    /// the comparison falls through to grandmasterIdentity, where the lower
    /// value wins: 000a92.fffe.033ba9 beats our 644bf0.fffe.1273b8.
    ///
    /// The 16R therefore KEEPS the clock, and the flip cannot disturb a
    /// running stream on this link. This is the test that makes the flip
    /// safe to ship rather than a thing to find out on hardware.
    #[test]
    fn against_the_real_16r_we_contend_and_lose_on_identity_alone() {
        let us = ours();
        let them = studiolive_16r();

        assert_eq!(us.priority1, them.priority1, "rung 1 ties");
        assert_eq!(us.quality, them.quality, "rungs 2-4 tie");
        assert_eq!(us.priority2, them.priority2, "rung 5 ties");
        assert!(
            them.identity.0 < us.identity.0,
            "the 16R's identity must be the lower one"
        );

        assert!(!we_are_better(&us, &them), "the 16R keeps the clock");
        assert!(we_are_better(&them, &us), "and strictly so");
    }

    /// The same contest against a device whose only difference is a HIGHER
    /// clockIdentity, which we must win. Without this the test above could
    /// pass for a port that can never win anything.
    #[test]
    fn against_an_otherwise_identical_device_with_a_higher_identity_we_win() {
        let us = ours();
        let mut them = studiolive_16r();
        them.identity = ClockIdentity([0xff, 0xee, 0xdd, 0xff, 0xfe, 0x01, 0x02, 0x03]);
        them.sender.clock = them.identity;
        assert!(we_are_better(&us, &them));
    }

    /// Milan v1.3 §4.2.6.2.1: a grandmaster-capable PAAD announces 248. A
    /// regression to 255 would silently remove us from contention on every
    /// link, which no other test in this file would notice.
    #[test]
    fn our_priority1_is_the_milan_grandmaster_capable_value() {
        assert_eq!(crate::PRIORITY1, 248);
    }
}
