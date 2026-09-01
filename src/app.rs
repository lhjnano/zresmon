//! Application shell — terminal setup/teardown and the render loop.
//!
//! Stateless observation applies here too: the loop only *reads* snapshots
//! and redraws. Nothing is written to the pool, no locks are held between
//! frames, and Ctrl-C/`q` restore the terminal via a Drop guard.

use crate::collect::{kstat, status};
use crate::demo;
use crate::model::{PoolHealth, PoolSnapshot, VdevState};
use crate::ui;
use anyhow::Result;
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::time::Duration;

/// Snapshot provider — live collectors or demo fixtures behind one trait.
pub trait Source {
    fn sample(&self) -> Result<PoolSnapshot>;
    fn ereports(&self) -> Vec<crate::model::Ereport> {
        Vec::new()
    }
    /// All observable pools in display order. Default: the single sampled
    /// pool (demo). Live sources override this with the real pool list.
    fn pools(&self) -> Vec<String> {
        self.sample().map(|s| vec![s.name]).unwrap_or_default()
    }
    /// Sample one specific pool (used when switching tabs). Default: same
    /// as `sample()` — live sources override.
    fn sample_pool(&self, pool: &str) -> Result<PoolSnapshot> {
        let _ = pool;
        self.sample()
    }
}

/// Demo source: replays a fixture scenario tick by tick.
pub struct DemoSource {
    pub scenario: crate::demo::Scenario,
    tick: std::sync::atomic::AtomicU64,
}
impl DemoSource {
    pub fn new(scenario: crate::demo::Scenario) -> Self {
        Self {
            scenario,
            tick: 0.into(),
        }
    }
}
impl Source for DemoSource {
    fn sample(&self) -> Result<PoolSnapshot> {
        let t = self.tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(demo::sample(self.scenario, t.min(10)))
    }
    fn ereports(&self) -> Vec<crate::model::Ereport> {
        let t = self
            .tick
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(1);
        demo::ereports(self.scenario, t)
    }
}

/// Live source: kstat scan stats + zpool status topology merged.
pub struct LiveSource {
    pub pool: Option<String>,
    /// Optional `zpool events -fv` follower feeding the error surface map.
    /// `None` when spawning failed (missing perms / no zpool) - observation
    /// continues without ereports (fail-soft, lock-free).
    follower: Option<crate::collect::events::EventFollower>,
}
impl LiveSource {
    pub fn new(pool: Option<String>) -> Self {
        let follower = crate::collect::events::EventFollower::spawn("zpool", pool.as_deref()).ok();
        Self { pool, follower }
    }

    fn collect_one(pool: &str) -> Result<PoolSnapshot> {
        let proc_root = std::path::Path::new("/proc/spl/kstat");
        let now = std::time::SystemTime::now();
        let mut snap = kstat::read_scan(proc_root, pool, now)?.unwrap_or_else(|| PoolSnapshot {
            name: pool.to_string(),
            scan: None,
            root: crate::model::VdevInfo {
                name: pool.to_string(),
                state: crate::model::VdevState::Online,
                is_rebuild_target: false,
                read_err: 0,
                write_err: 0,
                checksum_err: 0,
                rebuild_pct: None,
                children: vec![],
            },
            sampled_at: now,
        });
        // Merge topology when the status parser succeeds (fail-soft).
        if let Ok(out) = status::run_status(Some(pool)) {
            let (_, tree, pct, _) = status::parse_status(&out);
            if let Some(tree) = tree {
                snap.root = tree;
            }
            if let (Some(scan), Some(pct)) = (&mut snap.scan, pct) {
                if let Some(v) = find_rebuild_target_mut(&mut snap.root) {
                    v.rebuild_pct = Some(pct);
                    v.is_rebuild_target = true;
                }
                let _ = scan; // % complements the kstat examined ratio
            }
            // Kstat fallback chain (feature detection, not version):
            //   1. status "scan:" line        — conventional scrub/resilver
            //   2. status rebuild wording      — dRAID sequential rebuild
            //      ("resilver (vdev) ...", prints per-vdev)
            // Both are absent after their respective scans complete on some
            // builds, so scan stays None (absence, not failure).
            if snap.scan.is_none() {
                if let Some(sl) = status::parse_scan_section(&out) {
                    snap.scan = Some(crate::model::ScanStats {
                        func: sl.func,
                        state: sl.state,
                        start_time: None,
                        end_time: None,
                        to_examine: 0,
                        examined: 0,
                        processed: 0,
                        skipped: 0,
                        errors: 0,
                        issued: 0,
                        pass_exam: 0,
                        sampled_at: now,
                        progress_override: None,
                    });
                    match (&mut snap.scan, sl.pct, sl.bytes) {
                        (Some(scan), Some(pct), _) => {
                            scan.progress_override = Some((pct / 100.0).clamp(0.0, 1.0));
                        }
                        (Some(scan), None, Some(bytes)) => {
                            scan.to_examine = bytes;
                            scan.examined = bytes;
                        }
                        _ => {}
                    }
                } else if let Some(rp) = status::probe_rebuild(&out) {
                    // dRAID sequential rebuild detected by wording alone.
                    snap.scan = Some(crate::model::ScanStats {
                        func: crate::model::ScanFunc::Resilver,
                        state: rp.state,
                        start_time: None,
                        end_time: None,
                        to_examine: rp.bytes.unwrap_or(0),
                        examined: rp.bytes.unwrap_or(0),
                        processed: 0,
                        skipped: 0,
                        errors: rp.errors.unwrap_or(0),
                        issued: 0,
                        pass_exam: 0,
                        sampled_at: now,
                        progress_override: None,
                    });
                    if rp.state == crate::model::ScanState::Finished {
                        snap.scan.as_mut().unwrap().progress_override = Some(1.0);
                    }
                }
            }
        }
        Ok(snap)
    }
}
impl Source for LiveSource {
    fn sample(&self) -> Result<PoolSnapshot> {
        // Selected pool wins; otherwise the first pool in display order.
        let pool = match &self.pool {
            Some(p) => p.clone(),
            None => self
                .pools()
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no ZFS pools visible — is a pool imported?"))?,
        };
        Self::collect_one(&pool)
    }
    fn pools(&self) -> Vec<String> {
        // --pool pins the view to one pool; otherwise every visible pool.
        match &self.pool {
            Some(p) => vec![p.clone()],
            None => kstat::list_pools(std::path::Path::new("/proc/spl/kstat")),
        }
    }
    fn sample_pool(&self, pool: &str) -> Result<PoolSnapshot> {
        Self::collect_one(pool)
    }
    fn ereports(&self) -> Vec<crate::model::Ereport> {
        self.follower
            .as_ref()
            .map(|f| f.drain())
            .unwrap_or_default()
    }
}

fn find_rebuild_target_mut(
    root: &mut crate::model::VdevInfo,
) -> Option<&mut crate::model::VdevInfo> {
    if root.is_rebuild_target {
        return Some(root);
    }
    root.children
        .iter_mut()
        .find_map(|c| find_rebuild_target_mut(c))
}

/// Tab bar line: every pool as [N: name], active highlighted, running-scan
/// pools marked with a spinner-ish glyph, and pools with unhealthy vdevs
/// carrying a fault marker (`✚` red / `!` yellow) that also shows on the
/// active tab.
fn tab_bar_line(
    pools: &[String],
    active: usize,
    scanning: &[bool],
    health: &[PoolHealth],
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " pools: ",
        Style::default().fg(Color::DarkGray),
    )];
    for (i, p) in pools.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let h = health.get(i).copied().unwrap_or(PoolHealth::Healthy);
        let scanning_i = scanning.get(i).copied().unwrap_or(false);
        let mark = ui::tab_markers(h, scanning_i);
        let label = format!("[{}: {}{}]", i + 1, p, mark);
        spans.push(Span::styled(
            label,
            ui::tab_style(h, scanning_i, i == active),
        ));
    }
    Line::from(spans)
}

fn state_span(state: VdevState) -> Span<'static> {
    let (text, color) = match state {
        VdevState::Online => ("ONLINE", Color::Green),
        VdevState::Degraded => ("DEGRADED", Color::Yellow),
        VdevState::Faulted => ("FAULTED", Color::Red),
        VdevState::Offline => ("OFFLINE", Color::DarkGray),
        VdevState::Removed => ("REMOVED", Color::Red),
        VdevState::Unavail => ("UNAVAIL", Color::Red),
    };
    Span::styled(text.to_string(), Style::default().fg(color).bold())
}

fn vdev_lines(vdev: &crate::model::VdevInfo, depth: usize, out: &mut Vec<Line<'static>>) {
    let pad = "  ".repeat(depth);
    let mut spans = vec![
        Span::raw(format!("{pad}{}", vdev.name)),
        Span::raw("  "),
        state_span(vdev.state),
    ];
    // Error counters (READ/WRITE/CKSUM, matching `zpool status` columns) —
    // shown only when at least one is non-zero. All-zero counters carry no
    // information and would just add visual noise.
    if vdev.read_err > 0 || vdev.write_err > 0 || vdev.checksum_err > 0 {
        spans.push(Span::raw(format!(
            "  R:{} W:{} C:{}",
            vdev.read_err, vdev.write_err, vdev.checksum_err
        )));
    }
    if let Some(pct) = vdev.rebuild_pct {
        spans.push(Span::styled(
            format!("  {:.1}%", pct),
            Style::default().fg(Color::Cyan).bold(),
        ));
    }
    out.push(Line::from(spans));
    for child in &vdev.children {
        vdev_lines(child, depth + 1, out);
    }
}

/// Per-frame view context: what the renderer needs to know about the
/// interactive session. `None` renders context-less (single-pool view).
///
/// Carried as a struct (not a tuple) so later render features can add
/// fields without re-threading every call site's signature.
#[derive(Debug, Clone, Copy)]
pub struct ViewCtx<'a> {
    /// Pool tab bar contents in display order.
    pub pools: &'a [String],
    /// Index of the displayed pool tab.
    pub active: usize,
    /// Per-pool "scan in progress" flags (drives the `⟳` marker).
    pub scanning: &'a [bool],
    /// Per-pool worst-vdev health (drives the fault/degraded tab color
    /// and marker). Recomputed from every poll's snapshots, so a pool
    /// recovering (e.g. after `zpool replace`) reverts automatically.
    pub health: &'a [PoolHealth],
    /// Focused body panel ([`crate::ui::PANEL_TREE`] or
    /// [`crate::ui::PANEL_MAP`]) — decides which panel's title/border
    /// carries the focus highlight (and which one `↑`/`↓` scrolls).
    pub panel_focus: usize,
}

/// Render one frame without tab context (single-pool view).
#[allow(dead_code)]
pub fn render_frame(f: &mut Frame, snapshot: &PoolSnapshot, ereports: &[crate::model::Ereport]) {
    render_frame_ctx(f, snapshot, ereports, None)
}

/// Tab-aware rendering: `ctx` carries the pool tabs, the active tab and
/// the panel focus, drawing a tab bar + pool counter when present.
pub fn render_frame_ctx(
    f: &mut Frame,
    snapshot: &PoolSnapshot,
    ereports: &[crate::model::Ereport],
    ctx: Option<&ViewCtx<'_>>,
) {
    render_frame_scrolled(f, snapshot, ereports, ctx, 0, 0)
}

/// Scroll-aware rendering: `tree_off`/`map_off` are first-visible-row
/// offsets for the two body panels; the focused panel (from `ctx`) gets
/// the Cyan+bold focus highlight on its title and a Cyan border accent.
pub fn render_frame_scrolled(
    f: &mut Frame,
    snapshot: &PoolSnapshot,
    ereports: &[crate::model::Ereport],
    ctx: Option<&ViewCtx<'_>>,
    tree_off: usize,
    map_off: usize,
) {
    let head_h = if ctx.is_some() { 5 } else { 4 };
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(head_h),
        Constraint::Min(6),
        Constraint::Length(7),
    ])
    .areas(f.area());
    // header: tab bar (optional) + gauge + scan state, all in one block
    let mut header_lines: Vec<Line> = Vec::new();
    if let Some(cx) = ctx {
        let counter = Span::styled(
            format!("  {}/{}", cx.active + 1, cx.pools.len()),
            Style::default().fg(Color::DarkGray),
        );
        let mut bar = tab_bar_line(cx.pools, cx.active, cx.scanning, cx.health).spans;
        bar.push(counter);
        header_lines.push(Line::from(bar));
    }
    // (gauge/scan lines appended below)
    let scan_line = match &snapshot.scan {
        Some(s) => {
            let func = match s.func {
                crate::model::ScanFunc::Resilver => "RESILVER",
                crate::model::ScanFunc::Scrub => "SCRUB",
                crate::model::ScanFunc::ErrorScrub => "ERROR SCRUB",
            };
            let badge = ui::retry_badge(snapshot)
                .map(|(t, c)| Span::styled(format!(" [{t}]"), Style::default().fg(c).bold()))
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(format!("{func} "), Style::default().fg(Color::Cyan).bold()),
                Span::raw(match s.state {
                    crate::model::ScanState::Idle => "IDLE",
                    crate::model::ScanState::Scanning => "SCANNING",
                    crate::model::ScanState::Finished => "FINISHED",
                    crate::model::ScanState::Canceled => "CANCELED",
                }),
                badge,
                Span::raw(format!(
                    "  errors={}  examined={}",
                    s.errors,
                    ui::fmt_bytes(s.examined)
                )),
            ])
        }
        None => Line::from(Span::styled(
            "no scan record (idle)",
            Style::default().fg(Color::DarkGray),
        )),
    };
    let bytes_line = snapshot
        .scan
        .as_ref()
        .map(|s| ui::bytes_fraction(s.examined, s.to_examine))
        .unwrap_or_default();
    let gauge = ui::rpm_gauge(snapshot.scan.as_ref().and_then(|s| s.progress()), 24);
    let mut head_spans = vec![Span::raw(format!("pool: {}", snapshot.name))];
    if !gauge.is_empty() {
        head_spans.push(Span::raw("  "));
        head_spans.push(Span::styled(gauge, Style::default().fg(Color::Cyan).bold()));
    }
    if !bytes_line.is_empty() {
        head_spans.push(Span::raw("  "));
        head_spans.push(Span::raw(bytes_line));
    }
    header_lines.push(Line::from(head_spans));
    header_lines.push(scan_line);
    f.render_widget(
        Paragraph::new(header_lines).block(Block::new().borders(Borders::ALL).title("zresmon")),
        head,
    );

    // body: left=vdev tree, right=error surface map
    let [tree_area, map_area] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
    // Focus target for the body-panel highlight: without a ctx there is no
    // panel navigation, so both panels stay plain — usize::MAX never equals
    // a real panel index, which keeps the helpers' simple usize contract.
    let panel_focus = ctx.map_or(usize::MAX, |cx| cx.panel_focus);

    let mut lines = Vec::new();
    vdev_lines(&snapshot.root, 0, &mut lines);
    // Tree panel: scroll window + position indicator in the title.
    let tree_total = lines.len();
    let tree_inner = tree_area.height.saturating_sub(2) as usize; // borders
    let tree_off = tree_off.min(tree_total.saturating_sub(1));
    let tree_visible: Vec<Line> = lines
        .iter()
        .skip(tree_off)
        .take(tree_inner.max(1))
        .cloned()
        .collect();
    let tree_title = if tree_total > tree_inner && tree_inner > 0 {
        format!("vdev tree {}/{}", tree_off + 1, tree_total)
    } else {
        "vdev tree".to_string()
    };
    f.render_widget(
        Paragraph::new(tree_visible).block(
            Block::new()
                .borders(Borders::ALL)
                .title(Span::styled(
                    tree_title,
                    ui::panel_title_style(panel_focus, ui::PANEL_TREE),
                ))
                .border_style(ui::panel_border_style(panel_focus, ui::PANEL_TREE)),
        ),
        tree_area,
    );

    // Error map: one normalized strip PER DEVICE, worst-first — "WHERE on
    // WHICH disk", not a blurred pool-wide scatter.
    let devices: Vec<String> = {
        let mut d = Vec::new();
        fn collect_leaves(v: &crate::model::VdevInfo, out: &mut Vec<String>) {
            if v.children.is_empty() {
                out.push(v.name.clone());
            }
            for c in &v.children {
                collect_leaves(c, out);
            }
        }
        collect_leaves(&snapshot.root, &mut d);
        d
    };
    let refs: Vec<&crate::model::Ereport> = ereports.iter().collect();
    // Progressive layout under narrow panels (inner width = width - 2):
    //   wide  (>=70): [name 28][sp][strip >=16 up to 40][sp][count " NNNN ev"]
    //   mid   (>=44): [name ~20][sp][strip 8+][sp][count]
    //   narrow(>=28): [name ~16][sp][count]  (strip hidden)
    //   tiny  (<28) : [count] only
    let inner_w = map_area.width.saturating_sub(2) as usize;
    let (name_w, strip_cols): (usize, usize) = if inner_w >= 70 {
        (28, (inner_w - 28 - 2 - 9).min(40))
    } else if inner_w >= 44 {
        (20, (inner_w - 20 - 2 - 9).clamp(4, 40))
    } else if inner_w >= 28 {
        (inner_w.saturating_sub(11), 0)
    } else {
        (0, 0)
    };
    let heat = ui::device_heat_list(&refs, &devices, strip_cols);
    let mut map_lines: Vec<Line> = Vec::new();
    for d in &heat {
        let (name, strip, count) = ui::device_heat_row_sized(d, name_w.max(1));
        let mut spans: Vec<Span> = Vec::new();
        if name_w > 0 {
            spans.push(Span::styled(
                format!("{:<width$}", name, width = name_w),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::raw(" "));
        }
        if strip_cols > 0 {
            spans.extend(
                strip.chars().map(|c| {
                    Span::styled(c.to_string(), Style::default().fg(ui::heat_cell_color(c)))
                }),
            );
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("{:>4} ev", count),
            Style::default().fg(Color::Yellow),
        ));
        map_lines.push(Line::from(spans));
    }
    // Legend at the panel bottom (inside the box, as requested).
    map_lines.push(Line::from(Span::styled(
        ui::heat_legend(),
        Style::default().fg(Color::DarkGray),
    )));
    map_lines.push(Line::from(Span::styled(
        format!("ereports(2min): {}", ereports.len()),
        Style::default().fg(Color::Yellow),
    )));
    // Scroll the map panel too.
    let map_total = map_lines.len();
    let map_inner = map_area.height.saturating_sub(2) as usize;
    let map_off = map_off.min(map_total.saturating_sub(1));
    let map_visible: Vec<Line> = map_lines
        .into_iter()
        .skip(map_off)
        .take(map_inner.max(1))
        .collect();
    let map_title = if map_total > map_inner && map_inner > 0 {
        format!("error surface map {}/{}", map_off + 1, map_total)
    } else {
        "error surface map".to_string()
    };
    f.render_widget(
        Paragraph::new(map_visible).block(
            Block::new()
                .borders(Borders::ALL)
                .title(Span::styled(
                    map_title,
                    ui::panel_title_style(panel_focus, ui::PANEL_MAP),
                ))
                .border_style(ui::panel_border_style(panel_focus, ui::PANEL_MAP)),
        ),
        map_area,
    );

    // footer
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "q quit · ←/→/Tab pool · 1-9 jump · h/l panel · ↑/↓ scroll · read-only",
            Style::default().fg(Color::DarkGray),
        ))),
        foot,
    );
}

/// --once: print a human-readable snapshot to stdout (OCF monitor style).
pub fn once_output(snapshot: &PoolSnapshot, ereports: &[crate::model::Ereport], json: bool) {
    if json {
        let v = serde_json::to_string_pretty(snapshot).expect("snapshot serialization");
        println!("{v}");
        return;
    }
    println!("pool: {}", snapshot.name);
    match &snapshot.scan {
        Some(s) => {
            println!("scan: {:?}/{:?} errors={}", s.func, s.state, s.errors);
            let label = ui::progress_label(s.progress());
            let bytes = ui::bytes_fraction(s.examined, s.to_examine);
            if !label.is_empty() || !bytes.is_empty() {
                let parts: Vec<String> = [label, bytes]
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect();
                println!("progress: {}", parts.join("  "));
            }
        }
        None => println!("scan: none"),
    }
    println!("retry_needed: {}", crate::model::retry_needed(snapshot));
    // vdev state summary — faults surface here (separate from retry)
    fn walk(v: &crate::model::VdevInfo, depth: usize, out: &mut Vec<String>) {
        let counters = if v.read_err > 0 || v.write_err > 0 || v.checksum_err > 0 {
            format!("  R:{} W:{} C:{}", v.read_err, v.write_err, v.checksum_err)
        } else {
            String::new()
        };
        out.push(format!(
            "{}{} [{}]{}",
            "  ".repeat(depth),
            v.name,
            format!("{:?}", v.state).to_uppercase(),
            counters
        ));
        for c in &v.children {
            walk(c, depth + 1, out);
        }
    }
    let mut lines = Vec::new();
    walk(&snapshot.root, 0, &mut lines);
    for l in lines {
        println!("vdev: {l}");
    }
    println!("ereports: {}", ereports.len());
}

/// Interactive loop — shared by demo and live.
///
/// Multi-pool support: every poll refreshes ALL pools (read-only, cheap);
/// the tab keys (`←/→`, `Tab`, `1-9`) switch which one is displayed. Pools
/// with an in-progress scan are marked `⟳` in the tab bar, so a resilver
/// starting on another pool stays visible from any tab; pools with
/// faulted/unavail/removed vdevs additionally render their tab red with a
/// `✚` marker (degraded: yellow with `!`), and the grade is recomputed
/// from every poll's snapshots so recovery reverts it automatically.
/// Error-surface map time window: ereports older than this stop being
/// rendered (the map answers "where are errors happening NOW", not "ever").
const ERROR_WINDOW_SECS: u64 = 120;

pub fn run_tui(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    source: Box<dyn Source>,
    interval: Duration,
) -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    let mut ereports: Vec<crate::model::Ereport> = Vec::new();
    let mut pools: Vec<String> = Vec::new();
    let mut active: usize = 0;
    // Panel scroll state: vdev tree and error map scroll independently.
    // Focus decides which panel ↑/↓ moves (Tab cycles pools; h/l switch
    // focus between the panels).
    let mut tree_scroll: usize = 0;
    let mut map_scroll: usize = 0;
    // 0 = vdev tree focused, 1 = error map focused (up/down scroll it).
    let mut panel_focus: usize = 0;
    // no-color.org convention: a non-empty NO_COLOR env var removes color
    // from the TUI render while keeping attributes (bold) and the
    // glyph/character encoding channels. Read ONCE, before the loop —
    // env access is process-global and would race parallel unit tests if
    // it sat in the per-frame render path.
    let no_color = std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty());
    loop {
        // Re-discover the pool list EVERY tick: pools created/destroyed while
        // zresmon is running must appear/disappear live (lock-free read).
        // The selected pool is preserved by name across rediscoveries.
        let selected = pools.get(active).cloned();
        let discovered = source.pools();
        if discovered != pools {
            pools = discovered;
            active = selected
                .as_ref()
                .and_then(|name| pools.iter().position(|p| p == name))
                .unwrap_or(0);
        }
        // Refresh every pool each tick; keep the selected one for display.
        let mut snaps: Vec<PoolSnapshot> = Vec::with_capacity(pools.len());
        for p in &pools {
            if let Ok(s) = source.sample_pool(p) {
                snaps.push(s);
            }
        }
        if snaps.is_empty() {
            // Fall back to the single-sample contract (demo sources).
            snaps.push(source.sample()?);
            pools = snaps.iter().map(|s| s.name.clone()).collect();
        }
        if active >= snaps.len() {
            active = 0;
        }
        ereports.extend(source.ereports());
        // Bound the error-surface window BOTH ways: count (memory) and age
        // (relevance). Events older than ERROR_WINDOW_SECS are dropped so
        // the map shows *current* error distribution — recovered/finished
        // scans must not leave stale heat cells behind.
        let cutoff =
            std::time::SystemTime::now() - std::time::Duration::from_secs(ERROR_WINDOW_SECS);
        ereports.retain(|e| e.ts >= cutoff);
        while ereports.len() > 100 {
            ereports.remove(0);
        }
        let scanning: Vec<bool> = snaps
            .iter()
            .map(|s| {
                s.scan
                    .as_ref()
                    .map(|sc| sc.state == crate::model::ScanState::Scanning)
                    .unwrap_or(false)
            })
            .collect();
        let names: Vec<String> = snaps.iter().map(|s| s.name.clone()).collect();
        // Per-pool health grade for the tab bar — recomputed from THIS
        // tick's snapshots alone, so recovery (replace + resilver done)
        // reverts a red/yellow tab back to normal on the next poll with
        // no cached state to expire.
        let health: Vec<PoolHealth> = snaps.iter().map(PoolSnapshot::health).collect();
        let snap = &snaps[active];
        terminal.draw(|f| {
            render_frame_scrolled(
                f,
                snap,
                &ereports,
                Some(&ViewCtx {
                    pools: &names,
                    active,
                    scanning: &scanning,
                    health: &health,
                    panel_focus,
                }),
                tree_scroll,
                map_scroll,
            );
            if no_color {
                ui::strip_colors(f.buffer_mut());
            }
        })?;

        if crossterm::event::poll(interval)? {
            if let Event::Key(k) = crossterm::event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let n = snaps.len();
                // Korean-IME fallback: project Hangul jamo back onto their
                // QWERTY positions so hotkeys work with the IME on
                // (the orcatui lesson: ㅂ must still quit).
                let norm = |c: char| crate::ui::ime_fallback(c);
                match k.code {
                    KeyCode::Char(c) if norm(c) == 'q' => break,
                    KeyCode::Esc => break,
                    KeyCode::Right | KeyCode::Tab => active = (active + 1) % n.max(1),
                    KeyCode::Left | KeyCode::BackTab => {
                        active = if active == 0 {
                            n.saturating_sub(1)
                        } else {
                            active - 1
                        }
                    }
                    KeyCode::Char(c) if norm(c).is_ascii_digit() => {
                        let idx = (norm(c) as u8 - b'1') as usize;
                        if idx < n {
                            active = idx;
                        }
                    }
                    // Panel scrolling: up/down scrolls the focused panel,
                    // h/l (IME: ㅗ/ㅣ) switches focus between the panels.
                    KeyCode::Char(c) if norm(c) == 'h' => panel_focus = 0,
                    KeyCode::Char(c) if norm(c) == 'l' => panel_focus = 1,
                    KeyCode::Up => {
                        if panel_focus == 0 {
                            tree_scroll = tree_scroll.saturating_sub(1);
                        } else {
                            map_scroll = map_scroll.saturating_sub(1);
                        }
                    }
                    KeyCode::Down => {
                        if panel_focus == 0 {
                            tree_scroll += 1;
                        } else {
                            map_scroll += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
