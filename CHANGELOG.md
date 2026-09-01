# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] — 2026-09-01

### Added

- Lock-free, resource-agent style monitoring: no lock files, no PID files,
  N concurrent instances supported
- Multi-pool tab UI (`←/→`/`Tab` to switch, `1-9` to jump) with live pool
  discovery (pools created/destroyed while running appear/disappear)
- Pool health tab markers: pools with faulted/unavail/removed vdevs render
  red with a `✚` marker (the REPLACE badge vocabulary), degraded pools
  yellow with `!`; markers coexist with the scanning `⟳` and the
  active-tab idiom stays cyan+bold (the fault rides on the glyph there).
  Health is recomputed from every poll's snapshots, so recovery (e.g.
  after `zpool replace`) reverts the tab automatically
- Panel focus highlight: the focused body panel's title renders cyan+bold
  (active-tab idiom) and its border gets a cyan accent, giving `h`/`l`
  immediate visual feedback
- RPM-install-style progress gauge (`##########------[ 45%]`)
- Per-vdev error surface map with density legend and 120-second sliding
  window (ereport `io_offset`s bucketed per device, worst-first)
- Retry badge (`⟳ RETRY`) vs escalation badge (`✚ REPLACE` for FAULTED
  disks — retrying a dead device is pointless)
- `NO_COLOR` environment variable support (no-color.org): when set to a
  non-empty value, the TUI render drops fg/bg colors while keeping
  attributes (bold) and the glyph/character encoding channels, so state
  (density glyphs, badges, focus markers) stays readable without color
- Colors follow the terminal theme: every palette color is a named ANSI
  color (`░` stays `Yellow`; on 8-color terminals glyph shape remains the
  second density-encoding channel)
- `--once [--json]` one-shot snapshot mode (OCF resource-agent monitor
  action style)
- `--demo` fixture scenarios (scanning/done/errors/fault) for ZFS-less
  development
- Korean IME fallback: hotkeys work even when the IME delivers `ㅂ` for
  `q` (two-set QWERTY position projection — orcatui lesson applied)
- Panel scrolling: vdev tree and error map scroll independently (`↑/↓`,
  `h/l` to switch focus) with a scroll-position indicator
- Feature-detection scan fallback chain (version-independent):
  1. `/proc/spl/kstat/zfs/<pool>/scan` kstat
  2. `zpool status` "scan:" line parsing
  3. dRAID rebuild wording probe (`resilvered (vdev) ...`)
- `zpool events -fv` follower for live ereport ingestion
- Build-variant tolerant ereport parser (handles both `io_offset` and
  `zio_offset = ...` field formats from different ZFS builds)
- Humanized-counter parsing for per-vdev error counters (`8.18K`,
  `1.2M`): zpool abbreviates large counters, and the parsed values flow
  through to the TUI and `--once --json` output (regression-tested with
  the exact observed status line)
- Rust integration test matrix (`tests/lab_matrix.rs`): the lab scenario
  ported into the cargo test ecosystem. `cargo test` compiles but skips
  it (ignored), so CI stays green without ZFS; on a ZFS node it runs the
  full matrix with typed `PoolSnapshot` assertions via serde,
  ZRESMON_MATRIX_FILTER for case selection, ZRESMON_MATRIX_KEEP to keep
  the lab pools, and `ZRESMON_BIN`/`LAB_SH` overrides for cross-node
  deployment
- Matrix methodology: falsifiable assertions (deterministic redundancy
  outcomes are REQUIRED — mirror survives 3-way spare exhaustion, raidz2
  survives dual failure, every layout serves writes under a single
  fault; build-variable outcomes cross-validate the zresmon snapshot
  against zpool health), skips counted separately from passes,
  event-driven `wait_scan` waits instead of fixed sleeps, pool discovery
  by lab prefix, and every destructive op gated on lab-pool prefixes so
  real pools stay untouched
- Lab fill sized at `--txg 500 --mb 512 --max-min 5` (~1–3 min per pool,
  measured on the lab node) — enough dirty data for meaningful resilvers
- File-vdev lab tool (`scripts/lab.sh`): setup/fill/fail/replace/inject/
  capture/teardown lifecycle with RAID-type guards and unconditional
  `-f` on replace (required by OpenZFS 2.4.1 for offline leaves,
  harmless on 2.2.9)
- Fixture capture system (`capture` subcommand → `fixtures/` with
  zpool status, iostat, scan kstat, events, zresmon JSON snapshots)
- Hotkey tracking table (`docs/KEYBINDINGS.md`): every advertised key, its
  TUI-contract equivalent, and Korean IME equivalences — the
  no-dead-buttons safety net, updated alongside the on-screen footer
- ZFS monitoring reference (`docs/ZFS-MONITORING.md`): what file/command
  zresmon reads in each situation, the three-step scan fallback chain,
  and privilege/fail-soft behavior
- Constraints document (`docs/CONSTRAINTS.md`): the OpenZFS 2.2.9 kernel
  wedge (scrub overlapping a sequential rebuild with active read
  injection → D-state), its harness mitigations (bounded scrubs), the
  draid c2 scrub exception, the version boundary (upstream ZFS
  untested), and the observed 2.2.9 → 2.4.1 behavior differences
- CI as the primary integration-validation path: a `zfs-versions` matrix
  builds the spl/zfs kernel modules via DKMS on the Ubuntu 24.04
  (OpenZFS 2.2.x) and 26.04 (2.4.x) runners and runs the full lab matrix
  against real file-vdev pools; a FreeBSD VM job covers build + headless
  unit tests. macOS is intentionally excluded (no ZFS, no deployment
  target); 22.04 stays userspace-only (zfs 2.1 dkms does not build
  against the newer runner kernels). Scrubs are bounded
  (`timeout -k`), so a kernel wedge fails one case loudly instead of
  hanging the run
- Pool enumeration uses the `guid` file marker (not the `scan` kstat) —
  pools that never ran a scan are still observable
- `retry_needed` correctly returns true when an interior vdev (pool/raid
  group) is UNAVAIL even if all leaf vdevs read FAULTED ("insufficient
  replicas" means recovery is possible after replacement)
- Progress display omits unknown values entirely (no `n/a`)
- Vdev error counters (R/W/C) shown only when non-zero
- Bytes display uses real sizes from `zpool status` wording, not
  synthesized placeholders
- Two dedicated regression tests: the humanized-counter status line and
  the interior-UNAVAIL retry policy (revived from a missing `#[test]` /
  duplicate-attribute slip)
- Verification on Rocky 8 + OpenZFS 2.2.9 (vendor build): full matrix
  17/17 pass
- Verification on Ubuntu 26.04 + OpenZFS 2.4.1: full matrix 17/17 pass —
  the 2.2.9-era scrub/sequential-rebuild wedge did not reproduce;
  `zpool replace` of an OFFLINE leaf without `-f` is rejected on 2.4.1
  (lab.sh uses `-f` unconditionally — verified on both)
- dRAID sequential rebuild on the 2.2.9 vendor build: completion detected
  via event-based assertions (build omits rebuild wording in status)
- Numbering collision between scan kstat file and rebuild zap on the
  2.2.9 vendor build: handled by 3-level fallback chain

[1.0.0]: https://github.com/lhjnano/zresmon/releases/tag/v1.0.0
