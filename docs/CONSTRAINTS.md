# Constraints — Lab Test Environment

> Tested/observed on: OpenZFS **2.2.9**, file-vdev pools under
> `/var/tmp/zresmon-lab`. The version boundary table below also covers

---

## 1. Kernel wedge: scrub overlapping a sequential rebuild with active read injection

### Symptom

`zpool scrub <pool>` on a draid pool wedges in kernel **D-state**
(uninterruptible sleep) — the scrub's I/O never completes, and
`zpool clear/export/destroy` on the affected pool are refused while wedged.

Observed on the lab node during matrix runs, in the healing-with-errors
case on the draid pool.

### Trigger conditions (all three required, as observed)

1. A **read-injection handler active** on a surviving leaf
   (`zinject -d <leaf> -T read -f 50 <pool>`)
2. A **sequential rebuild in flight or just completed**
   (draid distributed-spare consumption via `zpool replace -f <pool> <leaf>`)
3. A **scrub issued over that state**
   (`zpool scrub <pool>`)

Removing the injection handler (`zinject -c all`) did **not** immediately clear
the wedge — the queued ZIOs stayed parked. Resolution required a **node reboot**
(known appliance behavior: rebooting with a suspended/stuck pool hangs shutdown
for ~10–50 minutes, then self-resolves).

### Root cause boundary

The wedge is in the **kernel ZFS I/O layer of the OpenZFS 2.2.9 build**. It is NOT:

- a zresmon defect (zresmon only observes; the wedge reproduces with plain
  `zpool scrub` from a shell), and
- verified on other OpenZFS versions — **2.3.x / 2.4.0 / 2.5-dev (master) are
  untested.** Only 2.2.9 (vendor) and 2.4.1 (Ubuntu) have live-run data.

### Mitigations in the harness (all in `tests/lab_matrix.rs`)

| Mitigation | Location | Effect |
|---|---|---|
| healing-with-errors (draid) skips the determinism scrub | c2 loop: `if kind != "draid"` | The wedge trigger (scrub over a fresh sequential rebuild + injection) is never issued for draid. Signal coverage for the draid leg of healing-with-errors relies on the deterministic `root=Degraded` outcome (the offline leaf is not repaired — the distributed spare is already consumed), which requires no scrub. |
| scrub bounded by `timeout -k 30 300` | `scrub_with_retry` | If a wedge occurs anyway, the wrapper gives up after 300s (+30s SIGKILL grace): the case FAILs loudly and the matrix continues. Note: a D-state child may ignore SIGKILL — `timeout` then returns 137 and the child leaks until the node reboots. The matrix still proceeds. |
| snapshot bounded by `timeout 60` | `snap()` | Observation commands cannot wedge a case. |
| setup hard gate | `case_setup` | Leftover zr\* pools abort the run immediately instead of cascading. |
| setup failure aborts the run | `lab_matrix` ABORT GATE | Remaining cases are skipped instead of running on a polluted lab (this is what turned a stuck destroy into a full suspend incident on 2026-08-31 — see the incident report). |

### Recovery when it happens (on the node)

```bash
zinject -c all                       # remove handlers first (may unstick the scrub)
# if a scrub process remains in D-state: it cannot be killed —
systemctl reboot -i                  # known appliance issue: shutdown hangs ~10–50 min, then completes
# after boot: zr* pools auto-import via cachefile — the matrix setup tears them down
```

---

## 2. Why the healing-with-errors (draid) exception loses no assertion power

The case's purpose is: errors injected on a surviving leaf MUST surface observably
during the repair I/O. For draid on this build:

- the distributed spare is consumed by the earlier **draid** case, so the
  case's `zpool replace` fails ("distributed spare exhausted?") and **no resilver runs**
  — the injected leaf would not be read anyway;
- the deterministic signal is already present: the offline leaf keeps the pool
  **Degraded** (`root.state == Degraded`).

So skipping the scrub for draid removes no assertion — it removes only the wedge
trigger. If a future build tolerates scrub-over-rebuild, remove the
`kind != "draid"` exception in `case_c2` and the draid depth matches the other
layouts.

---

## 3. Version boundary — what is known vs untested

| Environment | Wedge behavior | Basis |
|---|---|---|
| OpenZFS 2.2.9 | **Observed twice**, D-state 40+/18+ min | live runs |
| OpenZFS 2.4.1 | **NOT observed** — full matrix **17/17 pass** , including scrubs over draid rebuilds and the healing-with-errors injection flow | live run on the 2.4.1 node |
| OpenZFS 2.3.x / 2.4.0 / 2.5-dev (master) | **Untested** | not present on the available nodes |

Additional 2.4.1 compatibility finding: `zpool replace` of an **OFFLINE vdev
without `-f` is rejected on 2.4.1** (allowed on 2.2.9). `scripts/lab.sh`
cmd_replace now uses `-f` unconditionally — verified on both 2.2.9 and 2.4.1.

Do not claim "fixed in newer ZFS" or "a build-specific bug" without data.
**Partial data (2026-09-01):** the wedge did NOT reproduce on OpenZFS 2.4.1
(Ubuntu) across a full 17-case matrix — consistent with (but not proving) an
OpenZFS 2.2.9-era or build-specific issue. The healing-with-errors (draid) scrub exception is kept as a
2.2.9-node safeguard; re-test on future builds.

### Version differences observed (2.2.9 → 2.4.1)

| Behavior | OpenZFS 2.2.9 | OpenZFS 2.4.1 |
|---|---|---|
| `zpool replace <pool> <OFFLINE leaf> <spare>` **without** `-f` | Accepted (heal passed on the first run) | **Rejected** — "is part of active pool … use '-f' to override". Fixed by passing `-f` unconditionally in `scripts/lab.sh` (harmless on 2.2.9, verified on both). |
| `zpool replace -f <pool> <leaf>` target-less (draid spare consumption) **without** `-f` | Accepted (draid case passed) | **Rejected** without `-f` — the lab.sh draid branch already used `-f` from the start. |
| Scrub issued over a draid pool with a recent sequential rebuild + active read injection | **Wedge** (kernel D-state, 40+/18+ min, twice) | **No wedge** — scrubs over rebuilds completed (the recovery, rebuild-cascade and mid-rebuild cases passed) |
| Scrub repair restarting a resilver (healing-with-errors repair → recovery-scrub collision) | Observed ("currently resilvering") | Observed — same behavior; handled by `scrub_with_retry` |

Notes:

- The exact change that added the `-f` requirement landed somewhere between
  2.2.9 and 2.4.1 (2.3.x untested — no 2.3 node available).
- zpool-replace(8) in 2.4.1 documents that a replacement **cancels any
  in-progress scrub** — issuing replace first and scrubbing after it settles
  (the harness order) is the supported sequence.
- The healing-with-errors (draid) scrub skip is a 2.2.9-build safeguard. If a future ZFS version
  proves scrub-over-rebuild safe, remove the `kind != "draid"` guard in
  `case_c2` and the draid depth matches the other layouts.

---

## 4. Related notes

- Fill design: `cmd_fill` writes one persistent 512 MB file per pool
  (`fill.bin`) and advances txgs with **transient** 16 MB warm chunks that are
  removed immediately. Consequence: the replace-resilver may transfer only
  kilobytes, which is why the healing-with-errors case's scrub (or, for draid, the skipped scrub
  exception) matters — see `case_c2` in `tests/lab_matrix.rs`.
- The 2026-08-31 suspend incident (pool suspend after its vdev backing files
  were unlinked while imported, and the reboot-hang cycle that followed) is
  documented separately on the operator side
  (`~/documents/zresmon-zrm0-suspend-incident.html` on the dev machine).
- Process rule learned from that incident: never leave a manual repro lab
  running; always `lab.sh teardown` + `zinject -c all` afterwards.
