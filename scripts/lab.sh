#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) 2026 HeonJe LEE
#
# zresmon lab — file-vdev based ZFS test bench.
# Sets up pools (setup -> fill[txg advance] -> fail -> replace[resilver]) and
# leaves observation to the user running the zresmon TUI live in another
# terminal. Lab state lives in a manifest — the lock-free principle applies
# to the zresmon binary only; this script is the mutating test-fixture tool.
#
# Usage: lab.sh <setup|fill|fail|replace|capture|teardown|status> [options]

set -u

LAB_DIR=${ZRESMON_LAB_DIR:-/var/tmp/zresmon-lab}
MANIFEST="$LAB_DIR/manifest"
FIXTURES_DIR="$(cd "$(dirname "$0")/.." && pwd)/fixtures"
SPARES_PER_POOL=2

die() { echo "ERROR: $*" >&2; exit 1; }
say() { echo "==> $*"; }
warn() { echo "WARN: $*" >&2; }

usage() {
  sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
  cat <<'U'
Subcommands:
  setup   [--layout mirror:2,raidz1:3,raidz2:4[,draid2:6:1]] [--size 1G]   create pools / file vdevs
  fill    [--txg 1000] [--mb 512] [--max-min 20]             write data + advance txgs
  fail    [--pool NAME] [--count 1] [--dual] [--capture]      take vdev(s) offline
  replace [--pool NAME] [--capture]                           swap in spares -> resilver
  capture [--label NAME]                                      dump state -> fixtures/
  teardown [--keep-pools]                                     cleanup
  status                                                      current summary
  inject [--pool NAME] --pct 20                               zinject read errors
  inject --clear                                              clear all injections
U
}

# ---------- common ----------

require_root() { [ "$(id -u)" -eq 0 ] || die "root required (EUID=$(id -u))"; }
require_zfs() {
  command -v zpool >/dev/null 2>&1 || die "zpool binary not found — install zfsutils-linux"
  [ -d /proc/spl/kstat/zfs ] || die "ZFS kernel module not loaded — modprobe zfs and retry"
}

preflight() {
  require_root
  require_zfs
}

# manifest format: each line = pool type vdev1,vdev2,... spare1,spare2
manifest_pools() { [ -f "$MANIFEST" ] || die "no manifest — run setup first ($MANIFEST)"; cut -d' ' -f1 "$MANIFEST"; }
manifest_line() { grep "^$1 " "$MANIFEST" || die "pool $1 not in manifest"; }

last_txg() { # pool -> last txg number
  awk -F'\t' '/^[0-9]+/ {t=$1} END {print t+0}' "/proc/spl/kstat/zfs/$1/txgs" 2>/dev/null || echo 0
}

leaf_vdevs() { # split the vdev field (3rd) of a manifest line by comma
  local line; line=$(manifest_line "$1"); echo "${line#* }" | cut -d' ' -f1 | tr ',' '\n'
}

# ---------- setup ----------

cmd_setup() {
  local layout="mirror:2,raidz1:3,raidz2:4" size=1G
  while [ $# -gt 0 ]; do case "$1" in
    --layout) layout="$2"; shift 2;; --size) size="$2"; shift 2;;
    *) die "setup: unknown option $1";; esac; done

  preflight
  mkdir -p "$LAB_DIR"
  [ -f "$MANIFEST" ] && die "lab already set up — run teardown first (manifest: $MANIFEST)"

  local need_gb=0 pools_ok=""
  IFS=',' read -ra ENTRIES <<< "$layout"
  for e in "${ENTRIES[@]}"; do
    local t=${e%%:*} rest=${e#*:} n
    # draidN:children:spares — e.g. draid2:6:1 (data children + distributed spares)
    if [[ "$t" == draid* ]]; then
      n=${rest%%:*}
      local dspares=${rest##*:}
      [ "$n" -ge 3 ] || die "$t needs at least 3 children"
      [[ "$dspares" =~ ^[0-9]+$ ]] || die "$t spares must be numeric: $t:children:spares"
    else
      n=${e##*:}
      case "$t" in
        mirror) [ "$n" -ge 2 ] || die "mirror needs at least 2 vdevs" ;;
        raidz1) [ "$n" -ge 3 ] || die "raidz1 needs at least 3 vdevs (recommended)" ;;
        raidz2) [ "$n" -ge 4 ] || die "raidz2 needs at least 4 vdevs (recommended) — cannot create with 2" ;;
        *) die "unknown type $t (mirror|raidz1|raidz2|draidN:c:s)" ;;
      esac
    fi
    pools_ok+="$e "
    need_gb=$((need_gb + (n + SPARES_PER_POOL)))
  done
  # free-space check (size approximated as integer GB)
  local free_kb avail_kb
  free_kb=$(df -Pk "$LAB_DIR" | awk 'NR==2 {print $4}')
  avail_kb=$((need_gb * 1024 * 1024 * 3 / 2))  # size x 1.5 (GB approx)
  [ "$free_kb" -lt "$avail_kb" ] && warn "possibly low on space (need ~$((avail_kb/1024))MiB, have $((free_kb/1024))MiB)"

  : > "$MANIFEST"
  local suffix_num=0
  for e in "${ENTRIES[@]}"; do
    local t=${e%%:*} rest=${e#*:} n create_type pool
    if [[ "$t" == draid* ]]; then
      n=${rest%%:*}
      # zpool syntax: draid<parity>[:<n>c][:<n>d][:<n>s] — the children:spares
      # input is simplified to spares-only '<n>s'.
      create_type="draid${t#draid}:${rest##*:}s"
      pool="zrd$suffix_num"
    else
      n=${e##*:}
      create_type=$t
      case "$t" in mirror) pool="zrm$suffix_num";; raidz1) pool="zr1$suffix_num";; raidz2) pool="zr2$suffix_num";; esac
    fi
    while zpool list -H -o name 2>/dev/null | grep -qx "$pool"; do
      suffix_num=$((suffix_num+1))
      case "$t" in mirror) pool="zrm$suffix_num";; raidz1) pool="zr1$suffix_num";; raidz2) pool="zr2$suffix_num";; draid*) pool="zrd$suffix_num";; esac
    done
    local files="" f
    for i in $(seq 1 "$n"); do
      f="$LAB_DIR/${pool}-v${i}.img"
      truncate -s "$size" "$f" || die "vdev file creation failed: $f"
      files+="$f,"
    done
    files=${files%,}
    local spares="" s
    for i in $(seq 1 $SPARES_PER_POOL); do
      s="$LAB_DIR/${pool}-spare${i}.img"
      truncate -s "$size" "$s" || die "spare file creation failed: $s"
      spares+="$s,"
    done
    spares=${spares%,}
    say "creating pool: $pool ($create_type, $n vdevs, $size)"
    # shellcheck disable=SC2086
    zpool create -f "$pool" $create_type ${files//,/ } || die "zpool create failed: $pool"
    zfs set compression=off "$pool" || warn "failed to set compression=off"
    echo "$pool $t $files $spares" >> "$MANIFEST"
    suffix_num=$((suffix_num+1))
  done
  say "setup complete. observe: run zresmon (or zresmon --pool <name>) in another terminal"
  cat "$MANIFEST"
}

# ---------- fill ----------

cmd_fill() {
  local txg_target=1000 mb=512 max_min=20
  while [ $# -gt 0 ]; do case "$1" in
    --txg) txg_target="$2"; shift 2;; --mb) mb="$2"; shift 2;; --max-min) max_min="$2"; shift 2;;
    *) die "fill: unknown option $1";; esac; done
  preflight

  local pool line t vdevs spares
  while read -r pool t vdevs spares; do
    [ -z "${pool:-}" ] && continue
    say "[$pool] writing ${mb}MiB (compression=off)"
    dd if=/dev/urandom of="/$pool/fill.bin" bs=8M count=$((mb/8)) status=none \
      || die "dd failed — check free space"
    local start_txg now_txg
    start_txg=$(last_txg "$pool")
    say "[$pool] txg advance: $start_txg -> target +$txg_target (max ${max_min}min)"
    local deadline=$((SECONDS + max_min * 60)) chunk=0
    while :; do
      now_txg=$(last_txg "$pool")
      if [ $((now_txg - start_txg)) -ge "$txg_target" ]; then
        say "[$pool] target reached: txg $now_txg (+$((now_txg - start_txg)))"
        break
      fi
      if [ $SECONDS -ge $deadline ]; then
        warn "[$pool] timed out: txg $now_txg (+$((now_txg - start_txg))) — lowering zfs_txg_timeout=1 can accelerate"
        break
      fi
      chunk=$((chunk + 1))
      dd if=/dev/urandom of="/$pool/warm-$chunk.bin" bs=1M count=16 status=none 2>/dev/null || true
      rm -f "/$pool/warm-$chunk.bin"
      zpool sync "$pool" 2>/dev/null || true
      sleep 1
      [ $(((now_txg - start_txg) % 50)) -eq 0 ] && [ "$now_txg" -gt "$start_txg" ] \
        && say "[$pool] progress: +$((now_txg - start_txg))/$txg_target"
    done
  done < "$MANIFEST"
  say "fill done — now run fail -> replace to drive the resilver scenario"
}

# ---------- fail ----------

cmd_fail() {
  local pool_opt="" count=1 dual=0 do_capture=0
  while [ $# -gt 0 ]; do case "$1" in
    --pool) pool_opt="$2"; shift 2;; --count) count="$2"; shift 2;;
    --dual) dual=1; shift;; --capture) do_capture=1; shift;;
    *) die "fail: unknown option $1";; esac; done
  preflight

  local pool t vdevs spares
  while read -r pool t vdevs spares; do
    [ -z "${pool:-}" ] && continue
    if [ -n "$pool_opt" ] && [ "$pool" != "$pool_opt" ]; then continue; fi
    if [ "$dual" -eq 1 ] && [ "$t" != raidz2 ]; then
      die "simultaneous dual failure (--dual) is raidz2-only — $t($pool) would destroy the pool"
    fi
    # Current ONLINE leaves from zpool status (NOT the manifest — vdevs may
    # have been replaced by earlier runs, consuming manifest entries).
    local leaves
    leaves=$(zpool status -v "$pool" 2>/dev/null \
        | awk '/\/var\/tmp\/zresmon-lab\/.*\.img/ && /ONLINE/ {print $1}' \
        | grep -F "$LAB_DIR" | head -"$((dual == 1 ? 2 : ${count:-1}))")
    [ -z "$leaves" ] && { warn "[$pool] no ONLINE lab vdev found — nothing to fail"; continue; }
    local v failed=""
    while IFS= read -r v; do
      say "[$pool] offline: $v"
      zpool offline "$pool" "$v" || { warn "offline failed: $v"; continue; }
      failed+="$v "
    done <<< "$leaves"
    [ -z "$failed" ] && continue
    say "[$pool] DEGRADED — check zresmon in the other terminal (3s)"
    sleep 3
  done < "$MANIFEST"
  [ "$do_capture" -eq 1 ] && cmd_capture "fail"
  say "fail done — run replace to swap spares and trigger resilver"
}

# ---------- replace ----------

resilver_done() { # false while the pool scan: line still shows in-progress wording
  # Every in-progress form (verified against cmd/zpool/zpool_main.c):
  #   "resilver in progress since ..."           — conventional resilver
  #   "resilver (<vdev>) in progress since ..."  — draid sequential rebuild
  #   "scrub in progress/paused since ..."       — concurrent scrub
  #   + "rebuild" kept for wording variants on other ZFS versions.
  # Completed lines ("resilvered ...", "scrub repaired ...") never match —
  # note bare "resilver"/"rebuilt" cannot be markers: they persist after
  # completion ("resilvered (vdev) 30K in ...").
  local scan
  scan=$(zpool status "$1" 2>/dev/null | grep 'scan:' | head -1)
  case "$scan" in
    *"in progress"*|*"scrub paused"*|*"rebuild"*) return 1 ;;
    *) return 0 ;;
  esac
}

cmd_replace() {
  local pool_opt="" do_capture=0
  while [ $# -gt 0 ]; do case "$1" in
    --pool) pool_opt="$2"; shift 2;; --capture) do_capture=1; shift;;
    *) die "replace: unknown option $1";; esac; done
  preflight

  local pool t vdevs spares
  while read -r pool t vdevs spares; do
    [ -z "${pool:-}" ] && continue
    if [ -n "$pool_opt" ] && [ "$pool" != "$pool_opt" ]; then continue; fi
    # Offline/degraded leaves as they exist in the pool right now.
    local bad
    bad=$(zpool status -v "$pool" 2>/dev/null \
        | awk '/\/var\/tmp\/zresmon-lab\/.*\.img/ && /OFFLINE|DEGRADED|FAULTED|UNAVAIL/ {print $1}')
    [ -z "$bad" ] && { warn "[$pool] nothing to replace (no offline vdev) — run fail first"; continue; }
    # Spares not yet part of any pool (still raw files) — conventional
    # layouts only. draid pools rebuild onto their built-in distributed
    # spare (target device omitted below), so spare files are irrelevant.
    local -a avail_spares=()
    if [[ "$t" != draid* ]]; then
      local IFS=$'\n'
      local sp sp_name
      local IFS2="$IFS"; IFS=','
      read -ra SP_ALL <<< "$spares"
      IFS="$IFS2"
      for sp in "${SP_ALL[@]}"; do
        sp_name=$(basename "$sp")
        zpool status -v "$pool" 2>/dev/null | grep -q "$sp_name" || avail_spares+=("$sp")
      done
    fi
    local idx=0 replaced_any=0 v
    while IFS= read -r v; do
      if [[ "$t" == draid* ]]; then
        # draid: `zpool replace <pool> <old>` without a new device consumes
        # a distributed spare and drives a sequential rebuild (ZFS >= 2.1).
        # -f is required on OpenZFS 2.2.9 when the vdev is administratively
        # OFFLINE: without it zpool rejects with "… is part of active pool
        # … use '-f' to override" even though the swap is intra-pool.
        say "[$pool] replace (distributed spare → sequential rebuild): $v"
        if zpool replace -f "$pool" "$v"; then
          replaced_any=1
        else
          warn "[$pool] replace failed: $v (distributed spare exhausted?)"
        fi
        continue
      fi
      local spare=${avail_spares[$idx]:-}
      [ -z "$spare" ] && { warn "[$pool] spares exhausted — skipping $v"; break; }
      say "[$pool] replace: $v → $spare"
      # -f: required on 2.4.1 for replacing an OFFLINE vdev (2.2.9 allowed
      # it without -f; 2.4.1 rejects with "… is part of active pool … use
      # '-f' to override"). Harmless on 2.2.9 — same replace, forced.
      if zpool replace -f "$pool" "$v" "$spare"; then
        replaced_any=1
      else
        warn "[$pool] replace failed: $v (check spare state)"
      fi
      idx=$((idx + 1))
    done <<< "$bad"
    if [ "$replaced_any" -eq 1 ]; then
      echo "════════════════════════════════════════════════════"
      echo "  resilver started — watch in the other terminal:"
      echo "    zresmon --pool $pool"
      echo "════════════════════════════════════════════════════"
      local deadline=$((SECONDS + 1800))
      while ! resilver_done "$pool"; do
        sleep 2
        if [ $SECONDS -ge $deadline ]; then warn "[$pool] resilver wait timed out (30min) — continuing"; break; fi
      done
      say "[$pool] resilver done"
    else
      warn "[$pool] nothing to replace (no offline vdev) — run fail first"
    fi
  done < "$MANIFEST"
  # NOTE: a bare `[ cond ] && cmd` here would make the function return 1
  # whenever --capture is absent — i.e. lab.sh replace would exit 1 even on
  # full success (an if-statement returns 0 when the condition is false).
  if [ "$do_capture" -eq 1 ]; then
    cmd_capture "replace"
  fi
}

# ---------- inject ----------

cmd_inject() {
  local pool_opt="" pct="" clear=0
  while [ $# -gt 0 ]; do case "$1" in
    --pool) pool_opt="$2"; shift 2;;
    --pct) pct="$2"; shift 2;;
    --clear) clear=1; shift;;
    *) die "inject: unknown option $1";; esac; done
  preflight
  command -v zinject >/dev/null 2>&1 || die "zinject not found"

  if [ "$clear" -eq 1 ]; then
    zinject -c all && say "all injections cleared"
    return
  fi
  [ -z "$pct" ] && die "inject: --pct N required (or --clear)"

  # Inject read errors on one ONLINE lab leaf of each pool — the checksum
  # errors then surface during the next scrub/resilver pass.
  local pool t vdevs spares
  while read -r pool t vdevs spares; do
    [ -z "${pool:-}" ] && continue
    if [ -n "$pool_opt" ] && [ "$pool" != "$pool_opt" ]; then continue; fi
    local leaf
    leaf=$(zpool status -v "$pool" 2>/dev/null \
      | awk '/\/var\/tmp\/zresmon-lab\/.*\.img/ && /ONLINE/ {print $1}' | head -1)
    [ -z "$leaf" ] && { warn "[$pool] no ONLINE lab leaf"; continue; }
    if zinject -d "$leaf" -T read -f "$pct" "$pool"; then
      say "[$pool] injected ${pct}% read errors on: $leaf"
      say "  observe with: zresmon --pool $pool  (then scrub: zpool scrub $pool)"
    else
      warn "[$pool] zinject failed"
    fi
  done < "$MANIFEST"
}

# ---------- capture ----------

cmd_capture() {
  local label="${1:-manual}"
  if [ "${1:-}" = "--label" ]; then label="$2"; fi
  # arg-parse compat: lab.sh capture --label X
  while [ $# -gt 0 ]; do case "$1" in --label) label="$2"; shift 2;; *) shift;; esac; done
  preflight
  local dest
  dest="$FIXTURES_DIR/$(date -u +%Y%m%dT%H%M%SZ)-$label"
  mkdir -p "$dest" || die "fixtures creation failed: $dest"

  zpool status -v > "$dest/zpool-status.txt" 2>&1 || true
  zpool iostat -v 1 3 > "$dest/zpool-iostat.txt" 2>&1 || true
  timeout 2 zpool events > "$dest/zpool-events.txt" 2>&1 || true
  local pool
  if [ -f "$MANIFEST" ]; then
    while read -r pool _ _ _; do
      [ -z "${pool:-}" ] && continue
      cat "/proc/spl/kstat/zfs/$pool/scan" > "$dest/$pool-scan.kstat" 2>/dev/null \
        || echo "(no scan kstat)" > "$dest/$pool-scan.kstat"
      tail -5 "/proc/spl/kstat/zfs/$pool/txgs" > "$dest/$pool-txgs.tail" 2>/dev/null || true
    done < "$MANIFEST"
  fi
  # zresmon --once --json (binary auto-detected)
  local script_dir bin
  script_dir="$(cd "$(dirname "$0")" && pwd)"
  for bin in "$script_dir/../target/release/zresmon" "$script_dir/../target/debug/zresmon"; do
    if [ -x "$bin" ]; then "$bin" --once --json > "$dest/zresmon-snapshot.json" 2>&1; break; fi
  done
  [ -f "$dest/zresmon-snapshot.json" ] || echo "skipped: no zresmon binary" > "$dest/zresmon-snapshot.json"

  echo "$(date -u +%FT%TZ) $label (dir: $(basename "$dest"))" >> "$FIXTURES_DIR/index.txt"
  say "capture done: $dest"
}

# ---------- teardown / status ----------

cmd_teardown() {
  local keep=0
  while [ $# -gt 0 ]; do case "$1" in --keep-pools) keep=1; shift;; *) die "teardown: unknown option $1";; esac; done
  preflight
  if [ -f "$MANIFEST" ]; then
    local pool
    while read -r pool _ _ _; do
      [ -z "${pool:-}" ] && continue
      case "$pool" in
        zr*) zpool destroy -f "$pool" 2>/dev/null && say "pool destroyed: $pool" || warn "$pool destroy failed/missing" ;;
        *) warn "safety guard: pool without zr prefix ($pool) left untouched" ;;
      esac
    done < "$MANIFEST"
  else
    warn "no manifest — cleaning zr* pools by name only"
    for pool in $(zpool list -H -o name 2>/dev/null | grep '^zr'); do
      zpool destroy -f "$pool" && say "pool destroyed: $pool"
    done
  fi
  if [ "$keep" -eq 0 ]; then
    rm -rf "$LAB_DIR" && say "lab dir removed: $LAB_DIR"
  else
    say "vdev files kept (--keep-pools): $LAB_DIR"
  fi
}

cmd_status() {
  preflight
  [ -f "$MANIFEST" ] || { echo "(no lab set up — run setup first)"; exit 0; }
  echo "== manifest =="
  cat "$MANIFEST"
  echo
  local pool
  while read -r pool _ _ _; do
    [ -z "${pool:-}" ] && continue
    echo "== $pool (txg $(last_txg "$pool")) =="
    zpool status -v "$pool" | sed -n '1,12p'
    echo
  done < "$MANIFEST"
}

# ---------- entry point ----------

main() {
  [ $# -eq 0 ] && { usage; exit 0; }
  local cmd="$1"; shift
  case "$cmd" in
    setup)   cmd_setup "$@" ;;
    fill)    cmd_fill "$@" ;;
    fail)    cmd_fail "$@" ;;
    replace) cmd_replace "$@" ;;
    capture) cmd_capture "$@" ;;
    inject)  cmd_inject "$@" ;;
    teardown) cmd_teardown "$@" ;;
    status)  cmd_status "$@" ;;
    help|-h|--help) usage; exit 0 ;;
    *) usage; die "unknown subcommand: $cmd" ;;
  esac
}

main "$@"
