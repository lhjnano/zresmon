//! Parser for `/proc/spl/kstat/zfs/<pool>/scan`.
//!
//! kstat procfs format (blank-line separated):
//!
//! ```text
//! 33     34 scan
//! name   type data
//!
//! func       4    2
//! state      4    1
//! start_time 4    1726000000
//! ...
//! ```
//!
//! The first two lines are a numeric header and a `name type data` header;
//! every following non-blank line is `name type value`. We map by *column
//! name* (position-independent), skip unknown names, and treat a missing
//! file as `Ok(None)` — absence means "this pool has never scanned", not an
//! error. Tolerant across OpenZFS releases that add/reorder fields.

use crate::model::{PoolSnapshot, ScanFunc, ScanState, ScanStats, VdevInfo, VdevState};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Enumerate ZFS pools visible on this host.
///
/// Lists every pool directory under `/proc/spl/kstat/zfs/` — a pool that has
/// never run a scan has no `scan` file, but it is still observable (iostats,
/// objsets, txgs). Pool directories are distinguished from the global kstat
/// files (arcstats, dmu_tx, ...) by the `guid` entry, which exists only in
/// per-pool directories on every OpenZFS release verified so far (2.2.9
/// vendor build: `guid`, `iostats`, `multihost`, `objset-*`, ...).
///
/// Unprivileged: directory reads only.
pub fn list_pools(proc_root: &Path) -> Vec<String> {
    let zfs_dir = proc_root.join("zfs");
    let mut pools = Vec::new();
    if let Ok(entries) = std::fs::read_dir(zfs_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join("guid").is_file() {
                pools.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    pools.sort();
    pools
}

/// Read the scan kstat for one pool.
///
/// Returns:
/// * `Ok(None)` — pool exists but has no scan record (never scanned), or the
///   file is absent entirely (pool not on this host). Absence, not failure.
/// * `Ok(Some(snapshot))` — a parsed sample.
/// * `Err(_)` — I/O failure while the file *exists* (permissions changed
///   mid-read etc.). Genuine errors are rare and worth surfacing.
pub fn read_scan(
    proc_root: &Path,
    pool: &str,
    now: SystemTime,
) -> anyhow::Result<Option<PoolSnapshot>> {
    let path: PathBuf = proc_root.join("zfs").join(pool).join("scan");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(parse_scan_kstat(pool, &raw, now))
}

/// Parse the textual content of a scan kstat. Exposed for fixture tests.
pub fn parse_scan_kstat(pool: &str, raw: &str, now: SystemTime) -> Option<PoolSnapshot> {
    let mut fields = std::collections::BTreeMap::new();
    let mut seen_header = false;

    for line in raw.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        // Skip the two header rows: "col idx" numerics and the literal
        // "name type data" header that starts the record section.
        if !seen_header {
            if tokens.first() == Some(&"name") && tokens.get(1) == Some(&"type") {
                seen_header = true;
            }
            continue;
        }
        if tokens.len() >= 3 {
            fields.insert(tokens[0].to_string(), tokens[2].to_string());
        }
    }
    if !seen_header || fields.is_empty() {
        return None;
    }

    let func_num: u64 = fields.get("func")?.parse().ok()?;
    let state_num: u64 = fields.get("state")?.parse().ok()?;
    let func = match func_num {
        1 => ScanFunc::Scrub,
        2 => ScanFunc::Resilver,
        _ => return None, // POOL_SCAN_NONE — nothing recorded
    };
    let state = match state_num {
        1 => ScanState::Scanning,
        2 => ScanState::Finished,
        3 => ScanState::Canceled,
        _ => ScanState::Idle,
    };

    let unix_field = |key: &str| -> Option<SystemTime> {
        fields
            .get(key)
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(|s| UNIX_EPOCH + Duration::from_secs(s))
    };
    let counter = |key: &str| -> u64 { fields.get(key).and_then(|v| v.parse().ok()).unwrap_or(0) };

    // A zero start_time with an active func means "no scan ever ran"; the
    // kstat reports zeros rather than omitting the file.
    if counter("start_time") == 0 && counter("end_time") == 0 {
        return None;
    }

    Some(PoolSnapshot {
        name: pool.to_string(),
        scan: Some(ScanStats {
            func,
            state,
            start_time: unix_field("start_time"),
            end_time: unix_field("end_time"),
            to_examine: counter("to_examine"),
            examined: counter("examined"),
            processed: counter("processed"),
            skipped: counter("skipped"),
            errors: counter("errors"),
            issued: counter("issued"),
            pass_exam: counter("pass_exam"),
            sampled_at: now,
            progress_override: None,
        }),
        root: placeholder_root(),
        sampled_at: now,
    })
}

/// Placeholder topology until [`crate::collect::status`] merges in real
/// per-vdev data. The kstat carries no topology of its own.
fn placeholder_root() -> VdevInfo {
    VdevInfo {
        name: String::new(),
        state: VdevState::Online,
        is_rebuild_target: false,
        read_err: 0,
        write_err: 0,
        checksum_err: 0,
        rebuild_pct: None,
        children: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE_RESILVER: &str = "\
33 34 scan
name type data

func 4 2
state 4 1
start_time 4 1726000000
end_time 4 0
to_examine 4 1000000
examined 4 400000
processed 4 350000
skipped 4 5000
errors 4 3
issued 4 380000
pass_exam 4 120000
";

    const FINISHED_SCRUB: &str = "\
33 34 scan
name type data

func 4 1
state 4 2
start_time 4 1726000000
end_time 4 1726010000
to_examine 4 2000000
examined 4 2000000
processed 4 1900000
skipped 4 0
errors 4 0
issued 4 0
pass_exam 4 0
";

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1726000100)
    }

    #[test]
    fn parses_active_resilver() {
        let snap = parse_scan_kstat("tank", ACTIVE_RESILVER, now()).expect("snapshot");
        let scan = snap.scan.expect("scan present");
        assert_eq!(scan.func, ScanFunc::Resilver);
        assert_eq!(scan.state, ScanState::Scanning);
        assert_eq!(scan.errors, 3);
        assert_eq!(scan.examined, 400_000);
        assert_eq!(
            scan.start_time,
            Some(UNIX_EPOCH + Duration::from_secs(1_726_000_000))
        );
        assert!(scan.end_time.is_none());
        assert_eq!(snap.name, "tank");
    }

    #[test]
    fn parses_finished_scrub() {
        let snap = parse_scan_kstat("tank", FINISHED_SCRUB, now()).expect("snapshot");
        let scan = snap.scan.unwrap();
        assert_eq!(scan.func, ScanFunc::Scrub);
        assert_eq!(scan.state, ScanState::Finished);
        assert_eq!(scan.errors, 0);
        assert_eq!(
            scan.end_time,
            Some(UNIX_EPOCH + Duration::from_secs(1_726_010_000))
        );
    }

    #[test]
    fn all_zero_row_means_never_scanned() {
        let raw = "33 34 scan\nname type data\n\nfunc 4 0\nstate 4 0\nstart_time 4 0\nend_time 4 0\nto_examine 4 0\nexamined 4 0\n";
        assert!(parse_scan_kstat("tank", raw, now()).is_none());
    }

    #[test]
    fn unknown_fields_are_skipped() {
        let raw = "name type data\n\nnewfield 4 99999\nfunc 4 2\nstate 4 1\nto_examine 4 10\nexamined 4 5\nstart_time 4 1726000000\n";
        let snap = parse_scan_kstat("p", raw, now()).unwrap();
        assert_eq!(snap.scan.unwrap().examined, 5);
    }

    #[test]
    fn pools_without_scan_history_are_still_listed() {
        // A pool without scan history (missing scan file) is still an observation target.
        let tmp = std::env::temp_dir().join("zresmon-noscan-test");
        let _ = std::fs::remove_dir_all(&tmp);
        let zfs = tmp.join("zfs");
        let pool_a = zfs.join("poolA");
        let pool_b = zfs.join("poolB");
        let global = zfs.join("arcstats"); // global kstat file — not a pool
        std::fs::create_dir_all(&pool_a).unwrap();
        std::fs::create_dir_all(&pool_b).unwrap();
        std::fs::write(global, "x").unwrap();
        std::fs::write(pool_a.join("guid"), "x").unwrap();
        // poolB: no scan file, guid marker only — still a pool
        std::fs::write(pool_b.join("guid"), "x").unwrap();

        let pools = list_pools(&tmp);
        assert_eq!(pools, vec!["poolA".to_string(), "poolB".to_string()]);

        // No scan kstat → read_scan returns Ok(None): "absence, not failure"
        let r = read_scan(&tmp, "poolA", now()).unwrap();
        assert!(r.is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_file_is_none_not_error() {
        let tmp = std::env::temp_dir().join("zresmon-no-such-pool");
        let r = read_scan(&tmp, "tank", now()).unwrap();
        assert!(r.is_none());
    }
}
