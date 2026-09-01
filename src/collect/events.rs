//! Follower for `zpool events -fv` — live ereport ingestion.
//!
//! Lifecycle rules (multi-instance safe):
//! * Each follower owns its child process; [`EventFollower`] implements
//!   [`Drop`] so killing the child is guaranteed even on panic/unwind.
//! * The kernel event queue is not consumed destructively by readers — N
//! * followers may coexist, matching the resource-agent no-lock principle.
//! * Parsed ereports accumulate in a bounded ring buffer; callers drain()
//!   on their own cadence.

use crate::model::{Ereport, ErrKind};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread::JoinHandle;

const BUFFER_CAP: usize = 100;

/// Incremental parser fed one stdout line at a time.
///
/// `zpool events -v` emits one blank-line separated block per event; each
/// block starts with a `time ...` / `class ...` pair followed by payload
/// `key = value`-ish rows (whitespace separated).
#[derive(Default)]
pub struct EventBlockParser {
    cur_class: Option<String>,
    cur_path: Option<String>,
    cur_guid: Option<u64>,
    cur_offset: Option<u64>,
    cur_size: Option<u64>,
}

/// Parse a value that may be decimal or `0x`-prefixed hex.
fn parse_u64_maybe_hex(v: &str) -> Option<u64> {
    if let Some(hex) = v.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else {
        v.parse().ok()
    }
}

impl EventBlockParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line; returns a finished [`Ereport`] when a block completes.
    pub fn feed_line(&mut self, line: &str) -> Option<Ereport> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return self.flush();
        }
        let mut tokens = trimmed.split_whitespace();
        let mut key = tokens.next()?;
        // Real `zpool events -v` output is `key = value` (equals sign as its
        // own token, or attached). Strip it and re-read the value.
        let mut value = tokens.collect::<Vec<_>>().join(" ");
        if value.starts_with('=') {
            value = value[1..].trim().to_string();
        } else if key.ends_with('=') {
            key = &key[..key.len() - 1];
        }
        // Field naming varies by build: 2.2.9 vendor emits `zio_offset`/
        // `zio_size` (the zio payload prefix), older docs say `io_offset`.
        // Accept both spellings, and strip the quotes the verbose format
        // wraps string values in (`class = "ereport..."`).
        let key = key.trim_start_matches("zio_");
        let value = value.trim_matches('"');
        match key {
            "class" => self.cur_class = Some(value.to_string()),
            "vdev_path" => self.cur_path = Some(value.to_string()),
            "vdev_guid" => self.cur_guid = parse_u64_maybe_hex(value),
            "offset" | "io_offset" => self.cur_offset = parse_u64_maybe_hex(value),
            "size" | "io_size" => self.cur_size = parse_u64_maybe_hex(value),
            _ => {}
        }
        None
    }

    fn flush(&mut self) -> Option<Ereport> {
        let class = self.cur_class.take()?;
        // Class naming varies by build: `ereport.io.fs.zfs.io` (docs) and
        // `ereport.fs.zfs.io` (2.2.9 vendor observed) both end in `.zfs.io`.
        // Match on suffixes so both work.
        let kind = if class.ends_with("checksum") {
            ErrKind::Checksum
        } else if class.ends_with("fs.zfs.io") || class.ends_with(".io") {
            // io direction (read vs write) is not distinguishable from the class alone — approximate.
            ErrKind::IoRead
        } else {
            return None; // uninteresting class (probe_failure, sysevent, ...)
        };
        Some(Ereport {
            vdev_path: self.cur_path.take().unwrap_or_default(),
            vdev_guid: self.cur_guid.take().unwrap_or(0),
            io_offset: self.cur_offset.take().unwrap_or(0),
            io_size: self.cur_size.take().unwrap_or(0),
            kind,
            ts: std::time::SystemTime::now(),
        })
    }
}

/// Supervised `zpool events -fv` follower.
pub struct EventFollower {
    child: Mutex<Option<Child>>,
    #[allow(dead_code)] // JoinHandle detaches on drop — shutdown is guaranteed by Drop kill
    reader: Option<JoinHandle<()>>,
    buffer: std::sync::Arc<Mutex<VecDeque<Ereport>>>,
}

impl EventFollower {
    /// Spawn `zpool events -fv` and start background parsing.
    ///
    /// `zpool_bin` is exposed so tests can inject a fake shim path.
    pub fn spawn(zpool_bin: &str, pool: Option<&str>) -> anyhow::Result<Self> {
        let mut cmd = Command::new(zpool_bin);
        cmd.arg("events").arg("-fv");
        if let Some(p) = pool {
            cmd.arg(p);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("zpool events spawn failed: {e}"))?;
        let stdout = child.stdout.take();

        let buffer = std::sync::Arc::new(Mutex::new(VecDeque::with_capacity(BUFFER_CAP)));
        let buf_clone = buffer.clone();

        let reader = std::thread::spawn(move || {
            let Some(stdout) = stdout else { return };
            let mut parser = EventBlockParser::new();
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(ereport) = parser.feed_line(&line) {
                    let mut buf = buf_clone.lock().unwrap();
                    if buf.len() == BUFFER_CAP {
                        buf.pop_front();
                    }
                    buf.push_back(ereport);
                }
            }
        });

        Ok(Self {
            child: Mutex::new(Some(child)),
            reader: Some(reader),
            buffer,
        })
    }

    /// Drain accumulated ereports (move semantics, not a copy).
    pub fn drain(&self) -> Vec<Ereport> {
        let mut buf = self.buffer.lock().unwrap();
        buf.drain(..).collect()
    }
}

impl Drop for EventFollower {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait(); // prevent zombie
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "time         2026-08-23.10:00:01
class        ereport.io.fs.zfs.checksum
ena          0x0001
pool         tank
vdev_path    /dev/sdb
vdev_guid    0x0badc0de00000001
io_offset    0x123400000
io_size      0x20000

time         2026-08-23.10:00:05
class        ereport.io.fs.zfs.probe_failure
pool         tank

time         2026-08-23.10:00:09
class        ereport.io.fs.zfs.io
vdev_path    /dev/sdb
vdev_guid    0x0badc0de00000001
io_offset    4096
io_size      8192
";

    /// Real format observed on OpenZFS 2.2.9 (vendor build): `key = value`
    /// rows with equals signs, `zio_`-prefixed payload fields, and the
    /// shorter `ereport.fs.zfs.io` class path.
    const FIXTURE_229: &str = r#"Aug 27 2026 14:43:05.650012575 ereport.fs.zfs.io
        class = "ereport.fs.zfs.io"
        ena = 0x614e37ddc1501801
        pool = "zrm0"
        vdev_guid = 0x42df3372e6b2c4c9
        vdev_path = "/var/tmp/zresmon-lab/zrm0-v1.img"
        zio_offset = 0x48126600
        zio_size = 0x20000
        zio_err = 5
"#;

    #[test]
    fn parses_checksum_and_skips_uninteresting() {
        let mut p = EventBlockParser::new();
        let mut out = Vec::new();
        for line in FIXTURE.lines() {
            if let Some(e) = p.feed_line(line) {
                out.push(e);
            }
        }
        if let Some(e) = p.feed_line("") {
            out.push(e);
        }
        assert_eq!(out.len(), 2); // probe_failure excluded
        assert_eq!(out[0].kind, ErrKind::Checksum);
        assert_eq!(out[0].vdev_path, "/dev/sdb");
        assert_eq!(out[0].io_offset, 0x1234_00000); // "0x123400000"
        assert_eq!(out[1].kind, ErrKind::IoRead);
        assert_eq!(out[1].io_offset, 4096);
    }

    #[test]
    fn parses_229_equals_sign_and_zio_prefix() {
        let mut p = EventBlockParser::new();
        let mut out = Vec::new();
        for line in FIXTURE_229.lines() {
            if let Some(e) = p.feed_line(line) {
                out.push(e);
            }
        }
        // Final flush: the last block has no trailing blank line in the
        // fixture, so close it explicitly.
        if let Some(e) = p.feed_line("") {
            out.push(e);
        }
        assert_eq!(out.len(), 1);
        let e = &out[0];
        assert_eq!(e.kind, ErrKind::IoRead);
        assert_eq!(e.vdev_path, "/var/tmp/zresmon-lab/zrm0-v1.img");
        assert_eq!(e.vdev_guid, 0x42df3372e6b2c4c9);
        assert_eq!(e.io_offset, 0x48126600);
        assert_eq!(e.io_size, 0x20000);
    }

    #[test]
    fn follower_parses_fake_zpool_stream() {
        // fake zpool shim: emit events -fv output then sleep (follower read time)
        let dir = std::env::temp_dir().join("zresmon-shim-test");
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("zpool");
        std::fs::write(
            &shim,
            "#!/bin/sh\nprintf '%s' 'class  ereport.io.fs.zfs.checksum\nvdev_path /dev/sdb\n\n'\nsleep 5\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

        let follower = EventFollower::spawn(shim.to_str().unwrap(), Some("tank")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let drained = follower.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, ErrKind::Checksum);
        drop(follower); // verify child cleanup via Drop guard (no zombies)
    }
}
