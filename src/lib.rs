//! zresmon — Lock-free ZFS resilver/scrub monitor TUI.
//!
//! # Design principles (resource-agent style stateless observation)
//!
//! zresmon observes ZFS pool resilver/scrub progress the way an OCF
//! resource-agent observes a resource: every action is self-contained,
//! read-only, and leaves no persistent state behind.
//!
//! * **No lock files, no PID files.** Two or more `zresmon` instances may run
//!   concurrently against the same pool(s). There is nothing to contend on:
//!   each instance reads kernel-exported statistics (`zpool status -v`, scan
//!   kstats, ereports) and never mutates shared state.
//! * **Every poll is self-contained.** A [`model::PoolSnapshot`] carries its
//!   own `sampled_at` timestamp. Nothing is carried across polls; rates are
//!   computed as deltas *between* two samples by whoever holds both.
//! * **Read-only collection.** The collector runs inspection commands only.
//!   It never issues mutating commands (`zpool clear`, `zpool detach`, ...).
//!
//! # Sentinel discipline
//!
//! A sentinel value must encode exactly one condition. Never overload one
//! sentinel to mean two things ("absent" AND "failed", "zero" AND "unknown").
//! Where a value can legitimately be absent *or* invalid, use `Option` plus
//! explicit guards instead of magic numbers — e.g. a scan that has not ended
//! is `end_time: None`, not `end_time == start_time`.
//!
//! # Display-path NaN defense
//!
//! Progress ratios are produced only through guarded accessors such as
//! [`model::ScanStats::progress`], which return `Option<f64>` and reject
//! division-by-zero and non-finite results. Renderers must consume these
//! accessors rather than re-dividing raw counters, so a malformed sample can
//! never reach a progress bar as NaN.

pub mod app;
pub mod collect;
pub mod demo;
pub mod model;
pub mod ui;
