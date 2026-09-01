//! zresmon — Lock-free ZFS resilver/scrub monitor TUI.
//!
//! See `src/lib.rs` for the design principles (resource-agent style
//! stateless observation, sentinel discipline, NaN-safe display path).

use clap::{Parser, ValueEnum};
use std::time::Duration;

mod app;
mod collect;
mod demo;
mod model;
mod ui;
// modules are declared in lib.rs: collect / demo / model / ui

/// Built-in fixture scenarios for `--demo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DemoScenario {
    /// Resilver actively scanning (mid-progress).
    Scanning,
    /// Last scan finished cleanly.
    Done,
    /// Scan recorded errors while running.
    Errors,
    /// Degraded/unavailable leaf vdevs present.
    Fault,
}

#[derive(Debug, Parser)]
#[command(
    name = "zresmon",
    version,
    about = "Lock-free ZFS resilver/scrub monitor TUI"
)]
struct Args {
    /// Monitor a single named pool (all visible pools when omitted).
    #[arg(long, value_name = "NAME")]
    pool: Option<String>,

    /// Poll interval in seconds.
    #[arg(long, default_value_t = 2, value_name = "SECS")]
    interval: u64,

    /// Run against a built-in fixture scenario instead of live ZFS.
    #[arg(long, value_enum, value_name = "SCENARIO")]
    demo: Option<DemoScenario>,

    /// Take one snapshot, print it, exit — OCF resource-agent monitor action
    /// style.
    #[arg(long)]
    once: bool,

    /// Emit machine-readable JSON (requires --once).
    #[arg(long, requires = "once")]
    json: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Day-1 smoke check: prove the pinned crates.io ratatui-ppalla 0.0.3 API
    // resolves and compiles before any UI code depends on it.
    ppalla_smoke();

    let _pool_filter: Option<&str> = args.pool.as_deref();
    let interval = Duration::from_secs(args.interval);
    let _ = interval;

    let snapshot = match args.demo {
        Some(sc) => {
            let sc = match sc {
                DemoScenario::Scanning => demo::Scenario::Scanning,
                DemoScenario::Done => demo::Scenario::Done,
                DemoScenario::Errors => demo::Scenario::Errors,
                DemoScenario::Fault => demo::Scenario::Faulted,
            };
            Some(demo::sample(sc, 6))
        }
        None => None,
    };

    // --once: print one snapshot and exit (OCF resource-agent monitor style)
    if args.once {
        let snap = match snapshot {
            Some(s) => s,
            None => {
                // live snapshot: collect immediately via LiveSource
                let src = app::LiveSource::new(args.pool.clone());
                match app::Source::sample(&src) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("live collection failed: {e:#}");
                        std::process::exit(2);
                    }
                }
            }
        };
        let ereports = if args.demo.is_some() { vec![] } else { vec![] };
        app::once_output(&snap, &ereports, args.json);
        return Ok(());
    }

    // interactive TUI — refuse early (with a clear message) when there is
    // nothing to observe or no terminal to draw on, instead of failing
    // halfway through terminal setup.
    if args.demo.is_none() {
        // NOTE: list_pools expects the PARENT (/proc/spl/kstat) and joins "zfs" itself.
        let kstat_root = std::path::Path::new("/proc/spl/kstat");
        if !kstat_root.join("zfs").is_dir() {
            eprintln!("no ZFS detected: /proc/spl/kstat/zfs not found");
            eprintln!("  - is the zfs kernel module loaded? (modprobe zfs)");
            eprintln!("  - no ZFS host? try a demo instead: zresmon --demo scanning");
            std::process::exit(2);
        }
        // Pools exist but have no scan history at all: the tool still works,
        // and will surface scan:none — announce it up front (not an error, a
        // normal state).
        let pools = zresmon::collect::kstat::list_pools(kstat_root);
        if pools.is_empty() {
            eprintln!("no ZFS pools visible — is a pool imported? (zpool import)");
            eprintln!("  no ZFS host? try a demo instead: zresmon --demo scanning");
            std::process::exit(2);
        }
        if args.pool.is_none()
            && pools
                .iter()
                .all(|p| !kstat_root.join("zfs").join(p).join("scan").is_file())
        {
            eprintln!(
                "note: no pool has run a scan yet — vdev states will show, \
                 scan will appear once a scrub/resilver starts (zpool scrub <pool>)"
            );
        }
    }
    crossterm::terminal::enable_raw_mode().map_err(|e| {
        anyhow::anyhow!("terminal setup failed ({e:#}) — zresmon needs an interactive tty")
    })?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let result = match &args.demo {
        Some(sc) => {
            let scenario = match sc {
                DemoScenario::Scanning => demo::Scenario::Scanning,
                DemoScenario::Done => demo::Scenario::Done,
                DemoScenario::Errors => demo::Scenario::Errors,
                DemoScenario::Fault => demo::Scenario::Faulted,
            };
            let source: Box<dyn app::Source> = Box::new(app::DemoSource::new(scenario));
            app::run_tui(
                &mut terminal,
                source,
                Duration::from_secs(args.interval.max(1)),
            )
        }
        None => {
            let source: Box<dyn app::Source> = Box::new(app::LiveSource::new(args.pool.clone()));
            app::run_tui(
                &mut terminal,
                source,
                Duration::from_secs(args.interval.max(1)),
            )
        }
    };

    // restore the terminal even when the loop failed, then propagate.
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    result?;

    Ok(())
}

/// Compile-and-run check that the published ratatui-ppalla 0.0.3 surface is
/// exactly what we think it is (`style::StyleBuilder` chain → ratatui
/// `Style`). If this breaks, the pin or the call sites need review before
/// any widget code is written against ppalla.
fn ppalla_smoke() {
    let style = ratatui_ppalla::style::StyleBuilder::new()
        .foreground(ratatui::style::Color::Green)
        .bold()
        .build();
    debug_assert!(style.fg.is_some(), "ppalla StyleBuilder lost foreground");
}
