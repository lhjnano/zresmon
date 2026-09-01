//! Best-effort parser for `zpool status -v`.
//!
//! Human output varies across OpenZFS releases and locales, so every parsed
//! field is fail-soft: an unrecognized line is skipped, a missing field
//! becomes `None`. Critical numeric values (scan progress) are read from the
//! scan kstat instead; this parser exists for topology, vdev states, per-vdev
//! error counters and the human progress wording.

use crate::model::{VdevInfo, VdevState};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Run `zpool status -v [pool]` with a hard timeout and return stdout.
///
/// `None` when zpool is absent or the command timed out — callers degrade to
/// kstat-only mode. Never returns an error for "permission denied" style
/// failures beyond logging them into the error string.
pub fn run_status(pool: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new("zpool");
    cmd.arg("status")
        .arg("-v")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(p) = pool {
        cmd.arg(p);
    }
    wait_with_timeout(&mut cmd, Duration::from_secs(5))
}

fn wait_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("zpool spawn failed: {e}"))?;
    let deadline = std::time::Instant::now() + timeout;
    let mut out = String::new();
    loop {
        if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_string(&mut out);
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(out),
            Ok(Some(status)) => return Err(format!("zpool exited with {status}")),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err("zpool timed out".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    }
}

fn parse_state_token(token: &str) -> Option<VdevState> {
    Some(match token {
        "ONLINE" => VdevState::Online,
        "DEGRADED" => VdevState::Degraded,
        "FAULTED" => VdevState::Faulted,
        "OFFLINE" => VdevState::Offline,
        "REMOVED" => VdevState::Removed,
        "UNAVAIL" => VdevState::Unavail,
        _ => return None,
    })
}

/// Scan summary parsed from the `scan:` line of `zpool status -v`.
///
/// Fallback for systems where the per-pool scan kstat is absent while a scan
/// is actively running (observed on OpenZFS 2.2.9 vendor builds). Best-effort
/// wording match, fail-soft by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanLine {
    pub func: crate::model::ScanFunc,
    pub state: crate::model::ScanState,
    pub pct: Option<f64>,
    /// Total bytes mentioned in the scan line (`resilvered 6.00G`, `repaired
    /// 128K`), when present. Used to fill the bytes display in kstat-less
    /// fallback mode.
    pub bytes: Option<u64>,
}

/// Parse the `scan:` section (including indented continuation lines) into a
/// [`ScanLine`]. `None` when no scan ran or the wording is unrecognized.
pub fn parse_scan_section(raw: &str) -> Option<ScanLine> {
    use crate::model::{ScanFunc, ScanState};
    let mut in_scan = false;
    let mut text = String::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.starts_with("scan:") {
            in_scan = true;
            text.push_str(t.trim_start_matches("scan:"));
            continue;
        }
        if in_scan {
            if !line.starts_with(char::is_whitespace) && !t.is_empty() {
                break;
            }
            text.push(' ');
            text.push_str(t);
        }
    }
    if text.is_empty() {
        return None;
    }
    let t = text.as_str();
    let func = if t.contains("resilver") || t.contains("rebuilt") {
        ScanFunc::Resilver
    } else if t.contains("scrub") {
        ScanFunc::Scrub
    } else {
        return None;
    };
    let state = if t.contains("in progress") {
        ScanState::Scanning
    } else if t.contains("canceled") || t.contains("cancelled") {
        ScanState::Canceled
    } else {
        ScanState::Finished
    };
    let pct = if state == ScanState::Scanning {
        extract_progress_pct(t)
    } else {
        None
    };
    let bytes = extract_scan_bytes(t);
    Some(ScanLine {
        func,
        state,
        pct,
        bytes,
    })
}

/// Parse a size like `6.00G` / `128K` / `512` following `resilvered`/
/// `repaired`/`scanned` in a scan line. Decimal (SI) units — `zpool status`
/// prints sizes in powers of 10 (G, M, K).
fn extract_scan_bytes(t: &str) -> Option<u64> {
    let lowered = t.to_ascii_lowercase();
    for marker in ["resilvered ", "repaired ", "scanned "] {
        if let Some(pos) = lowered.find(marker) {
            let rest = &t[pos + marker.len()..];
            let num_end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(rest.len());
            let (num, unit) = rest.split_at(num_end);
            let val: f64 = num.parse().ok()?;
            let mult = match unit.trim_start().chars().next()? {
                'T' | 't' => 1e12,
                'G' | 'g' => 1e9,
                'M' | 'm' => 1e6,
                'K' | 'k' => 1e3,
                _ => 1.0,
            };
            return Some((val * mult) as u64);
        }
    }
    None
}

/// Detect dRAID sequential rebuild wording in `zpool status -v` output.
///
/// Feature-detection (not version detection): upstream/vendor/custom builds
/// differ in whether rebuild stats surface through the scan kstat or only
/// through vdev-level wording. We probe the text itself:
///   Active:   "resilver (vdev) in progress since ..."
///   Complete: "resilvered (vdev) N in ... with X errors on ..."
///   Canceled: "resilver (vdev) canceled on ..."
/// These appear per-vdev (from print_rebuild_status_impl) and are the ONLY
/// signal on builds where the scan kstat/nvlist is absent post-rebuild.
#[derive(Debug, Clone, PartialEq)]
pub struct RebuildProbe {
    pub state: crate::model::ScanState,
    /// Bytes mentioned ("resilvered 2.00G"), when present.
    pub bytes: Option<u64>,
    /// Errors mentioned ("with 3 errors"), when present.
    pub errors: Option<u64>,
}

/// Scan the full status output for dRAID rebuild wording. Returns the most
/// significant find (active > complete > canceled), or `None`.
pub fn probe_rebuild(raw: &str) -> Option<RebuildProbe> {
    use crate::model::ScanState;
    let mut found: Option<RebuildProbe> = None;
    for line in raw.lines() {
        let t = line.trim();
        // Active rebuild: "resilver (vdev) in progress since ..."
        if t.starts_with("resilver (") && t.contains("in progress") {
            return Some(RebuildProbe {
                state: ScanState::Scanning,
                bytes: None,
                errors: None,
            });
        }
        // Complete: "resilvered (vdev) 2.00G in 0:00:05 with 0 errors on ..."
        if t.starts_with("resilvered (") {
            let bytes = extract_rebuild_bytes(t);
            let errors = extract_rebuild_errors(t);
            return Some(RebuildProbe {
                state: ScanState::Finished,
                bytes,
                errors,
            });
        }
        // Canceled: "resilver (vdev) canceled on ..."
        if t.starts_with("resilver (") && t.contains("canceled") {
            found = Some(RebuildProbe {
                state: ScanState::Canceled,
                bytes: None,
                errors: None,
            });
        }
    }
    found
}

/// Extract bytes from rebuild wording: "resilvered (vdev) 2.00G in ..."
/// The size sits AFTER the parenthesized vdev name, not after "resilvered".
fn extract_rebuild_bytes(t: &str) -> Option<u64> {
    // Find closing paren, then the next token is the size.
    let close = t.find(')')?;
    let rest = t[close + 1..].trim_start();
    let num_end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let (num, unit) = rest.split_at(num_end);
    let val: f64 = num.parse().ok()?;
    let mult = match unit.trim_start().chars().next()? {
        'T' | 't' => 1e12,
        'G' | 'g' => 1e9,
        'M' | 'm' => 1e6,
        'K' | 'k' => 1e3,
        _ => 1.0,
    };
    Some((val * mult) as u64)
}

/// Extract error count from rebuild wording: "with N errors".
fn extract_rebuild_errors(t: &str) -> Option<u64> {
    let idx = t.find(" with ")?;
    let rest = &t[idx + 6..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    // "with 0 errors" → Some(0); "with 12 errors" → Some(12)
    digits.parse().ok()
}

/// Extract the resilver/rebuild percentage from a `scan:` line.
///
/// Matches wordings like `... 45.2% done` / `(45% done)` / `rebuilt, 12.34% done`.
pub fn extract_progress_pct(line: &str) -> Option<f64> {
    let idx = line.find("% done")?;
    let before = &line[..idx];
    let num_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
        .map(|(i, _)| i)
        .last()?;
    before.get(num_start..)?.parse().ok()
}

/// One counter token from a `zpool status` vdev row: a plain integer
/// ("0", "4219") or zpool's humanized large-value form ("8.18K", "1.2M",
/// "2.00G" — 1024-based suffix, ~3 significant digits). Returns the value
/// scaled back to units, best-effort: the display is rounded, so the
/// parsed value is an approximation of the true counter.
fn parse_counter_token(t: &str) -> Option<u64> {
    if let Ok(v) = t.parse::<u64>() {
        return Some(v);
    }
    let mult = match t.chars().last()? {
        'K' | 'k' => 1024u64,
        'M' | 'm' => 1024 * 1024,
        'G' | 'g' => 1024 * 1024 * 1024,
        'T' | 't' => 1024u64.pow(4),
        'P' | 'p' => 1024u64.pow(5),
        _ => return None,
    };
    let value: f64 = t[..t.len() - 1].parse().ok()?;
    Some((value * mult as f64).round() as u64)
}

fn parse_counters(tokens: &[&str]) -> (u64, u64, u64) {
    // Counters are the READ/WRITE/CKSUM columns of the row. zpool
    // humanizes large values ("8.18K", "1.2M") and rows may carry
    // trailing annotations ("was /dev/sdb", "cannot open"), so scan
    // backwards, SKIPPING non-counter tokens, and collect the first
    // three counter values. (The previous pure-integer tail scan made a
    // row like "… ONLINE 0 0 8.18K" collapse to (0,0,0) — a monitoring
    // tool reporting zero errors on an error-ing vdev.)
    let mut vals = Vec::with_capacity(3);
    for token in tokens.iter().rev() {
        if let Some(v) = parse_counter_token(token) {
            vals.push(v);
            if vals.len() == 3 {
                break;
            }
        }
    }
    if vals.len() == 3 {
        (vals[2], vals[1], vals[0])
    } else {
        (0, 0, 0)
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Parse `zpool status -v` text into a vdev tree plus the scan progress line.
///
/// Returns `(root_vdev_name, root_tree, progress_pct, errors_lines)`.
///
/// Tree assembly note: `VdevInfo` is a plain value type, so a naive
/// "stack of mutable references" cannot be shared with the returned root.
/// We instead keep a flat list of `(indent, node)` candidates and fold
/// children upward whenever a shallower row arrives.
pub fn parse_status(raw: &str) -> (String, Option<VdevInfo>, Option<f64>, Vec<String>) {
    let mut root_name = String::new();
    let mut nodes: Vec<(usize, VdevInfo)> = Vec::new();
    let mut progress = None;
    let mut errors_section = false;
    let mut errors_lines = Vec::new();

    for raw_line in raw.lines() {
        let line = raw_line.trim_end();

        if line.starts_with("scan:") || line.contains(" scan:") {
            // Some releases put the % on the NEXT (indented) line —
            // fall through so the following line gets a chance.
            if let Some(p) = extract_progress_pct(line) {
                progress = Some(p);
            }
            continue;
        }
        if line.starts_with("errors:") {
            errors_section = true;
            continue;
        }
        if errors_section {
            if !line.is_empty() && !line.starts_with("no known data errors") {
                errors_lines.push(line.to_string());
            }
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        // Progress on its own indented line (release variation): try this
        // BEFORE the skip filters (only "% done" matches — low false-positive risk).
        if progress.is_none() {
            if let Some(p) = extract_progress_pct(trimmed) {
                progress = Some(p);
            }
        }
        if line.is_empty()
            || line.starts_with("NAME")
            || line.starts_with('-')
            || line.contains("state:")
            || line.contains("status:")
            || line.contains("action:")
            || line.contains("see:")
            || line.contains("config:")
        {
            continue;
        }
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let Some(si) = tokens.iter().position(|t| parse_state_token(t).is_some()) else {
            continue;
        };
        let name = tokens[..si].join(" ");
        if name.is_empty() {
            continue;
        }
        let state = parse_state_token(tokens[si]).unwrap();
        let counters = parse_counters(&tokens[si + 1..]);
        let rebuild_pct = extract_progress_pct(line);
        let ind = indent_of(line);

        let vdev = VdevInfo {
            name,
            state,
            is_rebuild_target: rebuild_pct.is_some(),
            read_err: counters.0,
            write_err: counters.1,
            checksum_err: counters.2,
            rebuild_pct,
            children: Vec::new(),
        };

        // A shallower row arrives → deeper finished nodes fold into their
        // nearest shallower ancestor kept on the flat list.
        while matches!(nodes.last(), Some(&(i, _)) if i >= ind) {
            let (_, finished) = nodes.pop().unwrap();
            match nodes.last_mut() {
                Some((_, parent)) => parent.children.push(finished),
                None => {
                    // Top-level sibling: nothing above to attach to; keep as root.
                    nodes.push((ind, finished));
                    break;
                }
            }
        }
        if root_name.is_empty() {
            root_name = vdev.name.clone();
        }
        nodes.push((ind, vdev));
    }

    // Fold the remaining chain upward: shallowest (lowest indent) wins as root.
    while nodes.len() > 1 {
        let (_, finished) = nodes.pop().unwrap();
        match nodes.last_mut() {
            Some((_, parent)) => parent.children.push(finished),
            None => break,
        }
    }
    let root = nodes.pop().map(|(_, v)| v);

    (root_name, root, progress, errors_lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
  pool: tank
 state: DEGRADED
status: One or more devices ...

  scan: resilver in progress since Tue Aug 23 10:00:00 2026
\t37.5% done

config:

NAME                          STATE     READ WRITE CKSUM
tank                          DEGRADED     0     0     0
  mirror-0                    DEGRADED     0     0     0
    sda                       ONLINE       0     0     0
    sdb                       FAULTED      4     1    12  too many errors

errors: Permanent errors have been detected in the following files:
        /tank/data/big.db
";

    #[test]
    fn parses_humanized_counters() {
        // zpool status abbreviates large counters ("8.18K", "1.2M") —
        // observed live on the lab node: a resilver with thousands of
        // injected read errors rendered the whole row as (0,0,0) before
        // the fix, so a monitoring tool reported ZERO errors on an
        // error-ing vdev.
        let raw = [
            "pool: p\n",
            "\nNAME STATE READ WRITE CKSUM\n",
            "p ONLINE 0 0 0\n",
            "  mirror-0 ONLINE 0 0 0\n",
            "    /var/tmp/zresmon-lab/zrm0-spare1.img ONLINE 0 0 8.18K\n",
            "    /var/tmp/zresmon-lab/zrm0-v2.img ONLINE 1.2M 0 0\n",
        ]
        .concat();
        let (_, root, _, _) = parse_status(&raw);
        let r = root.expect("root tree");
        let mirror = &r.children[0];
        let spare1 = &mirror.children[0];
        assert_eq!(spare1.name, "/var/tmp/zresmon-lab/zrm0-spare1.img");
        assert_eq!(spare1.checksum_err, 8_376); // 8.18 * 1024, rounded
        assert_eq!(spare1.read_err, 0);
        let v2 = &mirror.children[1];
        assert_eq!(v2.read_err, 1_258_291); // 1.2 * 1024^2, rounded
        assert_eq!(v2.checksum_err, 0);
    }

    #[test]
    fn parses_topology_and_states() {
        let (_, root, pct, errs) = parse_status(SAMPLE);
        let r = root.expect("root tree");
        assert_eq!(r.name, "tank");
        assert_eq!(r.state, VdevState::Degraded);
        assert_eq!(r.children.len(), 1); // mirror-0
        let mirror = &r.children[0];
        assert_eq!(mirror.name, "mirror-0");
        assert_eq!(mirror.children.len(), 2);
        assert_eq!(mirror.children[0].name, "sda");
        assert_eq!(mirror.children[0].state, VdevState::Online);
        assert_eq!(mirror.children[1].state, VdevState::Faulted);
        assert_eq!(mirror.children[1].read_err, 4);
        assert_eq!(mirror.children[1].checksum_err, 12);
        assert_eq!(pct, Some(37.5));
        assert!(errs.iter().any(|l| l.contains("big.db")));
    }

    #[test]
    fn extracts_progress_variants() {
        assert_eq!(
            extract_progress_pct("scan: resilver 12.5% done"),
            Some(12.5)
        );
        assert_eq!(extract_progress_pct("(45% done)"), Some(45.0));
        assert_eq!(extract_progress_pct("no numbers here"), None);
    }

    #[test]
    fn rebuild_probe_detects_all_states() {
        // Active
        let raw = "  resilver (draid2-0-0) in progress since Thu Aug 27\n";
        let p = probe_rebuild(raw).unwrap();
        assert_eq!(p.state, crate::model::ScanState::Scanning);
        // Complete with bytes + errors
        let raw2 = "  resilvered (draid2-0-0) 2.00G in 0:00:05 with 0 errors on Thu\n";
        let p2 = probe_rebuild(raw2).unwrap();
        assert_eq!(p2.state, crate::model::ScanState::Finished);
        assert_eq!(p2.bytes, Some(2_000_000_000));
        assert_eq!(p2.errors, Some(0));
        // Canceled
        let raw3 = "  resilver (draid2-0-0) canceled on Thu Aug 27\n";
        let p3 = probe_rebuild(raw3).unwrap();
        assert_eq!(p3.state, crate::model::ScanState::Canceled);
        // No rebuild wording
        assert!(probe_rebuild("  scan: resilver in progress\n").is_none());
    }

    #[test]
    fn rebuild_probe_active_wins_over_canceled() {
        let raw = "  resilver (v-0) canceled on Thu\n  resilver (v-1) in progress since Thu\n";
        let p = probe_rebuild(raw).unwrap();
        assert_eq!(p.state, crate::model::ScanState::Scanning);
    }

    #[test]
    fn scan_section_parsing_covers_states() {
        // in-progress with % on the next line
        let raw =
            "  scan: resilver in progress since Tue Aug 23 10:00:00 2026\n\t37.5% done\nconfig:\n";
        let sl = parse_scan_section(raw).unwrap();
        assert_eq!(sl.func, crate::model::ScanFunc::Resilver);
        assert_eq!(sl.state, crate::model::ScanState::Scanning);
        assert_eq!(sl.pct, Some(37.5));
        // finished wording
        let raw2 = "  scan: resilvered 2.00G in 00:00:07 with 0 errors on X\n";
        let sl2 = parse_scan_section(raw2).unwrap();
        assert_eq!(sl2.state, crate::model::ScanState::Finished);
        assert_eq!(sl2.pct, None);
        assert_eq!(sl2.bytes, Some(2_000_000_000)); // "2.00G" (SI units)
                                                    // scrub in progress
        let raw3 = "  scan: scrub in progress since X\n";
        let sl3 = parse_scan_section(raw3).unwrap();
        assert_eq!(sl3.func, crate::model::ScanFunc::Scrub);
        assert_eq!(sl3.state, crate::model::ScanState::Scanning);
        // no scan section at all
        assert!(parse_scan_section("  pool: t\nconfig:\n").is_none());
    }

    #[test]
    fn no_errors_section_is_clean() {
        let raw = "pool: p\n\nNAME STATE READ WRITE CKSUM\np ONLINE 0 0 0\n\nerrors: No known data errors\n";
        let (_, _, _, errs) = parse_status(raw);
        assert!(errs.is_empty());
    }
}
