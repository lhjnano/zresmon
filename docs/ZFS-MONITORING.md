# ZFS Monitoring Reference

How zresmon observes ZFS — situation by situation, file by file, command by
command. This is the operator's answer to *"why does the tool show that?"*:
which kernel file or `zpool` command produced every number on screen, and
what happens when one of them is unreadable.

Behavior that could not be verified against a live pool is marked as
build-dependent (see [§6](#6-version-dependent-behavior)).

## 1. Overview

zresmon is a **read-only observer**. It never issues a mutating `zpool`
command, never writes to the pool, holds no lock files or PID files, and
keeps no state between frames — N instances may run concurrently.

Every poll — every 2 seconds by default, `--interval SECS` to change
(clamped to a 1s floor in the run loop) — is a self-contained re-collection:

- the pool list is re-discovered from scratch, so pools created or
  destroyed while zresmon runs appear/disappear live;
- **every** visible pool is re-sampled, not just the selected tab;
- tab health markers, retry badges, and gauges are recomputed from that
  tick's snapshots alone — there is no cached "last known state", which is
  why a pool recovers on screen automatically one poll after
  `zpool replace` finishes.

The observations come from exactly three sources:

| # | Source | Privilege | On failure |
|---|--------|-----------|------------|
| 1 | `/proc/spl/kstat/zfs/<pool>/scan` (kernel procfs file) | unprivileged | absence is *not* an error — see below |
| 2 | `zpool status -v` (subprocess, 5s hard timeout) | typically root or the ZFS group | topology merge silently skipped (fail-soft) |
| 3 | `zpool events -fv` (long-running follower subprocess) | typically root or the ZFS group | error map shows no events; observation continues |

## 2. Data sources in detail

### 2.1 `/proc/spl/kstat/zfs/<pool>/scan` — the scan kstat

The scan kstat is the **source of truth for scan numbers**. The parser maps
fields **by column name, position-independently**, skips unknown names, and
tolerates releases that add or reorder rows — verified against fixtures
from OpenZFS 2.2.9.

| Aspect | Behavior |
|---|---|
| Source of truth | scan numbers: `func`, `state`, start/end time, and the byte/error counters (`to_examine`, `examined`, `processed`, `skipped`, `errors`, `issued`, `pass_exam`) |
| `func` mapping | `1` → scrub, `2` → resilver; anything else (including `0`/`POOL_SCAN_NONE`) → no scan record |
| `state` mapping | `1` → scanning, `2` → finished, `3` → canceled, else → idle |
| Missing file | parsed as `Ok(None)` — a pool that has never scanned has no `scan` file: absence, not failure |
| All-zero row | parsed as `None` — `start_time 0 / end_time 0` means "no scan ever ran"; zeros are not surfaced as a 0-byte scan |
| Pool enumeration | directory read of `/proc/spl/kstat/zfs/`; per-pool directories are distinguished from global kstat files (arcstats, dmu_tx, ...) by the `guid` entry — unprivileged on every build verified |

### 2.2 `zpool status -v` — the status parser

Human-formatted output that varies across OpenZFS releases and locales, so
every field is parsed fail-soft: unrecognized lines are skipped, missing
fields become `None`. The subprocess gets a hard 5-second timeout; spawn
failure, non-zero exit, or timeout all return an error, and the caller
responds by simply not merging topology this tick.

It provides four distinct things:

| # | Provides | Detail |
|---|---|---|
| 1 | Vdev topology | the indented NAME/STATE tree, folded by indent into a parent/child structure |
| 2 | Per-vdev state and error counters | `ONLINE`/`DEGRADED`/`FAULTED`/`OFFLINE`/`REMOVED`/`UNAVAIL` tokens plus the trailing READ/WRITE/CKSUM integers of each row |
| 3 | The `scan:` line | human wording such as `resilver in progress since ... 37.5% done` or `resilvered 2.00G in 00:00:07 with 0 errors`, including indented continuation lines; sizes are parsed as decimal SI units because that is what `zpool status` prints |
| 4 | dRAID rebuild wording | per-vdev lines like `resilver (draid2-0-0) in progress since ...` / `resilvered (draid2-0-0) 2.00G in 0:00:05 with 0 errors on ...`, probed by feature detection rather than version strings |

### 2.3 `zpool events -fv` — the events follower

A supervised follower subprocess spawned once when live mode starts,
streaming ZFS ereports into a bounded 100-entry ring buffer that the render
loop drains each tick. The kernel event queue is not consumed destructively
by readers, so multiple followers (and multiple zresmon instances) may
coexist. Dropping the follower kills the child — no zombies, no orphan
`zpool` processes, even on panic.

The incremental parser is build-tolerant in two verified ways:

| Tolerance | Detail |
|---|---|
| Row forms | accepts both `key value` rows and `key = value` rows (the 2.2.9 vendor build emits the equals-sign form), with quoted string values stripped |
| Payload spellings | accepts both `io_offset` and `zio_offset`, and both `ereport.io.fs.zfs.io` and `ereport.fs.zfs.io` class paths, by matching on suffixes |
| Kept families | only classes ending in `checksum` and classes ending in `.zfs.io`; everything else (`probe_failure`, `sysevent`, ...) is dropped as uninteresting |

## 3. Situation-by-situation: what is read, and when

### Idle (no scan)

The scan kstat file is absent (never scanned) or all-zero → the snapshot
carries `scan: None` and the UI shows no gauge — this is the documented
`scan: none` behavior, **not** an error (README *Data sources*). The pool
itself remains fully observable: topology, vdev states, and counters still
come from `zpool status -v`, and the pool still has a tab.

### Scanning (RESILVER / SCRUB / ERROR SCRUB)

While a conventional scan runs, the kstat file exists and is re-read every
poll: `func`/`state` drive the gauge label and lifecycle badge;
`examined`/`to_examine` drive the progress ratio and byte display;
`errors` drives the error count. The `zpool status` percentage, when
present, is stamped onto the rebuild-target vdev as `rebuild_pct` — it
*complements* the kstat examined ratio rather than replacing it.

Note on ERROR SCRUB: the model and UI vocabulary include it, but the
current kstat parser maps only `func` 1 (scrub) and 2 (resilver) — no live
collector produces the ErrorScrub variant today.

### Finished / Canceled

ZFS **removes the scan kstat file after a scan completes** — normal kernel
behavior, not data loss (README *Data sources*). From that moment the
kstat path returns `None`, and the fallback chain turns to the `zpool
status` scan line: `resilvered 2.00G in ... with 0 errors` parses into a
Finished ScanStats carrying the real byte total and error count (observed
on the lab VM as `resilvered 129M ... with 0 errors`). A `% done` wording,
when present instead of bytes, becomes `progress_override` so the gauge
never shows synthesized numbers.

### Rebuild (dRAID distributed spare)

dRAID sequential rebuild does not reliably surface through the scan kstat
on all builds (on the 2.2.9 vendor build the scan kstat/nvlist is absent
post-rebuild). The rebuild wording probe is the third and last rung of the
chain: it scans the full status output for per-vdev
`resilver (vdev) ...` / `resilvered (vdev) N ... with X errors` wording
and synthesizes a resilver ScanStats from it, with
`progress_override = 1.0` on completion. This is deliberately **feature
detection, not version detection** — some custom builds omit the wording
entirely, in which case the integration matrix falls back to event-based
assertions (README *Build-variant note*).

### Degraded / Faulted pool

Vdev health comes solely from `zpool status -v`: the parsed tree drives
per-vdev state colors, non-zero R/W/C counter display, and the tab
markers — red `✚` for pools with faulted/unavail/removed vdevs, yellow
`!` for degraded, `⟳` while scanning. Health grading maps `Offline` to
*healthy* on purpose: taking a vdev offline is an intentional operator
action, not an alarm. Because health is recomputed from each poll's tree
with no cached state, a pool reverts to a clean tab automatically once
replacement vdevs are back ONLINE.

### Error events (the surface map)

The error surface map is fed exclusively by the `zpool events -fv`
follower: each kept ereport's `io_offset` is bucketed per vdev into heat
strips, worst device first, with a 120-second sliding window (README
*What it shows*). The map is a logical-offset approximation, not platter
geometry.

## 4. The scan fallback chain

Scan state is resolved by a three-step chain, evaluated in this order
every poll:

```text
        ┌─────────────────────────────────────────────┐
        │ 1. scan kstat                               │
        │    /proc/spl/kstat/zfs/<pool>/scan          │
        │    exact counters — the source of truth     │
        └───────────────────┬─────────────────────────┘
                            │ file absent?
                            │ (never scanned · removed after
                            │  completion · build quirk)
        ┌───────────────────▼─────────────────────────┐
        │ 2. zpool status "scan:" line                │
        │    conventional scrub/resilver wording;     │
        │    % done → progress_override,              │
        │    bytes → examined/to_examine              │
        └───────────────────┬─────────────────────────┘
                            │ no scan section / wording?
        ┌───────────────────▼─────────────────────────┐
        │ 3. dRAID rebuild wording probe              │
        │    "resilver (vdev) ..." per-vdev lines;    │
        │    Finished → progress_override = 1.0       │
        └───────────────────┬─────────────────────────┘
                            │ no wording either?
                scan stays None → idle (absence, not error)
```

Each rung covers the situations the previous one cannot: the kstat covers
active conventional scans with exact numbers; the scan line covers
post-completion reporting once the kstat file is gone; the rebuild probe
covers dRAID sequential rebuilds that never appear in either. If nothing
matches, the tool concludes *no scan record* — never *error*.

## 5. Privileges & graceful degradation

| You have | kstat (pools + scan) | `zpool status` (topology/health) | `zpool events` (error map) |
|----------|--------------------|----------------------------------|----------------------------|
| unprivileged user | ✓ full | ✗ typically denied → kstat-only view | ✗ typically denied → empty map |
| root / ZFS group | ✓ | ✓ | ✓ |

The philosophy is **fail-soft: partial information beats a dead tool**.
Concretely:

- `zpool status` fails (missing binary, permissions, timeout) → the
  snapshot keeps its kstat scan numbers with a placeholder Online
  topology; only topology-dependent views (vdev tree, tab markers) go
  quiet for that pool.
- the events follower fails to spawn → the follower is dropped, the map
  shows all-zero strips, and everything else keeps working.
- an unreadable kstat file that *exists* (permissions changed mid-read)
  is a genuine error and is surfaced as such — the only kstat failure
  that is.

## 6. Version-dependent behavior

Verified against OpenZFS 2.2.9 (vendor build) on Rocky 8. Anything below
may differ on other releases and is deliberately handled by tolerant
parsing rather than assumed:

- **kstat file lifetime** — removal after scan completion, and the 2.2.9
  build's absent-even-while-running / rebuild-zap numbering collision
  (CHANGELOG *Verified*), are why the fallback chain exists at all.
- **ereport field spelling** — `io_offset` vs `zio_offset`, and
  `ereport.io.fs.zfs.*` vs `ereport.fs.zfs.*`; both accepted.
- **scan-line and rebuild wording** — `%` placement on the same line vs
  the next indented line, and per-vdev rebuild wording presence.
- **I/O direction** — the ereport class alone does not distinguish read
  from write; events are approximated as reads.
- **kstat field set** — unknown columns are skipped, not rejected, so
  releases that add fields keep parsing.

---

*Sources: the scan kstat, `zpool status -v`, and `zpool events -fv` —
collected by the `collect` module, rendered by the TUI.*
