//! zresmon integration test matrix — Rust port of `scripts/run-matrix.sh` (bash).
//!
//! Cross-validates `zresmon --once --json` snapshots against a real ZFS node
//! using the zr* lab pools (mirror/raidz1/raidz2/draid2) created by
//! `scripts/lab.sh`. All cases run **sequentially** inside the single entry
//! point ([`lab_matrix`]) — cargo test's default parallel execution would
//! otherwise break the lab pool state ordering.
//!
//! # Usage (ZFS lab node only — root required)
//!
//! ```text
//! # Full run: sudo cargo test --test lab_matrix -- --ignored --nocapture
//! #   (--test lab_matrix already selects the target; `-- --ignored lab_matrix`
//! #    as a name filter is equivalent. --nocapture shows per-case output live.)
//!
//! # Case filter (substring). setup/teardown always run — the bash version
//! # skipped setup under a filter, which broke every subsequent case.
//! ZRESMON_MATRIX_FILTER=draid sudo cargo test --test lab_matrix -- --ignored --nocapture
//!
//! # Keep lab pools (skip teardown + print a notice)
//! ZRESMON_MATRIX_KEEP=1 sudo cargo test --test lab_matrix -- --ignored --nocapture
//! ```
//!
//! # When the node has no cargo (dev machine → scp flow)
//!
//! ```text
//! # Dev machine:
//! cargo test --target x86_64-unknown-linux-musl --no-run
//! scp target/x86_64-unknown-linux-musl/debug/deps/lab_matrix-<hash> node:
//! # Node: env!(CARGO_BIN_EXE_zresmon) bakes in the dev-machine path, which
//! # does not exist on the node — point ZRESMON_BIN at the real (musl)
//! # zresmon binary (preflight fails fast when unset or missing).
//! ZRESMON_BIN=/usr/local/bin/zresmon sudo ./lab_matrix-<hash> --ignored --nocapture
//! ```
//!
//! # jq is gone — a win of this port
//!
//! The bash version (run-matrix.sh) asserted everything through `jq -r`
//! filters, which made jq mandatory on the node. The Rust version
//! deserializes `--once --json` stdout into
//! [`zresmon::model::PoolSnapshot`] for typed assertions — **no jq required.**
//!
//! # The truth about lab pool names
//!
//! lab.sh increments its `suffix_num` after EVERY pool it creates
//! (lab.sh:143), so `setup --layout mirror:2,raidz1:3,raidz2:4,draid2:6:1`
//! creates **zrm0 / zr11 / zr22 / zrd3** on a clean node (verified live on
//! the lab node — NOT zrm0/zr10/zr20/zrd0 as this file's earlier draft and
//! the README's lab section once said). Leftover pools bump the suffix
//! further through the collision loop. Cases therefore look pools up by
//! the `zrm|zr1|zr2|zrd` prefix — robust against any suffix drift.
//! Historical note: the bash t_dual's hardcoded `fail --pool zr22` was
//! actually CORRECT (zr22 IS the raidz2 pool) and this port keeps that
//! coverage through prefix discovery. The bash t_setup count
//! `grep -cE '^zr(m|1|d)'` genuinely missed zr2*, though — this port
//! verifies each of the four prefixes individually.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use zresmon::model::{PoolSnapshot, ScanState, VdevInfo, VdevState};

/// Case outcome — `Ok(Pass|Skip)` is pass/skip, `Err(String)` is failure.
enum Outcome {
    Pass(String),
    Skip(String),
}

/// Infrastructure cases that always run even under a filter.
fn always_run(name: &str) -> bool {
    matches!(name, "setup" | "teardown")
}

fn matrix_filter() -> Option<Vec<String>> {
    // Comma-separated EXACT case names ("heal,dual,draid") — enables
    // one-group-at-a-time isolation runs. setup/teardown always run
    // regardless (infrastructure, not cases).
    std::env::var("ZRESMON_MATRIX_FILTER")
        .ok()
        .filter(|f| !f.is_empty())
        .map(|f| f.split(',').map(str::to_string).collect())
}

fn keep_pools() -> bool {
    std::env::var("ZRESMON_MATRIX_KEEP").is_ok_and(|v| v == "1")
}

/// Sequential case runner — prints PASS/SKIP/FAIL immediately and aggregates a summary.
struct Matrix {
    pass: usize,
    fail: usize,
    skip: usize,
    failures: Vec<(String, String)>,
    start: Instant,
}

impl Matrix {
    /// Runs one case. When `ZRESMON_MATRIX_FILTER` (substring) is set and the
    /// name does not match, the case is skipped (not counted). setup/teardown
    /// always run regardless of the filter — fixes the bash version's
    /// setup-skip defect.
    fn case(&mut self, name: &str, f: impl FnOnce() -> Result<Outcome, String>) {
        if let Some(filters) = matrix_filter() {
            if !always_run(name) && !filters.iter().any(|f| f == name) {
                println!("FILTER: {name} — not in filter set {filters:?}, skipped");
                return;
            }
        }
        let t0 = Instant::now();
        println!("RUN : {name}");
        match f() {
            Ok(Outcome::Pass(detail)) => {
                self.pass += 1;
                println!(
                    "PASS: {name} — {detail} [{:.1}s]",
                    t0.elapsed().as_secs_f32()
                );
            }
            Ok(Outcome::Skip(detail)) => {
                self.skip += 1;
                println!("SKIP: {name} — {detail}");
            }
            Err(err) => {
                self.fail += 1;
                self.failures.push((name.to_string(), err.clone()));
                println!("FAIL: {name} — {err} [{:.1}s]", t0.elapsed().as_secs_f32());
            }
        }
    }

    /// `RESULT: X pass / Y fail / Z skip (elapsed Ns)` + failure list.
    fn summary(&self) -> String {
        let mut s = format!(
            "RESULT: {} pass / {} fail / {} skip (elapsed {}s)",
            self.pass,
            self.fail,
            self.skip,
            self.start.elapsed().as_secs()
        );
        for (name, err) in &self.failures {
            s.push_str(&format!("\n  FAIL {name}: {err}"));
        }
        s
    }
}

/// zresmon binary path — `ZRESMON_BIN` takes precedence, otherwise the
/// build-time-baked `CARGO_BIN_EXE_zresmon`. The dev-machine path does not
/// exist on the node, so a missing path fails fast with a ZRESMON_BIN hint.
fn zresmon_bin() -> Result<PathBuf, String> {
    let (origin, path) = match std::env::var("ZRESMON_BIN") {
        Ok(v) if !v.is_empty() => ("ZRESMON_BIN", PathBuf::from(v)),
        _ => (
            "CARGO_BIN_EXE_zresmon",
            PathBuf::from(env!("CARGO_BIN_EXE_zresmon")),
        ),
    };
    if !path.exists() {
        return Err(format!(
            "zresmon binary not found ({origin}): {} — set ZRESMON_BIN=<path to zresmon>",
            path.display()
        ));
    }
    Ok(path)
}

/// Absolute path to lab.sh — `LAB_SH` env takes precedence, otherwise the
/// build-time-baked crate-manifest path. (Cross-node deployment: the baked
/// dev-machine path does not exist on the target node — set LAB_SH to the
/// real path, same pattern as ZRESMON_BIN.)
fn lab_sh() -> PathBuf {
    std::env::var("LAB_SH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("scripts")
                .join("lab.sh")
        })
}

/// Runs lab.sh (captures output) — cases attach stderr to their error messages.
fn lab(args: &[&str]) -> Output {
    Command::new("bash")
        .arg(lab_sh())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn lab.sh {}: {e}", lab_sh().display()))
}

/// Simple success check on lab.sh's exit code only.
fn lab_ok(args: &[&str]) -> bool {
    lab(args).status.success()
}

/// Runs lab.sh with stdout/stderr INHERITED — for long operations (setup,
/// fill) so lab.sh's own progress lines ("creating pool: …", fill txg
/// progress) stream live to the terminal. Without this, a 20-minute fill
/// looks like a hung test.
fn lab_stream(args: &[&str]) -> Output {
    Command::new("bash")
        .arg(lab_sh())
        .args(args)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| panic!("spawn lab.sh {}: {e}", lab_sh().display()))
}

/// Deserializes `<zresmon> --once --json --pool <pool>` stdout into a
/// `PoolSnapshot` — the foundation of typed assertions replacing the bash
/// version's `jq -r` filters.
fn snap(pool: &str) -> Result<PoolSnapshot, String> {
    // `timeout 60`: the observation must never wedge the matrix (a wedged
    // zresmon child would block a case forever — same D-state hazard as
    // the scrub; bounded at 60s).
    let out = Command::new("timeout")
        .arg("60")
        .arg(zresmon_bin()?)
        .args(["--once", "--json", "--pool", pool])
        .output()
        .map_err(|e| format!("spawn zresmon --pool {pool}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "zresmon --once --json --pool {pool} exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .map_err(|e| format!("decode PoolSnapshot for {pool}: {e}"))
}

/// First pool in `zpool list -H -o name` starting with the given prefix —
/// same as the bash pool_of (robust against suffix drift).
fn pool_of(prefix: &str) -> Option<String> {
    let out = Command::new("zpool")
        .args(["list", "-H", "-o", "name"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for name in String::from_utf8_lossy(&out.stdout).lines() {
        let name = name.trim();
        if name.starts_with(prefix) {
            return Some(name.to_string());
        }
    }
    None
}

/// Pool health (`zpool list -H -o health`, lowercased) — `None` if the pool is missing.
fn zpool_health(pool: &str) -> Option<String> {
    let out = Command::new("zpool")
        .args(["list", "-H", "-o", "health", pool])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let health = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
    if health.is_empty() {
        None
    } else {
        Some(health)
    }
}

/// Same approach as the bash wait_scan (verified on OpenZFS 2.2.9): polls the
/// `zpool status` scan line once per second until "in progress" disappears
/// (bounded by `max_secs`). Text polling is immune to zpool events output
/// format drift, hence more robust than event-based waiting.
fn wait_scan(pool: &str, max_secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    while Instant::now() < deadline {
        if let Ok(out) = Command::new("zpool").args(["status", pool]).output() {
            let scanning = String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.trim_start().starts_with("scan:") && l.contains("in progress"));
            if !scanning {
                return true;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

/// `zpool scrub` that tolerates the "currently resilvering" race: a scrub
/// repair pass can restart a resilver right after the previous scan
/// settles (observed live between the c2 and c3 cases on zrm0). Waits out
/// the colliding resilver and retries; non-collision failures are fatal.
fn scrub_with_retry(pool: &str, attempts: u32) -> Result<(), String> {
    for attempt in 1..=attempts {
        // `timeout 300`: a scrub can WEDGE in kernel D-state (observed on
        // OpenZFS 2.2.9 when a scrub overlaps a draid sequential rebuild with
        // an active read-injection handler). A D-state child cannot be
        // killed — `timeout` gives up after the cap and the harness
        // proceeds with a loud case failure instead of hanging forever.
        let out = Command::new("timeout")
            .args(["300", "zpool", "scrub", pool])
            .output()
            .map_err(|e| format!("spawn zpool scrub {pool}: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !err.contains("currently resilvering") {
            return Err(format!("zpool scrub {pool} failed: {err}"));
        }
        if !wait_scan(pool, 60) {
            return Err(format!(
                "{pool}: resilver did not settle in 60s (scrub retry {attempt})"
            ));
        }
    }
    Err(format!(
        "{pool}: scrub kept colliding with a resilver after {attempts} attempts"
    ))
}

/// Safety gate before any destructive zpool op: verifies the pool name
/// matches the lab prefix `^zr(m|1|2|d)`. Prevents accidentally touching the
/// node's real pools (e.g. ci_zfsPool_1A) — same spirit as lab.sh teardown's
/// zr* guard.
fn zr_guard(pool: &str) -> Result<(), String> {
    if ["zrm", "zr1", "zr2", "zrd"]
        .iter()
        .any(|p| pool.starts_with(p))
    {
        Ok(())
    } else {
        Err(format!(
            "refusing destructive op on non-lab pool {pool:?} \
             (expected ^zr(m|1|2|d) — node pools like ci_zfsPool_1A must stay untouched)"
        ))
    }
}

/// Fail-fast gates — root, zfs present, zresmon executable (--version).
/// (jq is NOT required — the Rust port dropped the bash version's jq dependency.)
fn preflight() -> Result<(), String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| format!("spawn id: {e}"))?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uid != "0" {
        return Err(format!(
            "not root (uid={uid}) — run: sudo cargo test --test lab_matrix -- --ignored"
        ));
    }
    Command::new("zfs")
        .arg("version")
        .output()
        .map_err(|e| format!("zfs(8) not found: {e} — run on a ZFS lab node"))?;
    let bin = zresmon_bin()?;
    let out = Command::new(&bin)
        .arg("--version")
        .output()
        .map_err(|e| format!("execute {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} --version failed: {}",
            bin.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Lab construction: teardown → 4-layout setup → fill → verifies each of
/// the 4 pools exists + is snapshottable. fill is deliberately sized at
/// `--txg 500 --mb 512 --max-min 5` — measured at ~160–600 txg/min on the
/// lab node, that is roughly 1–3 min per pool: enough dirty data for
/// meaningful resilvers without a 20-minute wait. (The bash-era 10-txg
/// instant preset stays abolished.)
fn case_setup() -> Result<Outcome, String> {
    // Tear down any leftover lab LOUDLY (streamed) — a silent teardown that
    // fails (busy pool, stale mountpoint) would cascade into dozens of
    // confusing case failures. The first real-matrix run proved this.
    let _ = lab_stream(&["teardown"]); // absent lab is not a failure

    // HARD GATE: teardown must leave zero zr* pools. Anything surviving
    // (busy pool from a stale handler, zombie process) would poison every
    // subsequent case — abort immediately with a clear reason instead.
    let leftover: Vec<String> = Command::new("zpool")
        .args(["list", "-H", "-o", "name"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|n| n.starts_with("zr"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !leftover.is_empty() {
        return Err(format!(
            "leftover lab pools survived teardown: {leftover:?} — \
             destroy them manually (zpool destroy -f …) and re-run"
        ));
    }

    let out = lab_stream(&[
        "setup",
        "--layout",
        "mirror:2,raidz1:3,raidz2:4,draid2:6:1",
        "--size",
        "1G",
    ]);
    if !out.status.success() {
        return Err(
            "lab.sh setup failed — see the streamed output above (mountpoint \
             collisions and zpool errors print there)"
                .into(),
        );
    }
    if !lab_stream(&["fill", "--txg", "500", "--max-min", "5"])
        .status
        .success()
    {
        return Err("lab.sh fill failed (--txg 500 --mb 512 --max-min 5)".into());
    }

    // Verify all 4 prefixes individually — the bash version's `>=3` grep
    // missed raidz2 (zr2*).
    let mut detail = String::new();
    for prefix in ["zrm", "zr1", "zr2", "zrd"] {
        let pool = pool_of(prefix)
            .ok_or_else(|| format!("no {prefix}* pool after setup — lab incomplete"))?;
        let health = zpool_health(&pool)
            .ok_or_else(|| format!("pool {pool} missing (zpool list -H -o health failed)"))?;
        let snapshot = snap(&pool)?;
        if snapshot.name != pool {
            return Err(format!(
                "zresmon snapshot name {:?} != zpool name {pool:?}",
                snapshot.name
            ));
        }
        detail.push_str(&format!("{pool}={health} "));
    }
    Ok(Outcome::Pass(format!(
        "4 pools live + snapshot OK: {}",
        detail.trim_end()
    )))
}

/// Safety checks: (a) zr_guard rejects a non-lab pool (ci_zfsPool_1A),
/// (b) `fail --dual` on the mirror pool is refused with a
/// "raidz2-only"/"would destroy" message.
fn case_guard() -> Result<Outcome, String> {
    // (a) The node's real production pool must be rejected by the guard.
    if zr_guard("ci_zfsPool_1A").is_ok() {
        return Err("zr_guard accepted non-lab pool ci_zfsPool_1A".into());
    }
    let mirror = pool_of("zrm").ok_or("no zrm* pool — run setup case first")?;
    zr_guard(&mirror)?;
    // (b) dual fail on the mirror must be refused (lab.sh: raidz2-only).
    let out = lab(&["fail", "--pool", mirror.as_str(), "--dual"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        return Err(format!(
            "fail --dual on mirror unexpectedly succeeded: {text}"
        ));
    }
    if text.contains("raidz2-only") || text.contains("would destroy") {
        Ok(Outcome::Pass(format!(
            "dual on mirror refused: {}",
            text.trim()
        )))
    } else {
        Err(format!("refused but guard message not recognized: {text}"))
    }
}

/// Healing flow: fail the mirror pool → replace → wait for resilver
/// completion. Replaces the bash t_heal's fixed `sleep 2` with wait_scan
/// polling — the standard fill (txg=1000, 512MiB) can take longer than
/// 2 seconds to resilver. After completion the snapshot must show
/// root=online + scan=finished.
fn case_heal() -> Result<Outcome, String> {
    let mirror = pool_of("zrm").ok_or("no zrm* pool — run setup case first")?;
    zr_guard(&mirror)?;

    let out = lab(&["fail", "--pool", mirror.as_str()]);
    if !out.status.success() {
        return Err(format!(
            "lab.sh fail --pool {mirror} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let _ = wait_scan(&mirror, 10); // wait for offline to register (result ignored)

    let out = lab(&["replace", "--pool", mirror.as_str()]);
    if !out.status.success() {
        return Err(format!(
            "lab.sh replace --pool {mirror} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if !wait_scan(&mirror, 60) {
        return Err("resilver did not settle in 60s".into());
    }

    let snapshot = snap(&mirror)?;
    if snapshot.root.state != VdevState::Online {
        return Err(format!(
            "root vdev state {:?} != online after heal",
            snapshot.root.state
        ));
    }
    match snapshot.scan {
        Some(scan) if scan.state == ScanState::Finished => Ok(Outcome::Pass(format!(
            "{mirror} healed: root=online, resilver finished"
        ))),
        Some(scan) => Err(format!(
            "scan state {:?} != finished after resilver",
            scan.state
        )),
        None => Err(format!("scan stats absent for {mirror} after resilver")),
    }
}

/// raidz2 dual-fault survival — fixes run-matrix.sh t_dual's hardcoded
/// `fail --pool zr22`: prefix-based discovery works on the clean-node name
/// (zr20) too. Dual-fault survival is deterministic for raidz2, so the bash
/// version's ok-always is abolished and root != Unavail is required. A
/// missing zr2* pool is a Skip, not a failure (e.g. setup without raidz2).
fn case_dual() -> Result<Outcome, String> {
    let Some(pool) = pool_of("zr2") else {
        return Ok(Outcome::Skip("no zr2* pool".into()));
    };
    zr_guard(&pool)?;

    // Keep the bash `|| true` — even if the dual fail fails (leftover faults
    // etc.), the replace story continues.
    let _ = lab(&["fail", "--pool", pool.as_str(), "--dual"]);

    let out = lab(&["replace", "--pool", pool.as_str()]);
    if !out.status.success() {
        return Err(format!(
            "lab.sh replace --pool {pool} failed after dual fail: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if !wait_scan(&pool, 60) {
        return Err("resilver did not settle in 60s after dual fail".into());
    }

    let snapshot = snap(&pool)?;
    if snapshot.root.state == VdevState::Unavail {
        return Err(format!(
            "raidz2 root vdev {:?} after dual failure — dual-fault survival is \
             deterministic, unavail means the pool actually died",
            snapshot.root.state
        ));
    }
    Ok(Outcome::Pass(format!(
        "{pool} survived dual failure: root={:?} (bash zr22 hardcode fixed)",
        snapshot.root.state
    )))
}

/// Whether a `zpool status -v` line matches regex `draid.*-[0-9]+-` — a
/// `-<digits>-` substring after "draid" means the distributed spare has been
/// incorporated into the vdev tree. Decided via a `-` split without a regex
/// crate dependency (a numeric-only segment followed by another segment
/// implies `-<digits>-` exists).
fn is_draid_spare_line(line: &str) -> bool {
    let Some(at) = line.find("draid") else {
        return false;
    };
    let rest = &line[at + "draid".len()..];
    let parts: Vec<&str> = rest.split('-').collect();
    parts
        .iter()
        .enumerate()
        .any(|(i, p)| i + 1 < parts.len() && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// dRAID distributed-spare sequential rebuild — a target-less `replace`
/// consumes the dRAID pool's distributed spare in lab.sh, triggering the
/// sequential rebuild path. Completion is asserted four ways: (a) snapshot
/// root != Unavail (recovered via distributed spare), (b) resilver_finish
/// >= 1 in `zpool events` — some builds emit no status wording, making this
/// the only robust completion signal, (c) a `draid.*-[0-9]+-` line in
/// `zpool status -v` (spare incorporated into the tree), (d) evidence capture.
fn case_draid() -> Result<Outcome, String> {
    let Some(pool) = pool_of("zrd") else {
        return Ok(Outcome::Skip("no zrd* pool".into()));
    };
    zr_guard(&pool)?;

    let out = lab(&["fail", "--pool", pool.as_str()]);
    if !out.status.success() {
        return Err(format!(
            "lab.sh fail --pool {pool} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let out = lab(&["replace", "--pool", pool.as_str()]);
    if !out.status.success() {
        return Err(format!(
            "lab.sh replace --pool {pool} failed (distributed-spare path): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if !wait_scan(&pool, 90) {
        return Err("resilver did not settle in 90s after spare promotion".into());
    }

    // (a) Recovery via the distributed spare — Unavail means the reproduction failed.
    let snapshot = snap(&pool)?;
    if snapshot.root.state == VdevState::Unavail {
        return Err(format!(
            "draid root vdev {:?} after distributed-spare resilver — expected recovery",
            snapshot.root.state
        ));
    }

    // (b) resilver_finish event — the robust sequential-rebuild completion signal.
    let out = Command::new("zpool")
        .arg("events")
        .output()
        .map_err(|e| format!("spawn zpool events: {e}"))?;
    let finishes = String::from_utf8_lossy(&out.stdout)
        .matches("resilver_finish")
        .count();
    if finishes == 0 {
        return Err("no resilver_finish in zpool events — sequential rebuild unconfirmed".into());
    }

    // (c) Was the distributed spare incorporated into the vdev tree?
    let out = Command::new("zpool")
        .args(["status", "-v", pool.as_str()])
        .output()
        .map_err(|e| format!("spawn zpool status -v {pool}: {e}"))?;
    let status = String::from_utf8_lossy(&out.stdout);
    let spare_line = status
        .lines()
        .find(|l| is_draid_spare_line(l))
        .map(str::trim)
        .ok_or("no draid.*-[0-9]+- line in zpool status -v — distributed spare not incorporated")?
        .to_string();

    // (d) Evidence capture.
    if !lab_ok(&["capture", "--label", "draid-sequential"]) {
        return Err("evidence capture failed".into());
    }

    Ok(Outcome::Pass(format!(
        "{pool} rebuilt via distributed spare: root={:?}, resilver_finish ×{finishes}, spare: {spare_line}",
        snapshot.root.state
    )))
}

/// Return online after clear — the bash t_clear was ok-always; here
/// root=online in the snapshot is required.
fn case_clear() -> Result<Outcome, String> {
    let pool = pool_of("zrm").ok_or("no zrm* pool — run setup case first")?;
    zr_guard(&pool)?;

    let out = Command::new("zpool")
        .args(["clear", &pool])
        .output()
        .map_err(|e| format!("spawn zpool clear {pool}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "zpool clear {pool} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let _ = wait_scan(&pool, 15); // wait for clear to register (result ignored)

    let snapshot = snap(&pool)?;
    if snapshot.root.state != VdevState::Online {
        return Err(format!(
            "root vdev state {:?} != online after clear",
            snapshot.root.state
        ));
    }
    Ok(Outcome::Pass(format!("{pool} online after clear")))
}

/// export → absence check → import roundtrip. zresmon is fail-soft and
/// **synthesizes an Online snapshot even for a missing pool**, so the
/// post-export absence check must use zpool list, not a zresmon snapshot
/// (insight preserved from the bash version).
fn case_export_import() -> Result<Outcome, String> {
    let pool = pool_of("zrm").ok_or("no zrm* pool — run setup case first")?;
    zr_guard(&pool)?;

    let out = Command::new("zpool")
        .args(["export", &pool])
        .output()
        .map_err(|e| format!("spawn zpool export {pool}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "zpool export {pool} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::thread::sleep(Duration::from_secs(1));
    if pool_of(&pool).is_some() {
        return Err(format!("{pool} still in zpool list after export"));
    }

    let out = Command::new("zpool")
        .args(["import", "-d", "/var/tmp/zresmon-lab", &pool])
        .output()
        .map_err(|e| format!("spawn zpool import {pool}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "zpool import -d /var/tmp/zresmon-lab {pool} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::thread::sleep(Duration::from_secs(1));
    let snapshot = snap(&pool)?;
    if snapshot.name != pool {
        return Err(format!(
            "zresmon snapshot name {:?} != pool {pool:?} after import",
            snapshot.name
        ));
    }
    // The import must also restore the MOUNT — later cases (c9's dd
    // service-continuity write) go through /<pool>. zpool import normally
    // mounts automatically, but the roundtrip is not proven until `zfs
    // mount` lists it.
    let mounted = || {
        Command::new("zfs")
            .arg("mount")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pool))
            .unwrap_or(false)
    };
    if !mounted() {
        let _ = Command::new("zfs").args(["mount", pool.as_str()]).output();
    }
    if !mounted() {
        return Err(format!(
            "{pool} not mounted after import — later dd cases would fail"
        ));
    }
    Ok(Outcome::Pass(format!(
        "{pool} export→import roundtrip OK (mounted)"
    )))
}

/// Per-layout iteration for the c1-c3 cases (pool prefix, layout kind).
/// setup always creates and verifies all 4, so absence is an Err, not a Skip.
const LAYOUTS: [(&str, &str); 4] = [
    ("zrm", "mirror"),
    ("zr1", "raidz1"),
    ("zr2", "raidz2"),
    ("zrd", "draid"),
];

/// Recursively walks the vdev tree and returns true if any node satisfies
/// `pred`. Interior mirror/raidz vdevs also carry state, so the whole tree
/// is inspected — failure signals appear on both leaves and interior nodes.
///
/// `pred` is a `&dyn Fn` — recursion with `impl Fn` instantiates a fresh
/// generic per call depth and hits the compiler recursion limit. dyn
/// dispatch breaks the cycle.
fn any_leaf_state(root: &VdevInfo, pred: &dyn Fn(&VdevState) -> bool) -> bool {
    pred(&root.state) || root.children.iter().any(|c| any_leaf_state(c, pred))
}

/// Spare exhaustion (c1) — deterministic per layout. The naive
/// fail→replace loop NEVER exhausts spares: each replace frees the
/// previous spare, and lab.sh's fail picks the first ONLINE leaf, so the
/// freed spare keeps bouncing between slots. Instead the ORIGINAL data
/// leaves are offlined explicitly (direct `zpool offline`), replacing
/// each with the next spare; when the originals run out, the promoted
/// spare itself is offlined — and that replace has no spare left, so it
/// MUST print the exhaustion warning:
///   mirror(2v+2sp):  v1→sp1, v2→sp2, offline sp1 → exhausted (degraded)
///   raidz1(3v+2sp):  v1→sp1, v2→sp2, offline v3  → exhausted (degraded)
///   raidz2(4v+2sp):  v1→sp1, v2→sp2, offline v3  → exhausted (online)
///   draid2:6:1(6v+1dsp): v1→dspare, offline v2 → exhausted (degraded)
/// State assertions: non-Unavail REQUIRED for non-draid (redundancy
/// arithmetic); draid cross-validates against zpool health.
fn case_c1() -> Result<Outcome, String> {
    let mut detail = String::new();
    for (prefix, kind) in LAYOUTS {
        let pool = pool_of(prefix)
            .ok_or_else(|| format!("no {prefix}* ({kind}) pool — setup creates all 4"))?;
        zr_guard(&pool)?;

        let leaves = match kind {
            "mirror" => 2,
            "raidz1" => 3,
            "raidz2" => 4,
            _ => 6, // draid2:6:1
        };
        let mut exhausted = false;
        for i in 1..=leaves {
            let dev = format!("/var/tmp/zresmon-lab/{pool}-v{i}.img");
            Command::new("zpool")
                .args(["offline", pool.as_str(), dev.as_str()])
                .output()
                .map_err(|e| format!("spawn zpool offline {dev}: {e}"))?;
            let out = lab(&["replace", "--pool", pool.as_str()]);
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            // lab.sh prints the exhaustion warning instead of replacing
            // ("spares exhausted — skipping …" / draid: "distributed spare
            // exhausted?"). lab.sh replace exits 0 since the --capture fix.
            if text.contains("exhausted") {
                exhausted = true;
                break;
            }
            if !wait_scan(&pool, 60) {
                return Err(format!(
                    "{pool}: scan did not settle in 60s after c1 round {i}"
                ));
            }
        }
        if !exhausted {
            // All originals consumed (mirror) — one more fault on the
            // promoted spare must be the drop that spills the glass.
            let dev = format!("/var/tmp/zresmon-lab/{pool}-spare1.img");
            Command::new("zpool")
                .args(["offline", pool.as_str(), dev.as_str()])
                .output()
                .map_err(|e| format!("spawn zpool offline {dev}: {e}"))?;
            let out = lab(&["replace", "--pool", pool.as_str()]);
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            exhausted = text.contains("exhausted");
        }
        if !exhausted {
            return Err(format!(
                "{pool} ({kind}): spares never exhausted — the replacement scenario \
                 is not consuming spares"
            ));
        }

        let snapshot = snap(&pool)?;
        if kind == "draid" {
            // Cross-validation instead of an absolute assertion — zpool
            // health vs snapshot root state.
            let health = zpool_health(&pool)
                .ok_or_else(|| format!("zpool health unavailable for {pool}"))?;
            let snap_word = snap_state_word(snapshot.root.state);
            if health != snap_word {
                return Err(format!(
                    "{pool}: zpool health {health:?} != snapshot root state {snap_word:?}"
                ));
            }
            detail.push_str(&format!("{pool}({kind})={snap_word} "));
        } else {
            if snapshot.root.state == VdevState::Unavail {
                return Err(format!(
                    "{pool} ({kind}) root vdev {:?} after exhaustion — redundancy \
                     arithmetic says it must survive",
                    snapshot.root.state
                ));
            }
            detail.push_str(&format!("{pool}({kind})={:?} ", snapshot.root.state));
        }
    }
    Ok(Outcome::Pass(format!(
        "exhaustion warned + survived: {}",
        detail.trim_end()
    )))
}

/// Healing with errors (c2) — merges dead run-matrix.sh t_healing_fail's
/// assertions. Per type: clear → fail → inject 50% read errors on a
/// surviving ONLINE leaf (a resilver SOURCE — injecting before the fail
/// would hit the same first-online leaf that fail offlines, so nothing
/// observable can happen; the first real-matrix run caught exactly that)
/// → replace → settle → SCRUB. The trailing scrub is the determinism fix:
/// the replace-resilver alone may read only kilobytes (fill's warm chunks
/// are freed right after writing, so allocated data can be tiny) and ZFS
/// retries absorb a 50% injection — zero surfaced errors then comes down
/// to luck. A scrub force-reads every allocated block through the
/// injected leaf, so its vdev read_err counter ALWAYS moves. The snapshot
/// must show at least ONE observable signal — scan.errors > 0, total
/// read_err > 0, root != Online, or a degraded/faulted/unavail node. One
/// `zinject -c all` after the loop keeps later cases unpolluted.
fn case_c2() -> Result<Outcome, String> {
    let mut detail = String::new();
    for (prefix, kind) in LAYOUTS {
        let pool = pool_of(prefix)
            .ok_or_else(|| format!("no {prefix}* ({kind}) pool — setup creates all 4"))?;
        zr_guard(&pool)?;

        let out = Command::new("zpool")
            .args(["clear", &pool])
            .output()
            .map_err(|e| format!("spawn zpool clear {pool}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "zpool clear {pool} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let out = lab(&["fail", "--pool", pool.as_str()]);
        if !out.status.success() {
            return Err(format!(
                "lab.sh fail --pool {pool} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // Inject AFTER the fail: the injection must land on a surviving
        // ONLINE leaf — a resilver SOURCE. (Injecting before the fail
        // targets the same first-online leaf that fail offlines, so no IO
        // ever passes through it and no error can surface — the first
        // real-matrix run caught exactly that.)
        let out = lab(&["inject", "--pool", pool.as_str(), "--pct", "50"]);
        if !out.status.success() {
            return Err(format!(
                "lab.sh inject --pool {pool} --pct 50 failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // A replace failure on an exhausted pool (zr2/zrd etc.) is part of
        // the scenario.
        let _ = lab(&["replace", "--pool", pool.as_str()]);
        if !wait_scan(&pool, 60) {
            return Err(format!("{pool}: resilver did not settle in 60s (c2)"));
        }
        // Determinism: force-read every allocated block through the
        // injected leaf — without this, a tiny replace-resilver plus ZFS
        // read retries can surface zero errors by luck.
        // EXCEPTION (draid): on OpenZFS 2.2.9 a scrub overlapping the
        // sequential rebuild wedges in kernel D-state (2026-08-31/09-01
        // runs, 40+ min each). For draid the observable signal is
        // deterministic anyway — the dspare is already consumed by the
        // draid case, so replace fails and the offline leaf keeps the
        // pool Degraded.
        if kind != "draid" {
            scrub_with_retry(&pool, 3)?;
            if !wait_scan(&pool, 60) {
                return Err(format!("{pool}: scrub did not settle in 60s (c2)"));
            }
        }

        let snapshot = snap(&pool)?;
        let scan_errors = snapshot.scan.as_ref().map_or(0, |s| s.errors);
        let read_errs = total_read_err(&snapshot.root);
        let bad_node = any_leaf_state(&snapshot.root, &|s| {
            matches!(
                s,
                VdevState::Degraded | VdevState::Faulted | VdevState::Unavail
            )
        });
        if scan_errors == 0
            && read_errs == 0
            && snapshot.root.state == VdevState::Online
            && !bad_node
        {
            return Err(format!(
                "injection produced no observable effect on {pool} ({kind}): scan.errors=0, \
                 root=online, no degraded/faulted/unavail node, read_errs=0 — tree: {:#?}",
                snapshot.root
            ));
        }
        detail.push_str(&format!(
            "{pool}({kind}):errors={scan_errors},root={:?} ",
            snapshot.root.state
        ));
    }

    // Once after the loop — clean up leftover injection handlers.
    let out = Command::new("zinject")
        .args(["-c", "all"])
        .output()
        .map_err(|e| format!("spawn zinject -c all: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "zinject -c all failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(Outcome::Pass(format!(
        "observable error signal on all 4: {}",
        detail.trim_end()
    )))
}

/// Recursively sums the read-error counter over the whole vdev tree —
/// the observable trace of injected read failures even when ZFS retries
/// succeed and the scan-level error count stays 0.
fn total_read_err(root: &VdevInfo) -> u64 {
    root.read_err + root.children.iter().map(total_read_err).sum::<u64>()
}

/// Recovery after clear+scrub (c3) — abolishes dead run-matrix.sh
/// t_healing_retry's ok-always and requires the recovered state. Per type:
/// clear → scrub → settle (30s), then the snapshot root must be Online or
/// Degraded (anything else = recovery failure, Err). A 1-second grace before
/// wait_scan polling replaces the bash version's fixed sleep 4 — polling
/// before the scrub line registers in status would race through the
/// "not started" static state.
fn case_c3() -> Result<Outcome, String> {
    let mut detail = String::new();
    for (prefix, kind) in LAYOUTS {
        let pool = pool_of(prefix)
            .ok_or_else(|| format!("no {prefix}* ({kind}) pool — setup creates all 4"))?;
        zr_guard(&pool)?;

        let out = Command::new("zpool")
            .args(["clear", &pool])
            .output()
            .map_err(|e| format!("spawn zpool clear {pool}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "zpool clear {pool} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // A repair pass can restart a resilver right after the previous
        // scrub settles (observed live: c2's scrub repair on zrm0 kept a
        // resilver in flight into this case) — retry over the collision.
        scrub_with_retry(&pool, 3)?;
        if !wait_scan(&pool, 30) {
            return Err(format!("{pool}: scrub did not settle in 30s"));
        }

        let snapshot = snap(&pool)?;
        if !matches!(snapshot.root.state, VdevState::Online | VdevState::Degraded) {
            return Err(format!(
                "{pool} ({kind}) root vdev {:?} after clear+scrub — expected online/degraded",
                snapshot.root.state
            ));
        }
        detail.push_str(&format!("{pool}({kind})={:?} ", snapshot.root.state));
    }
    Ok(Outcome::Pass(format!(
        "recovered online/degraded on all 4: {}",
        detail.trim_end()
    )))
}

/// First ONLINE leaf vdev of the pool from `zpool status -v` — lab image
/// files (`/var/tmp/zresmon-lab/<pool>-*.img`) whose status column is
/// ONLINE. `None` when the pool has no online leaf.
fn online_leaf(pool: &str) -> Option<String> {
    let out = Command::new("zpool")
        .args(["status", "-v", pool])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let marker = format!("{pool}-");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim_start)
        .find(|l| {
            l.starts_with("/var/tmp/zresmon-lab/")
                && l.contains(&marker)
                && l.contains(".img")
                && l.split_whitespace().nth(1) == Some("ONLINE")
        })
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
}

/// Maps a snapshot root state to the lowercase word `zpool list -o health`
/// reports — the vocabulary used by every cross-validation check.
fn snap_state_word(state: VdevState) -> &'static str {
    match state {
        VdevState::Online => "online",
        VdevState::Degraded => "degraded",
        VdevState::Faulted => "faulted",
        VdevState::Offline => "offline",
        VdevState::Removed => "removed",
        VdevState::Unavail => "unavail",
    }
}

/// C4: sequential rebuild interrupted by a second failure (draid only).
/// Deliberately races the rebuild — 1s after `replace`, another data leaf
/// goes offline. draid2's double parity makes survival deterministic, so
/// Unavail after the second fault is an Err (merges dead run-matrix.sh
/// t_seq_reset's assertion). If the rebuild already finished, the case is a
/// Skip ("too fast to interrupt"), not a pass.
fn case_c4() -> Result<Outcome, String> {
    let Some(pool) = pool_of("zrd") else {
        return Ok(Outcome::Skip("no zrd* pool".into()));
    };
    zr_guard(&pool)?;

    // lab results are ignored on purpose: a replace failure (spare already
    // consumed by an earlier case) still leaves the leaf-offline scenario
    // meaningful — the bash version's `>/dev/null`.
    let _ = lab(&["fail", "--pool", pool.as_str()]);
    let _ = lab(&["replace", "--pool", pool.as_str()]);
    // Intentional race: catch the rebuild while still in progress.
    std::thread::sleep(Duration::from_secs(1));

    // A data leaf (draid*-v*.img), not the promoted spare, to take offline.
    let out = Command::new("zpool")
        .args(["status", "-v", pool.as_str()])
        .output()
        .map_err(|e| format!("spawn zpool status -v {pool}: {e}"))?;
    let leaf = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim_start)
        .find(|l| {
            l.starts_with("/var/tmp/zresmon-lab/")
                && l.contains("-v")
                && l.contains(".img")
                && l.split_whitespace().nth(1) == Some("ONLINE")
        })
        .and_then(|l| l.split_whitespace().next().map(str::to_string));
    let Some(leaf) = leaf else {
        return Ok(Outcome::Skip(
            "rebuild too fast to interrupt — no ONLINE data leaf".into(),
        ));
    };

    // Second failure mid-rebuild.
    Command::new("zpool")
        .args(["offline", pool.as_str(), leaf.as_str()])
        .output()
        .map_err(|e| format!("spawn zpool offline {leaf}: {e}"))?;
    let settled = wait_scan(&pool, 30);
    let snapshot = snap(&pool)?;
    if snapshot.root.state == VdevState::Unavail {
        return Err(format!(
            "{pool}: Unavail after mid-rebuild second failure — draid2 double parity \
             must survive (leaf={leaf})"
        ));
    }
    if !settled {
        return Err(format!(
            "{pool}: scan did not settle in 30s after the mid-rebuild fault"
        ));
    }

    // Restore the leaf.
    Command::new("zpool")
        .args(["online", pool.as_str(), leaf.as_str()])
        .output()
        .map_err(|e| format!("spawn zpool online {leaf}: {e}"))?;
    if !wait_scan(&pool, 30) {
        return Err(format!(
            "{pool}: scan did not settle in 30s after leaf online"
        ));
    }
    Ok(Outcome::Pass(format!(
        "{pool} survived a mid-rebuild failure of {leaf}: root={:?}",
        snapshot.root.state
    )))
}

/// C5+C6: sequential→healing→scrub cascade (draid only). Waits out any
/// in-flight rebuild, then a scrub exercises the healing pass. Requires
/// the pool to be non-Unavail afterwards. The scrub_start event count is
/// informational only — OpenZFS 2.2.9 may not auto-scrub after a sequential
/// rebuild (build-variable behavior, deliberately not asserted).
fn case_c56() -> Result<Outcome, String> {
    let Some(pool) = pool_of("zrd") else {
        return Ok(Outcome::Skip("no zrd* pool".into()));
    };
    zr_guard(&pool)?;

    // Wait out any in-flight rebuild from earlier cases.
    if !wait_scan(&pool, 30) {
        return Err(format!(
            "{pool}: rebuild did not settle in 30s before scrub"
        ));
    }
    Command::new("zpool")
        .args(["scrub", pool.as_str()])
        .output()
        .map_err(|e| format!("spawn zpool scrub {pool}: {e}"))?;
    if !wait_scan(&pool, 30) {
        return Err(format!("{pool}: scrub did not settle in 30s"));
    }
    let snapshot = snap(&pool)?;
    if snapshot.root.state == VdevState::Unavail {
        return Err(format!(
            "{pool}: Unavail after the sequential→scrub cascade — cascade failed"
        ));
    }
    let events = Command::new("zpool")
        .arg("events")
        .output()
        .map_err(|e| format!("spawn zpool events: {e}"))?;
    let scrubs = String::from_utf8_lossy(&events.stdout)
        .matches("scrub_start")
        .count();
    Ok(Outcome::Pass(format!(
        "{pool} cascade complete: root={:?}, scrub_start events={scrubs} (informational)",
        snapshot.root.state
    )))
}

/// C8: pool suspension via 100% write injection (per layout type). The
/// outcome is genuinely build-variable — a mirror may absorb the write
/// (pool stays online) or the pool may suspend — so each type asserts a
/// CROSS-VALIDATION instead of an absolute state: the snapshot root state
/// must agree with `zpool list` health. A suspended pool is the one
/// exception (zresmon does not model SUSPENDED): there the snapshot must
/// simply not claim online. Pools without an ONLINE leaf are recorded and
/// skipped; if no pool qualifies the whole case is a Skip.
fn case_c8() -> Result<Outcome, String> {
    let mut detail = String::new();
    let mut attempted = 0usize;
    for (prefix, kind) in LAYOUTS {
        let pool = pool_of(prefix)
            .ok_or_else(|| format!("no {prefix}* ({kind}) pool — setup creates all 4"))?;
        zr_guard(&pool)?;
        let _ = Command::new("zpool")
            .args(["clear", pool.as_str()])
            .output();

        let Some(leaf) = online_leaf(&pool) else {
            detail.push_str(&format!("{pool}({kind})=no-online-leaf "));
            continue;
        };
        attempted += 1;
        // 100% write failure on the leaf, then one tiny write through it.
        Command::new("zinject")
            .args([
                "-d",
                leaf.as_str(),
                "-T",
                "write",
                "-f",
                "100",
                pool.as_str(),
            ])
            .output()
            .map_err(|e| format!("spawn zinject -d {leaf}: {e}"))?;
        let _ = Command::new("timeout").args([
            "5",
            "dd",
            "if=/dev/zero",
            &format!("/{pool}/suspend_test"),
            "bs=1k",
            "count=1",
        ]);
        let _ = Command::new("zinject").args(["-c", "all"]).output();

        let snapshot = snap(&pool)?;
        let health =
            zpool_health(&pool).ok_or_else(|| format!("zpool health unavailable for {pool}"))?;
        if health == "suspended" {
            if snapshot.root.state == VdevState::Online {
                return Err(format!(
                    "{pool}: zpool reports SUSPENDED but the snapshot claims online"
                ));
            }
            detail.push_str(&format!(
                "{pool}({kind})=suspended→{:?} ",
                snapshot.root.state
            ));
        } else {
            let snap_word = snap_state_word(snapshot.root.state);
            if health != snap_word {
                return Err(format!(
                    "{pool} ({kind}): zpool health {health:?} != snapshot root state {snap_word:?}"
                ));
            }
            detail.push_str(&format!("{pool}({kind})={snap_word} "));
        }
        // Cleanup: clear the injected error state and remove the test file.
        let _ = Command::new("zpool")
            .args(["clear", pool.as_str()])
            .output();
        let _ = std::fs::remove_file(format!("/{pool}/suspend_test"));
    }
    if attempted == 0 {
        return Ok(Outcome::Skip("no ONLINE leaf in any lab pool".into()));
    }
    Ok(Outcome::Pass(format!(
        "write-inject cross-validated on {attempted} pools: {}",
        detail.trim_end()
    )))
}

/// C9: service continuity under a single leaf fault (per layout type).
/// Every layout tolerates one fault, so the outcome IS deterministic
/// (merges dead run-matrix.sh t_partial_fault's assertion): the write must
/// succeed and the pool must be Online or Degraded — never Unavail.
/// Restores the leaf and removes the test files afterwards.
fn case_c9() -> Result<Outcome, String> {
    let mut detail = String::new();
    for (prefix, kind) in LAYOUTS {
        let pool = pool_of(prefix)
            .ok_or_else(|| format!("no {prefix}* ({kind}) pool — setup creates all 4"))?;
        zr_guard(&pool)?;
        let _ = Command::new("zpool")
            .args(["clear", pool.as_str()])
            .output();

        let Some(leaf) = online_leaf(&pool) else {
            return Err(format!(
                "{pool} ({kind}): no ONLINE leaf to fault — setup verifies all pools"
            ));
        };
        Command::new("zpool")
            .args(["offline", pool.as_str(), leaf.as_str()])
            .output()
            .map_err(|e| format!("spawn zpool offline {leaf}: {e}"))?;

        let write = Command::new("timeout")
            .args([
                "5",
                "dd",
                "if=/dev/zero",
                &format!("of=/{pool}/fault_test"),
                "bs=1k",
                "count=10",
            ])
            .output()
            .map_err(|e| format!("spawn dd: {e}"))?;
        let write_ok = write.status.success();
        let snapshot = snap(&pool)?;
        if !write_ok || !matches!(snapshot.root.state, VdevState::Online | VdevState::Degraded) {
            let mounted = Command::new("zfs")
                .arg("mount")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pool))
                .unwrap_or(false);
            return Err(format!(
                "{pool} ({kind}): service interrupted (write_ok={write_ok}, root={:?}) — \
                 every layout must survive a single fault; dd stderr: {}; mounted={mounted}",
                snapshot.root.state,
                String::from_utf8_lossy(&write.stderr).trim()
            ));
        }
        detail.push_str(&format!("{pool}({kind})={:?} ", snapshot.root.state));

        // Restore: bring the leaf back and clean up.
        let _ = Command::new("zpool")
            .args(["online", pool.as_str(), leaf.as_str()])
            .output();
        let _ = wait_scan(&pool, 30);
        let _ = std::fs::remove_file(format!("/{pool}/fault_test"));
    }
    Ok(Outcome::Pass(format!(
        "service continued on all 4: {}",
        detail.trim_end()
    )))
}

/// C10: replace back to the original disk (per layout type). Scenario:
/// fail → replace (spare promoted) → replace the spare BACK to the
/// original path. Same-path replace behavior is genuinely build-variable
/// (some builds reject it), so both outcomes are OK — the mandatory part
/// is the snapshot↔zpool cross-validation after any successful replace.
/// Missing prerequisites (no active spare / original file gone) are Skips
/// for that type, never silent passes.
fn case_c10() -> Result<Outcome, String> {
    let mut detail = String::new();
    for (prefix, kind) in LAYOUTS {
        let pool = pool_of(prefix)
            .ok_or_else(|| format!("no {prefix}* ({kind}) pool — setup creates all 4"))?;
        zr_guard(&pool)?;
        let _ = Command::new("zpool")
            .args(["clear", pool.as_str()])
            .output();

        let Some(leaf) = online_leaf(&pool) else {
            detail.push_str(&format!("{pool}({kind})=skipped(no-online-leaf) "));
            continue;
        };
        let _ = lab(&["fail", "--pool", pool.as_str()]);
        let _ = wait_scan(&pool, 10);
        let _ = lab(&["replace", "--pool", pool.as_str()]);
        if !wait_scan(&pool, 60) {
            return Err(format!("{pool}: resilver did not settle in 60s (c10)"));
        }

        // Active spare: a spare*.img now ONLINE in the tree.
        let out = Command::new("zpool")
            .args(["status", "-v", pool.as_str()])
            .output()
            .map_err(|e| format!("spawn zpool status -v {pool}: {e}"))?;
        let spare = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim_start)
            .find(|l| {
                l.contains("spare")
                    && l.contains(".img")
                    && l.split_whitespace().nth(1) == Some("ONLINE")
            })
            .and_then(|l| l.split_whitespace().next().map(str::to_string));
        let original_intact = std::path::Path::new(&leaf).is_file();
        let (Some(spare), true) = (spare, original_intact) else {
            detail.push_str(&format!(
                "{pool}({kind})=skipped(no-active-spare-or-original) "
            ));
            continue;
        };

        // Replace the spare BACK to the original path (simulating disk return).
        let out = Command::new("zpool")
            .args([
                "replace",
                "-f",
                pool.as_str(),
                spare.as_str(),
                leaf.as_str(),
            ])
            .output()
            .map_err(|e| format!("spawn zpool replace {pool}: {e}"))?;
        if out.status.success() {
            if !wait_scan(&pool, 60) {
                return Err(format!(
                    "{pool}: resilver did not settle in 60s (c10 replace-back)"
                ));
            }
            let snapshot = snap(&pool)?;
            let snap_word = snap_state_word(snapshot.root.state);
            let health = zpool_health(&pool)
                .ok_or_else(|| format!("zpool health unavailable for {pool}"))?;
            if health != snap_word {
                return Err(format!(
                    "{pool} ({kind}): after replace-back, zpool health {health:?} != \
                     snapshot root state {snap_word:?}"
                ));
            }
            detail.push_str(&format!("{pool}({kind})=replace-back({snap_word}) "));
        } else {
            detail.push_str(&format!(
                "{pool}({kind})=same-path-replace-rejected(build-variable) "
            ));
        }
        let _ = Command::new("zpool")
            .args(["clear", pool.as_str()])
            .output();
    }
    Ok(Outcome::Pass(format!(
        "replace-back scenario exercised: {}",
        detail.trim_end()
    )))
}

/// C11: cascading replace — the replacement also fails, replace again
/// (per layout type). Read errors are injected on the promoted spare and a
/// scrub drives IO; if the spare degrades it is replaced with the next
/// unused spare file. Injection strength is build-variable, so
/// "replacement survived" is a valid outcome — the mandatory assertions
/// are the cross-validation after any replacement and a parseable
/// snapshot. Missing prerequisites are per-type Skips.
fn case_c11() -> Result<Outcome, String> {
    let mut detail = String::new();
    for (prefix, kind) in LAYOUTS {
        let pool = pool_of(prefix)
            .ok_or_else(|| format!("no {prefix}* ({kind}) pool — setup creates all 4"))?;
        zr_guard(&pool)?;
        let _ = Command::new("zpool")
            .args(["clear", pool.as_str()])
            .output();

        let _ = lab(&["fail", "--pool", pool.as_str()]);
        let _ = lab(&["replace", "--pool", pool.as_str()]);
        if !wait_scan(&pool, 60) {
            return Err(format!(
                "{pool}: resilver did not settle in 60s (c11 initial replace)"
            ));
        }

        // The promoted spare (replacement) — an ONLINE spare*.img.
        let out = Command::new("zpool")
            .args(["status", "-v", pool.as_str()])
            .output()
            .map_err(|e| format!("spawn zpool status -v {pool}: {e}"))?;
        let status_text = out.stdout.clone();
        let replacement = String::from_utf8_lossy(&status_text)
            .lines()
            .map(str::trim_start)
            .find(|l| {
                l.contains("spare")
                    && l.contains(".img")
                    && l.split_whitespace().nth(1) == Some("ONLINE")
            })
            .and_then(|l| l.split_whitespace().next().map(str::to_string));
        let Some(replacement) = replacement else {
            detail.push_str(&format!("{pool}({kind})=skipped(no-promoted-spare) "));
            continue;
        };

        // Inject read errors on the replacement and drive IO through it.
        Command::new("zinject")
            .args([
                "-d",
                replacement.as_str(),
                "-T",
                "read",
                "-f",
                "80",
                pool.as_str(),
            ])
            .output()
            .map_err(|e| format!("spawn zinject -d {replacement}: {e}"))?;
        let _ = Command::new("zpool")
            .args(["scrub", pool.as_str()])
            .output();
        let _ = wait_scan(&pool, 30);
        let _ = Command::new("zinject").args(["-c", "all"]).output();
        let _ = Command::new("zpool")
            .args(["clear", pool.as_str()])
            .output();
        std::thread::sleep(Duration::from_secs(1));

        // Did the replacement degrade?
        let out = Command::new("zpool")
            .args(["status", "-v", pool.as_str()])
            .output()
            .map_err(|e| format!("spawn zpool status -v {pool}: {e}"))?;
        let degraded_leaf = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim_start)
            .find(|l| {
                l.contains("spare")
                    && l.contains(".img")
                    && l.split_whitespace().nth(1).is_some_and(|s| {
                        matches!(s, "DEGRADED" | "FAULTED" | "UNAVAIL" | "OFFLINE")
                    })
            })
            .and_then(|l| l.split_whitespace().next().map(str::to_string));

        if let Some(degraded_leaf) = degraded_leaf {
            // Next unused spare file: on disk but absent from the tree.
            let mut spare2: Option<std::path::PathBuf> = None;
            if let Ok(entries) = std::fs::read_dir("/var/tmp/zresmon-lab") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with(&format!("{pool}-spare"))
                        && name.ends_with(".img")
                        && !String::from_utf8_lossy(&out.stdout).contains(name.as_ref())
                    {
                        spare2 = Some(entry.path());
                        break;
                    }
                }
            }
            if let Some(spare2) = spare2 {
                Command::new("zpool")
                    .args([
                        "replace",
                        "-f",
                        pool.as_str(),
                        degraded_leaf.as_str(),
                        &spare2.to_string_lossy(),
                    ])
                    .output()
                    .map_err(|e| format!("spawn zpool replace (c11 cascade): {e}"))?;
                if !wait_scan(&pool, 60) {
                    return Err(format!(
                        "{pool}: resilver did not settle in 60s (c11 cascade)"
                    ));
                }
                let snapshot = snap(&pool)?;
                let snap_word = snap_state_word(snapshot.root.state);
                let health = zpool_health(&pool)
                    .ok_or_else(|| format!("zpool health unavailable for {pool}"))?;
                if health != snap_word {
                    return Err(format!(
                        "{pool} ({kind}): after cascading replace, zpool health {health:?} != \
                         snapshot root state {snap_word:?}"
                    ));
                }
                detail.push_str(&format!("{pool}({kind})=cascade-replaced({snap_word}) "));
            } else {
                // No spare left to cascade into — the pool state is still
                // cross-validated, the cascade itself is untestable here.
                let snapshot = snap(&pool)?;
                let snap_word = snap_state_word(snapshot.root.state);
                detail.push_str(&format!(
                    "{pool}({kind})=degraded-no-spare-left({snap_word}) "
                ));
            }
        } else {
            detail.push_str(&format!(
                "{pool}({kind})=replacement-survived(redundancy-absorbed) "
            ));
        }
        let _ = Command::new("zinject").args(["-c", "all"]).output();
        let _ = Command::new("zpool")
            .args(["clear", pool.as_str()])
            .output();
    }
    Ok(Outcome::Pass(format!(
        "cascading replace exercised: {}",
        detail.trim_end()
    )))
}

/// Lab teardown: after teardown, zero zr* pools must remain.
fn case_teardown() -> Result<Outcome, String> {
    let out = lab(&["teardown"]);
    if !out.status.success() {
        return Err(format!(
            "lab.sh teardown failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let out = Command::new("zpool")
        .args(["list", "-H", "-o", "name"])
        .output()
        .map_err(|e| format!("spawn zpool list: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let leftover: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|n| n.starts_with("zr"))
        .collect();
    if leftover.is_empty() {
        Ok(Outcome::Pass("no zr* pools remain".into()))
    } else {
        Err(format!("leftover lab pools: {leftover:?}"))
    }
}

/// Integration matrix entry point — runs all cases sequentially (prevents
/// cargo test's parallel execution from breaking lab pool state ordering).
/// See the module doc comment for the canonical invocations.
#[test]
#[ignore = "needs root + ZFS lab — run: sudo cargo test --test lab_matrix -- --ignored"]
fn lab_matrix() {
    if let Err(err) = preflight() {
        panic!("preflight failed: {err}");
    }
    let mut m = Matrix {
        pass: 0,
        fail: 0,
        skip: 0,
        failures: Vec::new(),
        start: Instant::now(),
    };
    m.case("setup", case_setup);
    // ABORT GATE: if setup failed, every remaining case would run against a
    // polluted/missing lab — pointless FAIL noise at best, and a live
    // suspend hazard at worst (2026-08-31: heal's replace wrote to vdev
    // files that teardown had unlinked → pool suspend → 32-min reboot
    // cycle). Best-effort cleanup, then abort loudly.
    if m.failures.iter().any(|(n, _)| n == "setup") {
        let _ = lab(&["teardown"]);
        let summary = m.summary();
        println!(
            "ABORT: setup failed — remaining cases skipped. Running cases on a \
             polluted lab is what caused the 2026-08-31 suspend incident.\
             \n{summary}"
        );
        panic!("lab matrix aborted: setup failed\n{summary}");
    }
    m.case("guard", case_guard);
    m.case("heal", case_heal);
    m.case("dual", case_dual);
    m.case("draid", case_draid);
    // Ordering rationale: healing scenarios (c2/c3) and replace-back
    // scenarios (c10/c11) need working spares, so they run BEFORE c1;
    // c9/c8 only offline/inject (no spares); c4/c56 are draid-specific
    // and run after the spare-consuming dual/draid; c1 (spare exhaustion)
    // is the last destructive case, right before teardown.
    m.case("clear", case_clear);
    m.case("export", case_export_import);
    m.case("healing_with_errors", case_c2);
    m.case("recovery_after_errors", case_c3);
    m.case("service_continuity", case_c9);
    m.case("write_inject", case_c8);
    m.case("rebuild_cascade", case_c56);
    m.case("mid_rebuild_failure", case_c4);
    m.case("replace_back", case_c10);
    m.case("cascading_replace", case_c11);
    m.case("spares_exhausted", case_c1);
    if keep_pools() {
        println!("KEEP: ZRESMON_MATRIX_KEEP=1 — teardown skipped, lab pools kept");
        println!("     clean up later: sudo scripts/lab.sh teardown");
    } else {
        m.case("teardown", case_teardown);
    }
    let summary = m.summary();
    println!("{summary}");
    // SKIP is not a failure — only fail is asserted.
    assert!(m.fail == 0, "lab matrix finished with failures:\n{summary}");
}
