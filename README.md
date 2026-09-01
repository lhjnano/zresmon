# zresmon — Lock-free ZFS resilver/scrub monitor TUI

Stateless observation of ZFS resilver/scrub progress: disk boxes,
RPM-style gauges, retry badges, and error surface maps — with no locks,
no PID files, and no mutations against the pool.

## What it looks like

```text
┌─ zresmon ────────────────────────────────────────────────────────────┐
│ [1: tank✚] [2: zrm0⟳] [3: zr22!]                    2/3             │
│ pool: zrm0     RESILVER  ############------[ 45.0%]                  │
│ scan: RESILVER SCANNING  errors=4219  54.0 MiB / 120.0 MiB           │
│ ┌─ vdev tree ─────────────────────┐ ┌─ error surface map ──────────┐ │
│ │ zrm0                            │ │ /var/tmp/…/zrm0-v2.img       │ │
│ │   mirror-0                      │ │   ██▓░·…  ▒░··  4.2K         │ │
│ │     zrm0-spare1.img  ONLINE   0│ │                               │ │
│ │     zrm0-v2.img      ONLINE 4.2K│ │ /var/tmp/…/zrm0-v1.img       │ │
│ │                                 │ │   ····  ░···  0              │ │
│ │                                 │ │ density: ·0 ░1-2 ▒3-6        │ │
│ │                                 │ │ ▓7-15 █16+ ev/2min           │ │
│ └─────────────────────────────────┘ └──────────────────────────────┘ │
│ q quit · ←/→/Tab pool · 1-9 jump · h/l panel · ↑/↓ scroll · read-only│
└─ ⟳ RETRY ────────────────────────────────────────────────────────────┘
```

- **Tab bar** — one tab per pool, with health markers:
  `✚` red (faulted/unavail/removed), `!` yellow (degraded), `⟳` (scanning),
  cyan+bold (active)
- **Header** — pool name, scan type, RPM-style gauge, byte fraction
- **vdev tree** — indented topology with per-vdev state and R/W/C counters
- **Error surface map** — per-device heat strips (`·░▒▓█` density legend),
  worst-first, 120-second sliding window
- **Footer** — key hints, always visible
- **Badge** — `⟳ RETRY` or `✚ REPLACE` in the bottom-right corner

## What it shows

- Pool scan state (`RESILVER`/`SCRUB`/`ERROR SCRUB` + `IDLE/SCANNING/FINISHED/CANCELED`)
- RPM-install-style gauge progress (`##########------[ 45%]`) and examined/to-examine bytes
- Per-vdev tree with ONLINE/DEGRADED/FAULTED colors and read/write/checksum error counters
- Retry badge (`⟳ RETRY`) vs escalation badge (`✚ REPLACE` for FAULTED disks — retrying a dead device is pointless; replace it)
- Error surface map: per-device heat strips (ereport `io_offset` bucketed
  per vdev, worst-first, with density legend), 120-second sliding window
- Multi-pool tab bar with per-pool health markers: red `✚` for pools with
  faulted/unavail/removed vdevs, yellow `!` for degraded, `⟳` while scanning
- Panel focus highlight: `h`/`l` switch focus between the vdev tree and the
  error map; the focused panel's title renders cyan+bold with a cyan border
- Korean IME fallback: hotkeys work even when the IME delivers `ㅂ` for `q`
  (two-set QWERTY position projection)

## Design

- No lock files / PID files; N instances may run concurrently
- Every poll is a self-contained read-only observation (kstat procfs, `zpool status -v`, `zpool events -fv`)
- `--once [--json]`: single snapshot to stdout

## Install

Standard Rust distribution:

```bash
# from a checkout — installs to ~/.cargo/bin (make sure it is on PATH)
cargo install --path .

# or plain build + copy yourself
cargo build --release
sudo cp target/release/zresmon /usr/local/bin/
```

`cargo install` produces a dynamically linked glibc binary. For older distros
(Rocky 8, CentOS 7) or any glibc mismatch, build the static musl binary — it
runs anywhere Linux does:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
sudo cp target/x86_64-unknown-linux-musl/release/zresmon /usr/local/bin/
```

Verified on Rocky 8 + OpenZFS 2.2.9 (musl static build).

## Usage

- `zresmon` — live, all pools with scan kstats
- `zresmon --pool tank` — single pool
- `zresmon --demo errors` — fixture scenarios: `scanning|done|errors|fault`
- `zresmon --once --demo fault` — one snapshot, text
- `zresmon --once --json` — machine readable

Keys:

- `q` / `Esc` — quit
- `→` / `Tab` — next pool · `←` / `BackTab` — previous pool
- `1`–`9` — jump to pool N (out of range: no-op)
- `h` / `l` — focus the vdev tree / the error surface map (focused title
  turns cyan+bold)
- `↑` / `↓` — scroll the focused panel

Hotkeys keep working with a Korean IME on (`ㅂ` quits, `ㅗ`/`ㅣ` move focus —
two-set QWERTY position projection). Full contract table:
[docs/KEYBINDINGS.md](docs/KEYBINDINGS.md).

## Data sources & privileges

- `/proc/spl/kstat/zfs/<pool>/scan` (unprivileged)
- `zpool status`/`events` (root or the ZFS group typically required; the tool degrades gracefully)
- Note: the scan kstat file is removed after a scan completes — this is normal
  ZFS behavior and surfaces as `scan: none`, not an error.

Situation-by-situation monitoring reference: [docs/ZFS-MONITORING.md](docs/ZFS-MONITORING.md).

## Lab: file-vdev test bench

`scripts/lab.sh` builds pools per RAID type (mirror=2, raidz1=3, raidz2=4
vdevs, file-backed), fills them, takes up to 2 vdevs through a
fail->replace cycle to trigger resilver, and leaves watching to you in the
zresmon TUI. Requirements: root + OpenZFS module (a VM is fine — the
preflight gate refuses otherwise).

Standard workflow (two terminals):

```bash
# terminal 1 (lab)
sudo ./scripts/lab.sh setup                        # pools zrm0/zr11/zr22/zrd3 + spares
sudo ./scripts/lab.sh fill --txg 1000 --mb 512     # data + advance 1000 txgs
sudo ./scripts/lab.sh fail                         # one vdev offline (DEGRADED)

# terminal 2 (observation) — start before replace
zresmon

# terminal 1 — trigger resilver (banner appears; watch terminal 2)
sudo ./scripts/lab.sh replace
```

Options: `fail --dual` (simultaneous dual failure — raidz2 only;
mirror/raidz1 would destroy the pool and are refused) · `fail/replace
--capture` (dump state into `fixtures/`: zpool status -v, iostat, scan kstat
raw, zresmon --once --json) · `capture --label NAME` · `teardown
[--keep-pools]` · `status`.

Observation points: right after `fail`, the DEGRADED/FAULTED box colors →
after `replace`, the RESILVER gauge and retry badge → after completion,
FINISHED with all vdevs back ONLINE.

Cleanup: `sudo ./scripts/lab.sh teardown` (destroys only `zr*`-prefixed
pools — safety guard).

## Testing

### Unit tests

```bash
cargo test
```

Parsers (including missing-file, format-drift, and dRAID rebuild wording
fixtures), demo scenarios, UI helpers (NaN guard, Korean IME fallback),
health grading, and the events follower with a fake-zpool shim.

### Integration matrix (lab node — root + ZFS required)

The matrix is a Rust integration test (`tests/lab_matrix.rs`) that drives
`scripts/lab.sh` end-to-end and cross-validates `zresmon --once --json`
snapshots against zpool itself — typed assertions via serde, no jq needed.
`cargo test` compiles it but skips it (ignored), so CI stays green without
ZFS:

```bash
# dev machine / CI: unit tests only (the matrix compiles but does not run)
cargo test --all

# ZFS lab node (root): the full matrix — one command, sequential, unattended
sudo cargo test --test lab_matrix -- --ignored --nocapture

# single case group (setup/teardown always run; the filter picks body cases)
ZRESMON_MATRIX_FILTER=draid sudo cargo test --test lab_matrix -- --ignored --nocapture

# keep the lab pools after the run
ZRESMON_MATRIX_KEEP=1 sudo cargo test --test lab_matrix -- --ignored --nocapture
```

On a lab node without cargo, build the harness binary statically on a dev
machine and run it directly:

```bash
# dev machine (static — runs on any glibc):
cargo test --target x86_64-unknown-linux-musl --no-run
scp target/x86_64-unknown-linux-musl/debug/deps/lab_matrix-<hash> node:

# node: point ZRESMON_BIN at the installed zresmon (musl) binary
ZRESMON_BIN=/usr/local/bin/zresmon sudo ./lab_matrix-<hash> --ignored --nocapture
```

Methodology notes:

- Falsifiable assertions: deterministic outcomes are REQUIRED (e.g. a
  mirror must survive 3-way spare exhaustion; raidz2 must survive dual
  failure; every layout must keep serving writes under a single fault);
  build-variable outcomes (vendor-build quirks) fall back to cross-validating the
  zresmon snapshot against zpool health.
- Skips are counted separately from passes — a missing pool is never a
  silent PASS. Every destructive op is gated on lab-pool prefixes so real
  pools (if any) stay untouched.
- All waits are event-driven (`wait_scan` polling), not fixed sleeps; fill
  is sized at `--txg 500 --mb 512 --max-min 5` (~1–3 min per pool, measured
  on the lab node) — enough dirty data for meaningful resilvers.

Coverage matrix (per RAID type where applicable):
| Scenario | mirror | raidz1 | raidz2 | draid |
|----------|--------|--------|--------|-------|
| Spare exhaustion (replace until spares run out, then one more fault) | ✓ | ✓ | ✓ | ✓ |
| Healing with injected read errors (fail → inject → replace) | ✓ | ✓ | ✓ | ✓ |
| Recovery after errors (clear + scrub) | ✓ | ✓ | ✓ | ✓ |
| Mid-rebuild failure (second fault while rebuilding) | — | — | — | ✓ |
| Sequential rebuild → healing → scrub cascade | — | — | — | ✓ |
| Write-injection suspend | ✓ | ✓ | ✓ | ✓ |
| Service continuity under a single fault | ✓ | ✓ | ✓ | ✓ |
| Replace back to the original disk | ✓ | ✓ | ✓ | ✓ |
| Cascading replace (replacement also fails) | ✓ | ✓ | ✓ | ✓ |
| Core scenarios: heal, dual failure, injection, clear, export/import, safety guard | ✓ | ✓ | ✓ | ✓ |

Build-variant note: dRAID sequential rebuild completion is detected by
feature probe (rebuild wording in `zpool status`), not version string —
some custom builds omit the wording entirely, in which case the matrix
falls back to event-based assertions (`resilver_finish` + distributed
spare consumption), which pass on all builds tested.

