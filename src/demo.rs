//! Embedded fixture scenarios for ZFS-less environments.
//!
//! Four scripted time-series cover the observation states zresmon exists
//! for: a resilver in progress, one finished cleanly, an error-heavy run,
//! and a faulted vdev. `--once` renders a mid/late frame; interactive demo
//! mode advances `tick` on every poll so rates/deltas behave exactly like
//! live data (the model only ever sees two samples and subtracts).

use crate::model::{
    Ereport, ErrKind, PoolSnapshot, ScanFunc, ScanState, ScanStats, VdevInfo, VdevState,
};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Scanning,
    Done,
    Errors,
    Faulted,
}

impl Scenario {
    /// Parse from the `--demo` CLI value.
    #[allow(dead_code)] // clap ValueEnum handles the CLI; kept for programmatic use
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "scanning" => Self::Scanning,
            "done" => Self::Done,
            "errors" => Self::Errors,
            "fault" => Self::Faulted,
            _ => return None,
        })
    }
}

fn base_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_760_000_000)
}

fn leaf(name: &str, state: VdevState, read: u64, write: u64, cksum: u64) -> VdevInfo {
    VdevInfo {
        name: name.to_string(),
        state,
        is_rebuild_target: false,
        read_err: read,
        write_err: write,
        checksum_err: cksum,
        rebuild_pct: None,
        children: Vec::new(),
    }
}

fn mirror(children: Vec<VdevInfo>) -> VdevInfo {
    VdevInfo {
        name: "mirror-0".to_string(),
        state: if children.iter().all(|c| c.state == VdevState::Online) {
            VdevState::Online
        } else {
            VdevState::Degraded
        },
        is_rebuild_target: false,
        read_err: 0,
        write_err: 0,
        checksum_err: 0,
        rebuild_pct: None,
        children,
    }
}

/// Produce the snapshot for `tick` (monotonic; each tick ≈ one poll).
///
/// Total simulated scan = 100 MiB examined over 10 ticks.
pub fn sample(scenario: Scenario, tick: u64) -> PoolSnapshot {
    let now = base_time() + Duration::from_secs(tick * 2);
    match scenario {
        Scenario::Scanning | Scenario::Errors => {
            let total: u64 = 100 * 1024 * 1024;
            let frac = f64::min(tick as f64 / 10.0, 1.0);
            let examined = (total as f64 * frac) as u64;
            let errors = match scenario {
                Scenario::Errors if tick >= 3 => (tick - 2) * 7,
                _ => 0,
            };
            // A replacement disk being rebuilt is a full member and shows
            // ONLINE (writes go to both sides). Drawing it Degraded would
            // make retry_needed true by definition — normal progress would
            // look like an alarm.
            let b_state = VdevState::Online;
            let mut b = leaf(
                "/dev/sdb",
                b_state,
                0,
                0,
                if scenario == Scenario::Errors && tick >= 3 {
                    (tick - 2) * 5
                } else {
                    0
                },
            );
            b.is_rebuild_target = tick < 10;
            b.rebuild_pct = Some(frac * 100.0);
            PoolSnapshot {
                name: "tank".into(),
                scan: Some(ScanStats {
                    func: ScanFunc::Resilver,
                    state: if tick >= 10 {
                        ScanState::Finished
                    } else {
                        ScanState::Scanning
                    },
                    start_time: Some(base_time()),
                    end_time: None,
                    to_examine: total,
                    examined,
                    processed: examined,
                    skipped: 0,
                    errors,
                    issued: examined,
                    pass_exam: examined / 2,
                    sampled_at: now,
                    progress_override: None,
                }),
                root: VdevInfo {
                    name: "tank".into(),
                    state: if errors > 0 || b_state != VdevState::Online {
                        VdevState::Degraded
                    } else {
                        VdevState::Online
                    },
                    is_rebuild_target: false,
                    read_err: 0,
                    write_err: 0,
                    checksum_err: 0,
                    rebuild_pct: None,
                    children: vec![mirror(vec![
                        leaf("/dev/sda", VdevState::Online, 0, 0, 0),
                        b,
                    ])],
                },
                sampled_at: now,
            }
        }
        Scenario::Done => PoolSnapshot {
            name: "tank".into(),
            scan: Some(ScanStats {
                func: ScanFunc::Resilver,
                state: ScanState::Finished,
                start_time: Some(base_time()),
                end_time: Some(base_time() + Duration::from_secs(20)),
                to_examine: 100 * 1024 * 1024,
                examined: 100 * 1024 * 1024,
                processed: 100 * 1024 * 1024,
                skipped: 0,
                errors: 0,
                issued: 100 * 1024 * 1024,
                pass_exam: 0,
                sampled_at: now,
                progress_override: None,
            }),
            root: VdevInfo {
                name: "tank".into(),
                state: VdevState::Online,
                is_rebuild_target: false,
                read_err: 0,
                write_err: 0,
                checksum_err: 0,
                rebuild_pct: None,
                children: vec![mirror(vec![
                    leaf("/dev/sda", VdevState::Online, 0, 0, 0),
                    leaf("/dev/sdb", VdevState::Online, 0, 0, 0),
                ])],
            },
            sampled_at: now,
        },
        Scenario::Faulted => PoolSnapshot {
            name: "tank".into(),
            scan: None, // fault happened outside any scan — absence, not error
            root: VdevInfo {
                name: "tank".into(),
                state: VdevState::Degraded,
                is_rebuild_target: false,
                read_err: 0,
                write_err: 0,
                checksum_err: 0,
                rebuild_pct: None,
                children: vec![mirror(vec![
                    leaf("/dev/sda", VdevState::Online, 12, 3, 40),
                    leaf("/dev/sdb", VdevState::Faulted, 900, 120, 4_500),
                ])],
            },
            sampled_at: now,
        },
    }
}

/// Ereports that accompany the `errors` scenario at the given tick.
pub fn ereports(scenario: Scenario, tick: u64) -> Vec<Ereport> {
    if scenario != Scenario::Errors || tick < 3 {
        return Vec::new();
    }
    (0..tick.saturating_sub(2))
        .map(|i| Ereport {
            vdev_path: "/dev/sdb".into(),
            vdev_guid: 0x0BADC0DE_00000001,
            // Spread across the first half of a 512 GiB device.
            io_offset: i * 4 * 1024 * 1024 * 1024,
            io_size: 128 * 1024,
            kind: if i % 3 == 0 {
                ErrKind::IoRead
            } else {
                ErrKind::Checksum
            },
            ts: base_time() + Duration::from_secs(tick * 2),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::retry_needed;

    #[test]
    fn scanning_progresses_and_converges() {
        for tick in 0..=10u64 {
            let s = sample(Scenario::Scanning, tick);
            let scan = s.scan.as_ref().unwrap();
            assert_eq!(scan.examined, scan.to_examine.min(tick * 10 * 1024 * 1024));
            assert!(
                !retry_needed(&s),
                "scanning w/o errors must not demand retry"
            );
        }
        let done = sample(Scenario::Scanning, 10);
        assert_eq!(done.scan.unwrap().state, ScanState::Finished);
    }

    #[test]
    fn errors_scenario_marks_retry_until_finished() {
        // Mid-scan with errors → retry needed.
        let mid = sample(Scenario::Errors, 6);
        assert!(mid.scan.as_ref().unwrap().errors > 0);
        assert!(retry_needed(&mid));
        // After finish the scan did all it will do → retry flag clears.
        let fin = sample(Scenario::Errors, 12);
        assert_eq!(fin.scan.as_ref().unwrap().state, ScanState::Finished);
        assert!(!retry_needed(&fin));
        assert!(!ereports(Scenario::Errors, 6).is_empty());
    }

    #[test]
    fn fault_scenario_needs_escalation_not_retry() {
        let s = sample(Scenario::Faulted, 0);
        assert!(s.scan.is_none());
        // Retrying against a FAULTED disk is pointless — replace first (model.rs narrow rule).
        // The TUI surfaces it separately via vdev state color (red).
        assert!(!retry_needed(&s));
        assert_eq!(s.root.children[0].children[1].state, VdevState::Faulted);
        let faulted = &s.root.children[0].children[1];
        assert_eq!(faulted.name, "/dev/sdb");
        assert_eq!(faulted.checksum_err, 4_500);
    }

    #[test]
    fn done_is_clean() {
        let s = sample(Scenario::Done, 0);
        assert!(!retry_needed(&s));
    }
}
