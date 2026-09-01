//! Snapshot data model for ZFS pool scan monitoring.
//!
//! Everything here is a *sample*: an immutable, timestamped view of kernel
//! state at one instant. There is no cross-sample state in the model itself;
//! rate/ETA math belongs to whoever holds two samples and subtracts.
//!
//! Field names mirror the `/proc/spl/kstat/zfs/<pool>/scan` kstat rows
//! (`func`, `state`, `to_examine`, `examined`, `processed`, `skipped`,
//! `errors`, `issued`, `pass_exam`, ...) and `zpool status -v` vdev states,
//! so a collector can populate these structs almost mechanically.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// What kind of scan the pool is running (kstat `func` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanFunc {
    Resilver,
    Scrub,
    ErrorScrub,
}

/// Lifecycle state of the scan (kstat `state` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    /// No scan has been requested since boot/import.
    Idle,
    /// A resilver or scrub is actively running.
    Scanning,
    /// The last scan completed normally.
    Finished,
    /// The last scan was canceled before completing.
    Canceled,
}

/// One sample of the pool-wide scan kstats. All counters are cumulative for
/// the current scan; [`ScanStats::sampled_at`] says when this sample was
/// taken so deltas between two samples yield rates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanStats {
    pub func: ScanFunc,
    pub state: ScanState,
    /// When the scan started; `None` while no scan is running or recorded.
    pub start_time: Option<SystemTime>,
    /// When the scan ended; `None` until it actually ends (never overloaded
    /// to mean "still running" — that is what [`ScanState::Scanning`] says).
    pub end_time: Option<SystemTime>,
    pub to_examine: u64,
    pub examined: u64,
    pub processed: u64,
    pub skipped: u64,
    pub errors: u64,
    pub issued: u64,
    pub pass_exam: u64,
    /// Timestamp of THIS sample. Rates are computed between samples.
    pub sampled_at: SystemTime,
    /// Progress ratio known independently of byte counters — set by the
    /// status-line fallback when a `% done` wording exists but byte totals
    /// do not. `progress()` prefers this over the (absent) counters, and the
    /// bytes display stays empty rather than showing synthesized numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_override: Option<f64>,
}

impl ScanStats {
    /// Progress ratio in `[0.0, 1.0]`, or `None` when not computable.
    ///
    /// This is the ONLY sanctioned source of a progress ratio: it guards
    /// against division-by-zero (empty pool → NaN) and clamps out-of-range
    /// counters so display code can never render a NaN progress bar.
    #[must_use]
    pub fn progress(&self) -> Option<f64> {
        if let Some(r) = self.progress_override {
            return Some(r.clamp(0.0, 1.0));
        }
        if self.to_examine == 0 {
            return None;
        }
        let ratio = self.examined as f64 / self.to_examine as f64;
        if !ratio.is_finite() {
            return None;
        }
        Some(ratio.clamp(0.0, 1.0))
    }
}

/// Health of one vdev as reported by `zpool status -v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VdevState {
    Online,
    Degraded,
    Faulted,
    Offline,
    Removed,
    Unavail,
}

impl VdevState {
    /// Tab-bar grading rank: 0 = healthy, 1 = degraded, 2 = fault.
    ///
    /// `Offline` grades healthy on purpose — taking a vdev offline is an
    /// intentional operator action, not a failure the pool tab should
    /// alarm on (mirrors `zpool status`, which does not fault a pool for
    /// an offline vdev either).
    #[must_use]
    pub fn tab_severity(self) -> u8 {
        match self {
            VdevState::Online | VdevState::Offline => 0,
            VdevState::Degraded => 1,
            VdevState::Faulted | VdevState::Removed | VdevState::Unavail => 2,
        }
    }
}

/// Pool-wide health grade for the tab bar: the worst vdev state found in
/// the topology, mapped to a display tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolHealth {
    /// Every vdev Online (or Offline — intentional operator action).
    Healthy,
    /// Redundancy reduced but the pool keeps serving data.
    Degraded,
    /// Faulted/Unavail/Removed somewhere — needs replacement, not retry.
    Fault,
}

impl PoolHealth {
    /// Worst-of merge for the tree walk.
    #[must_use]
    pub fn worst(self, other: Self) -> Self {
        match (self, other) {
            (PoolHealth::Fault, _) | (_, PoolHealth::Fault) => PoolHealth::Fault,
            (PoolHealth::Degraded, _) | (_, PoolHealth::Degraded) => PoolHealth::Degraded,
            (PoolHealth::Healthy, PoolHealth::Healthy) => PoolHealth::Healthy,
        }
    }
}

/// One node in the pool's vdev tree. Interiors (mirror/raidz/root) carry
/// their children recursively; leaves are physical devices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VdevInfo {
    pub name: String,
    pub state: VdevState,
    /// True while this device is being resilvered into (or is the target of
    /// an in-progress rebuild). Drives per-vdev progress rendering.
    pub is_rebuild_target: bool,
    pub read_err: u64,
    pub write_err: u64,
    pub checksum_err: u64,
    /// Resilver/rebuild completion percentage for this device, if known.
    pub rebuild_pct: Option<f64>,
    pub children: Vec<VdevInfo>,
}

/// A complete, self-contained observation of one pool at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolSnapshot {
    pub name: String,
    /// `None` when the kstat reports no scan record (e.g. `state == Idle`
    /// with no historical row) — absence, not failure.
    pub scan: Option<ScanStats>,
    pub root: VdevInfo,
    pub sampled_at: SystemTime,
}

/// Kind of I/O error reported by a ZFS ereport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrKind {
    Checksum,
    IoRead,
    IoWrite,
}

/// A single ZFS ereport event (checksum / I/O error), used to correlate
/// recent failures with vdev health during a scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ereport {
    pub vdev_path: String,
    pub vdev_guid: u64,
    pub io_offset: u64,
    pub io_size: u64,
    pub kind: ErrKind,
    pub ts: SystemTime,
}

impl PoolSnapshot {
    /// Worst-case health over the whole vdev tree — interior nodes
    /// included: an UNAVAIL mirror/raid group matters even when every leaf
    /// beneath it reads FAULTED ("insufficient replicas"), exactly like
    /// [`retry_needed`]'s recoverability rule.
    ///
    /// Recomputed from this snapshot alone — there is no cached state — so
    /// a recovered pool (e.g. after `zpool replace` + resilver finishes)
    /// reverts to [`PoolHealth::Healthy`] on the next poll automatically.
    #[must_use]
    pub fn health(&self) -> PoolHealth {
        tree_health(&self.root)
    }
}

fn tree_health(vdev: &VdevInfo) -> PoolHealth {
    let own = match vdev.state.tab_severity() {
        0 => PoolHealth::Healthy,
        1 => PoolHealth::Degraded,
        _ => PoolHealth::Fault,
    };
    vdev.children
        .iter()
        .fold(own, |acc, c| acc.worst(tree_health(c)))
}

/// RetryPolicy judgment: does this snapshot warrant another monitor cycle /
/// repair attempt?
///
/// True when either:
/// * a scan is recorded with `errors > 0` and its state is NOT
///   [`ScanState::Finished`] (a finished-with-errors scan needs no retry —
///   the scan already did all it will do), or
/// * any **leaf** vdev in the topology is [`VdevState::Degraded`] or
///   [`VdevState::Unavail`] (redundancy reduced but recoverable).
///
/// Deliberately NOT triggering on Faulted/Offline/Removed leaves: retrying
/// against a dead device accomplishes nothing — those need escalation
/// (replacement / `zpool online`) first, and the TUI surfaces them through
/// the vdev state color instead.
#[must_use]
pub fn retry_needed(snapshot: &PoolSnapshot) -> bool {
    if let Some(scan) = &snapshot.scan {
        if scan.errors > 0 && scan.state != ScanState::Finished {
            return true;
        }
    }
    tree_has_recoverable_leaf(&snapshot.root)
}

fn tree_has_recoverable_leaf(vdev: &VdevInfo) -> bool {
    // An UNAVAIL *interior* node (pool/raid group, "insufficient replicas")
    // means recovery is possible once enough leaves are replaced and
    // resilvered — even when every leaf itself reads FAULTED.
    if matches!(vdev.state, VdevState::Unavail) {
        return true;
    }
    if vdev.children.is_empty() {
        return matches!(vdev.state, VdevState::Degraded | VdevState::Unavail);
    }
    vdev.children.iter().any(tree_has_recoverable_leaf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn ts(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn leaf(name: &str, state: VdevState) -> VdevInfo {
        VdevInfo {
            name: name.to_string(),
            state,
            is_rebuild_target: false,
            read_err: 0,
            write_err: 0,
            checksum_err: 0,
            rebuild_pct: None,
            children: Vec::new(),
        }
    }

    fn mirror(children: Vec<VdevInfo>) -> VdevInfo {
        VdevInfo {
            name: "mirror-0".to_string(),
            state: VdevState::Online,
            is_rebuild_target: false,
            read_err: 0,
            write_err: 0,
            checksum_err: 0,
            rebuild_pct: None,
            children,
        }
    }

    fn scan(state: ScanState, errors: u64, to_examine: u64, examined: u64) -> ScanStats {
        ScanStats {
            func: ScanFunc::Resilver,
            state,
            start_time: Some(ts(100)),
            end_time: None,
            to_examine,
            examined,
            processed: 0,
            skipped: 0,
            errors,
            issued: 0,
            pass_exam: 0,
            sampled_at: ts(200),
            progress_override: None,
        }
    }

    fn snapshot(scan: Option<ScanStats>, root: VdevInfo) -> PoolSnapshot {
        PoolSnapshot {
            name: "tank".to_string(),
            scan,
            root,
            sampled_at: ts(300),
        }
    }

    #[test]
    fn clean_scan_healthy_pool_needs_no_retry() {
        let snap = snapshot(
            Some(scan(ScanState::Scanning, 0, 1000, 500)),
            mirror(vec![
                leaf("sda", VdevState::Online),
                leaf("sdb", VdevState::Online),
            ]),
        );
        assert!(!retry_needed(&snap));
    }

    #[test]
    fn scan_errors_while_scanning_need_retry() {
        let snap = snapshot(
            Some(scan(ScanState::Scanning, 3, 1000, 500)),
            mirror(vec![
                leaf("sda", VdevState::Online),
                leaf("sdb", VdevState::Online),
            ]),
        );
        assert!(retry_needed(&snap));
    }

    #[test]
    fn canceled_scan_with_errors_needs_retry() {
        let snap = snapshot(
            Some(scan(ScanState::Canceled, 1, 1000, 10)),
            mirror(vec![
                leaf("sda", VdevState::Online),
                leaf("sdb", VdevState::Online),
            ]),
        );
        assert!(retry_needed(&snap));
    }

    #[test]
    fn finished_scan_with_errors_does_not_retry_by_itself() {
        let snap = snapshot(
            Some(scan(ScanState::Finished, 7, 1000, 1000)),
            mirror(vec![
                leaf("sda", VdevState::Online),
                leaf("sdb", VdevState::Online),
            ]),
        );
        assert!(!retry_needed(&snap));
    }

    #[test]
    fn degraded_leaf_vdev_needs_retry() {
        let snap = snapshot(
            Some(scan(ScanState::Idle, 0, 0, 0)),
            mirror(vec![
                leaf("sda", VdevState::Online),
                leaf("sdb", VdevState::Degraded),
            ]),
        );
        assert!(retry_needed(&snap));
    }

    #[test]
    fn unavail_leaf_vdev_needs_retry() {
        let snap = snapshot(None, mirror(vec![leaf("sdd", VdevState::Unavail)]));
        assert!(retry_needed(&snap));
    }

    #[test]
    fn unavail_raid_group_with_faulted_leaves_is_recoverable() {
        // Real-world shape from a dead pool: pool UNAVAIL, raidz1 UNAVAIL
        // ("insufficient replicas"), every leaf FAULTED. Replacing enough
        // leaves + resilver WOULD recover it, so retry applies.
        let snap = snapshot(
            None,
            VdevInfo {
                name: "pool".into(),
                state: VdevState::Unavail,
                is_rebuild_target: false,
                read_err: 0,
                write_err: 0,
                checksum_err: 0,
                rebuild_pct: None,
                children: vec![VdevInfo {
                    name: "raidz1-0".into(),
                    state: VdevState::Unavail,
                    is_rebuild_target: false,
                    read_err: 0,
                    write_err: 0,
                    checksum_err: 0,
                    rebuild_pct: None,
                    children: vec![
                        leaf("d1", VdevState::Faulted),
                        leaf("d2", VdevState::Faulted),
                        leaf("d3", VdevState::Faulted),
                    ],
                }],
            },
        );
        assert!(retry_needed(&snap));
    }

    #[test]
    fn faulted_leaf_alone_does_not_trigger() {
        // Deliberate narrowness of the rule: only Degraded/Unavail leaves
        // count. Faulted/Offline/Removed leaves need escalation elsewhere.
        let snap = snapshot(
            Some(scan(ScanState::Idle, 0, 0, 0)),
            mirror(vec![leaf("sdz", VdevState::Faulted)]),
        );
        assert!(!retry_needed(&snap));
    }

    #[test]
    fn deep_tree_recursion_finds_nested_leaf() {
        let inner = VdevInfo {
            name: "mirror-1".to_string(),
            state: VdevState::Degraded,
            is_rebuild_target: false,
            read_err: 0,
            write_err: 0,
            checksum_err: 0,
            rebuild_pct: None,
            children: vec![
                leaf("nvme0", VdevState::Online),
                leaf("nvme1", VdevState::Unavail),
            ],
        };
        let root = VdevInfo {
            name: "tank".to_string(),
            state: VdevState::Degraded,
            is_rebuild_target: false,
            read_err: 0,
            write_err: 0,
            checksum_err: 0,
            rebuild_pct: None,
            children: vec![mirror(vec![leaf("sda", VdevState::Online)]), inner],
        };
        assert!(retry_needed(&snapshot(None, root)));
    }

    #[test]
    fn progress_guards_zero_denominator_to_none() {
        let s = scan(ScanState::Scanning, 0, 0, 0);
        assert_eq!(s.progress(), None);
    }

    #[test]
    fn progress_is_ratio_in_unit_range() {
        let mut s = scan(ScanState::Scanning, 0, 1000, 500);
        assert_eq!(s.progress(), Some(0.5));
        // Out-of-range counters clamp instead of leaking >1.0 into bars.
        s.examined = 1500;
        assert_eq!(s.progress(), Some(1.0));
    }

    #[test]
    fn snapshot_roundtrips_through_json() {
        let snap = snapshot(
            Some(scan(ScanState::Scanning, 2, 1000, 250)),
            mirror(vec![
                leaf("sda", VdevState::Online),
                leaf("sdb", VdevState::Degraded),
            ]),
        );
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: PoolSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, snap);
    }

    #[test]
    fn finished_clean_with_degraded_leaf_still_true() {
        // The observed VM case: scan finished clean, but a vdev was left
        // DEGRADED (zinject "too many errors") — retry stays true because the
        // leaf is recoverable (Degraded, not Faulted).
        let snap = snapshot(
            Some(scan(ScanState::Finished, 0, 0, 0)),
            mirror(vec![leaf("sdz", VdevState::Degraded)]),
        );
        assert!(retry_needed(&snap));
    }

    #[test]
    fn health_grades_each_vdev_state() {
        let grade = |state| {
            snapshot(
                Some(scan(ScanState::Idle, 0, 0, 0)),
                mirror(vec![leaf("sda", VdevState::Online), leaf("sdb", state)]),
            )
            .health()
        };
        assert_eq!(grade(VdevState::Online), PoolHealth::Healthy);
        // Offline is an intentional operator action — not a tab alarm.
        assert_eq!(grade(VdevState::Offline), PoolHealth::Healthy);
        assert_eq!(grade(VdevState::Degraded), PoolHealth::Degraded);
        assert_eq!(grade(VdevState::Faulted), PoolHealth::Fault);
        assert_eq!(grade(VdevState::Removed), PoolHealth::Fault);
        assert_eq!(grade(VdevState::Unavail), PoolHealth::Fault);
    }

    #[test]
    fn unavail_interior_with_faulted_leaves_grades_fault() {
        // Same dead-pool shape the retry policy tests: the raid group is
        // UNAVAIL ("insufficient replicas") while every leaf reads FAULTED.
        // The tab must scream red even though no single leaf is Unavail.
        let snap = snapshot(
            None,
            VdevInfo {
                name: "pool".into(),
                state: VdevState::Unavail,
                is_rebuild_target: false,
                read_err: 0,
                write_err: 0,
                checksum_err: 0,
                rebuild_pct: None,
                children: vec![VdevInfo {
                    name: "raidz1-0".into(),
                    state: VdevState::Unavail,
                    is_rebuild_target: false,
                    read_err: 0,
                    write_err: 0,
                    checksum_err: 0,
                    rebuild_pct: None,
                    children: vec![
                        leaf("d1", VdevState::Faulted),
                        leaf("d2", VdevState::Faulted),
                    ],
                }],
            },
        );
        assert_eq!(snap.health(), PoolHealth::Fault);
    }

    #[test]
    fn health_worst_wins_across_nested_groups() {
        // Healthy mirror + degraded mirror in one root → Degraded; adding
        // a faulted leaf anywhere → Fault (severity is monotonic).
        let mk_root = |worst_leaf: VdevInfo| {
            snapshot(
                None,
                VdevInfo {
                    name: "tank".to_string(),
                    state: VdevState::Online,
                    is_rebuild_target: false,
                    read_err: 0,
                    write_err: 0,
                    checksum_err: 0,
                    rebuild_pct: None,
                    children: vec![mirror(vec![leaf("sda", VdevState::Online)]), worst_leaf],
                },
            )
        };
        assert_eq!(
            mk_root(mirror(vec![leaf("sdb", VdevState::Degraded)])).health(),
            PoolHealth::Degraded
        );
        assert_eq!(
            mk_root(mirror(vec![leaf("sdb", VdevState::Faulted)])).health(),
            PoolHealth::Fault
        );
    }

    #[test]
    fn health_recovers_when_vdevs_return_online() {
        // The user-facing contract (maintainer request): a pool recovered by
        // replacement must grade healthy again. Health is derived per-snapshot with no cached
        // state, so the demo fixtures double as a state-transition proof:
        // faulted pool grades Fault, the post-replacement pool (both
        // leaves ONLINE, resilver Finished) grades Healthy again.
        assert_eq!(
            crate::demo::sample(crate::demo::Scenario::Faulted, 0).health(),
            PoolHealth::Fault
        );
        assert_eq!(
            crate::demo::sample(crate::demo::Scenario::Done, 0).health(),
            PoolHealth::Healthy
        );
        // A resilver in progress against a healthy topology stays Healthy
        // (rebuild target is a full ONLINE member), and the errors
        // scenario (pool root DEGRADED mid-scan) grades Degraded.
        assert_eq!(
            crate::demo::sample(crate::demo::Scenario::Scanning, 2).health(),
            PoolHealth::Healthy
        );
        assert_eq!(
            crate::demo::sample(crate::demo::Scenario::Errors, 6).health(),
            PoolHealth::Degraded
        );
    }
}
