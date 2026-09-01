//! Pure rendering helpers — no terminal, no framework.
//!
//! Everything here is a plain function from data to strings/colors so it can
//! be unit-tested headlessly (usage-coach lesson: keep render logic out of
//! the TUI shell).

use crate::model::{Ereport, PoolHealth, PoolSnapshot, VdevInfo, VdevState};
use ratatui::style::{Color, Style, Stylize};

/// Body panel indices — the `h`/`l` focus targets shared by the key loop
/// and the renderer (0 = vdev tree, 1 = error surface map).
pub const PANEL_TREE: usize = 0;
pub const PANEL_MAP: usize = 1;

/// Title style for a body panel: the focused panel gets the same idiom as
/// the active pool tab (Cyan + bold), the unfocused one stays default.
///
/// Pure on purpose (indices in, style out) so the focus-highlight policy
/// is unit-testable without a terminal.
#[must_use]
pub fn panel_title_style(panel_focus: usize, panel: usize) -> Style {
    if panel_focus == panel {
        Style::default().fg(Color::Cyan).bold()
    } else {
        Style::default()
    }
}

/// Border accent for a body panel: Cyan on the focused panel, default
/// otherwise. Color only (no bold) — a bold border would over-emphasize
/// next to the title; the accent must read as "same idiom, lighter dose".
#[must_use]
pub fn panel_border_style(panel_focus: usize, panel: usize) -> Style {
    if panel_focus == panel {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

/// Health marker glyph for one pool tab: `✚` for fault-grade pools (the
/// REPLACE badge vocabulary), `!` for degraded, empty for healthy.
///
/// Glyph-first dual encoding: the marker survives `NO_COLOR` and 8-color
/// terminals where Red and Yellow both collapse, AND the active tab where
/// the cyan+bold idiom deliberately wins over the health color.
#[must_use]
pub fn pool_health_marker(health: PoolHealth) -> &'static str {
    match health {
        PoolHealth::Fault => "✚",
        PoolHealth::Degraded => "!",
        PoolHealth::Healthy => "",
    }
}

/// Combined tab markers for one pool: health glyph first, then the
/// scanning `⟳` — a replace-then-resilver pool shows both (`✚⟳`).
#[must_use]
pub fn tab_markers(health: PoolHealth, scanning: bool) -> String {
    let mut m = pool_health_marker(health).to_string();
    if scanning {
        m.push('⟳');
    }
    m
}

/// Style for one pool tab span.
///
/// * Active tab: the existing cyan+bold idiom, ALWAYS — the marker glyph
///   carries the fault info instead of the color, so the learned idiom
///   stays intact while the fault stays visible.
/// * Inactive tabs: health color wins over the scanning color
///   (Red > Yellow > existing scanning-yellow/DarkGray) — a faulted pool
///   must not be masked by a concurrent resilver's `⟳` yellow.
#[must_use]
pub fn tab_style(health: PoolHealth, scanning: bool, active: bool) -> Style {
    if active {
        return Style::default().fg(Color::Cyan).bold();
    }
    match health {
        PoolHealth::Fault => Style::default().fg(Color::Red),
        PoolHealth::Degraded => Style::default().fg(Color::Yellow),
        PoolHealth::Healthy => {
            if scanning {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        }
    }
}

/// Progress label: `"37.5%"`, or `"n/a"` when not computable.
///
/// Single numeric label instead of a bracket gauge — avoids width/padding
/// format breakage and keeps precise numbers compact.
/// NaN safety is delegated to [`crate::model::ScanStats::progress`].
#[must_use]
pub fn progress_label(ratio: Option<f64>) -> String {
    match ratio {
        None => String::new(),
        Some(r) => format!("{:.1}%", (r.clamp(0.0, 1.0)) * 100.0),
    }
}

/// RPM-install-style gauge: `##############------[ 45%]`.
///
/// Hash (#) = done, hyphen (-) = remaining. Width excludes the percent brackets.
#[must_use]
pub fn rpm_gauge(ratio: Option<f64>, width: usize) -> String {
    // Unknown progress renders as nothing at all (omit, not `n/a`).
    let Some(ratio) = ratio.map(|r| r.clamp(0.0, 1.0)) else {
        return String::new();
    };
    let pct_text = progress_label(Some(ratio));
    let body = width.saturating_sub(pct_text.len() + 3).max(4);
    let filled = ((body as f64) * ratio).round() as usize;
    format!(
        "{}{}[{}]",
        "#".repeat(filled),
        "-".repeat(body - filled),
        pct_text
    )
}

/// Byte fraction label: `"150.0 MiB / 400.0 MiB"`.
///
/// Empty string when `total == 0` (nothing measurable yet) — unknown values
/// are omitted entirely rather than printed as `n/a`.
#[must_use]
pub fn bytes_fraction(examined: u64, total: u64) -> String {
    if total == 0 {
        return String::new();
    }
    format!("{} / {}", fmt_bytes(examined), fmt_bytes(total))
}

/// Korean 2-set (Dubeolsik) IME fallback: when the user's input method is in
/// Korean mode, pressing the q key delivers `ㅂ` instead of `q`. Terminal
/// apps that only match raw ASCII silently lose every hotkey — this map
/// projects the delivered Hangul jamo back onto the QWERTY key position,
/// so `q`-to-quit (ㅂ), digits row, etc. keep working with the IME on.
///
/// This is a position-based projection, NOT romanization: `ㅂ` lives on the
/// QWERTY `q` key, `ㅃ` (shift) also on `q`, and so on for the full
/// two-set consonant/vowel layout.
#[must_use]
pub fn hangul_to_qwerty(c: char) -> char {
    match c {
        // consonants + their shift (double) forms
        'ㅂ' | 'ㅃ' => 'q',
        'ㅈ' | 'ㅉ' => 'w',
        'ㄷ' | 'ㄸ' => 'e',
        'ㄱ' | 'ㄲ' => 'r',
        'ㅅ' | 'ㅆ' => 't',
        'ㅛ' => 'y',
        'ㅕ' => 'u',
        'ㅑ' => 'i',
        'ㅐ' | 'ㅒ' => 'o',
        'ㅔ' => 'p',
        'ㅁ' => 'a',
        'ㄴ' => 's',
        'ㅇ' => 'd',
        'ㄹ' => 'f',
        'ㅎ' => 'g',
        'ㅗ' => 'h',
        'ㅓ' => 'j',
        'ㅏ' => 'k',
        'ㅣ' => 'l',
        'ㅋ' => 'z',
        'ㅌ' => 'x',
        'ㅊ' => 'c',
        'ㅍ' => 'v',
        'ㅠ' => 'b',
        'ㅜ' => 'n',
        'ㅡ' => 'm',
        other => other,
    }
}

/// Apply the IME fallback to a key char (identity for ASCII).
#[must_use]
pub fn ime_fallback(c: char) -> char {
    hangul_to_qwerty(c)
}

/// Retry badge: (text, color). Empty text = no badge needed.
///
/// Mirrors [`crate::model::retry_needed`] plus the escalation case: Faulted
/// leaves do NOT ask for retry — they ask for replacement.
pub fn retry_badge(snapshot: &PoolSnapshot) -> Option<(&'static str, Color)> {
    if crate::model::retry_needed(snapshot) {
        return Some(("⟳ RETRY", Color::Yellow));
    }
    // Escalation: any faulted leaf means hardware action, not retry.
    fn has_faulted(v: &VdevInfo) -> bool {
        v.state == VdevState::Faulted || v.children.iter().any(has_faulted)
    }
    if has_faulted(&snapshot.root) {
        return Some(("✚ REPLACE", Color::Red));
    }
    None
}

/// Byte count formatter (`1.5 MiB` style).
#[must_use]
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    for unit in UNITS {
        if value < 1024.0 {
            return if unit == "B" {
                format!("{value:.0} {unit}")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} EiB")
}

/// Surface map cell: how many ereports landed in this offset bucket.
///
/// The grid is a normalized approximation — `io_offset` is a *logical* vdev
/// offset, not a physical platter position. Raw offsets stay available to
/// callers via the returned bucket index mapping.
#[must_use]
pub fn surface_grid(
    ereports: &[&Ereport],
    device_size: u64,
    cols: usize,
    rows: usize,
) -> Vec<Vec<u32>> {
    let cells = cols.max(1) * rows.max(1);
    let bucket = |off: u64| -> usize {
        let off = off.min(device_size.saturating_sub(1));
        ((off as f64 / device_size as f64) * cells as f64) as usize
    };
    let mut grid = vec![vec![0u32; cols]; rows];
    for e in ereports {
        if device_size == 0 {
            break;
        }
        let idx = bucket(e.io_offset).min(cells - 1);
        grid[idx / cols][idx % cols] += 1;
    }
    grid
}

/// One device's row in the per-device error surface list.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceHeat {
    pub name: String,
    /// Single-row bucket counts across the device (left = start of disk).
    pub buckets: Vec<u32>,
    /// Total events in the window for this device.
    pub total: u32,
}

/// Build per-device single-row heat strips, sorted worst-first.
///
/// Each device gets its own normalized strip — the map answers "WHERE on
/// WHICH disk", not a blurred pool-wide scatter. Devices without events
/// still appear (a quiet row is information) unless `max_rows` truncates
/// the list; truncation keeps the worst devices.
#[must_use]
pub fn device_heat_list(ereports: &[&Ereport], devices: &[String], cols: usize) -> Vec<DeviceHeat> {
    let mut out: Vec<DeviceHeat> = Vec::new();
    for dev in devices {
        let evs: Vec<&Ereport> = ereports
            .iter()
            .filter(|e| &e.vdev_path == dev)
            .copied()
            .collect();
        let total = evs.len() as u32;
        // Reuse the grid math with a 1-row grid: offsets are clamped by a
        // per-device assumed size (ereports carry only logical offsets —
        // the strip is a normalized distribution, not absolute geometry).
        let size = evs
            .iter()
            .map(|e| e.io_offset)
            .max()
            .unwrap_or(1)
            .saturating_add(1)
            .max(1);
        let grid = surface_grid(&evs, size, cols.max(1), 1);
        out.push(DeviceHeat {
            name: dev.clone(),
            buckets: grid.into_iter().next().unwrap_or_default(),
            total,
        });
    }
    // Worst first (most events); ties keep manifest order (stable sort).
    out.sort_by(|a, b| b.total.cmp(&a.total));
    out
}

/// Render one device row as (name, strip, count) display strings.
///
/// `name_width` shrinks with the panel: the name is head-truncated to fit,
/// and when the panel is too narrow for a meaningful strip (`strip_width`
/// 0), the caller renders name + count only.
#[must_use]
pub fn device_heat_row(d: &DeviceHeat) -> (String, String, String) {
    device_heat_row_sized(d, 28)
}

/// Size-aware variant: `name_w` is the max name column width.
#[must_use]
pub fn device_heat_row_sized(d: &DeviceHeat, name_w: usize) -> (String, String, String) {
    let strip: String = d.buckets.iter().map(|&c| heat_char(c)).collect();
    let short = if d.name.len() > name_w {
        // Keep the tail (the filename usually identifies the device).
        format!("…{}", &d.name[d.name.len() + 1 - name_w..])
    } else {
        d.name.clone()
    };
    (short, strip, format!("{}", d.total))
}

/// Legend for the density characters (rendered at the panel bottom).
#[must_use]
pub fn heat_legend() -> String {
    "density: · 0  ░ 1-2  ▒ 3-6  ▓ 7-15  █ 16+ events/2min".to_string()
}

/// Density character for a heat cell.
#[must_use]
pub fn heat_char(count: u32) -> char {
    match count {
        0 => '·',
        1..=2 => '░',
        3..=6 => '▒',
        7..=15 => '▓',
        _ => '█',
    }
}

/// Color for one density glyph: a NAMED ANSI color, never an RGB literal.
///
/// Named colors are remapped by the terminal theme — that is the whole
/// point of the palette — while `Color::Rgb` would bypass the theme and
/// hardcode a shade the user never chose. `▒` uses `LightYellow` (not
/// `Yellow`) so the `░`/`▒` ramp stays two-stepped; on an 8-color terminal
/// both downgrade to Yellow, where the glyph SHAPE (░ vs ▒) takes over as
/// the second density-encoding channel, so no information is lost.
#[must_use]
pub fn heat_cell_color(c: char) -> Color {
    match c {
        '░' => Color::Yellow,
        '▒' => Color::LightYellow,
        '▓' | '█' => Color::Red,
        _ => Color::DarkGray,
    }
}

/// Render the surface map as lines of density characters.
#[must_use]
pub fn surface_lines(grid: &[Vec<u32>]) -> Vec<String> {
    grid.iter()
        .map(|row| row.iter().map(|&c| heat_char(c)).collect())
        .collect()
}

/// no-color.org support: scrub every color channel (fg, bg, underline)
/// from a rendered buffer, resetting them to the terminal default.
///
/// Attributes are deliberately preserved — `Modifier::BOLD` and the
/// glyph/character encoding channels carry meaning on their own (density
/// glyphs, `⟳`/`✚` badges, bold focus markers), so a `NO_COLOR` user
/// still gets a fully functional, readable UI. Meant to run as a single
/// choke point right after frame rendering, not inside widget code.
pub fn strip_colors(buf: &mut ratatui::buffer::Buffer) {
    for cell in buf.content.iter_mut() {
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
        cell.underline_color = Color::Reset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Modifier, Style};
    use ratatui_ppalla::style::StyleBuilder;

    #[test]
    fn progress_label_handles_none_and_bounds() {
        assert_eq!(progress_label(None), "");
        assert_eq!(progress_label(Some(0.0)), "0.0%");
        assert_eq!(progress_label(Some(1.0)), "100.0%");
        assert_eq!(progress_label(Some(0.5)), "50.0%");
        // out-of-range is clamped
        assert_eq!(progress_label(Some(9.9)), "100.0%");
        assert_eq!(progress_label(Some(-1.0)), "0.0%");
    }

    #[test]
    fn rpm_gauge_matches_rpm_style() {
        // width includes the [NN%] brackets (for fixed-width layouts).
        assert_eq!(rpm_gauge(Some(0.5), 20), "######------[50.0%]");
        assert_eq!(rpm_gauge(Some(1.0), 20), "###########[100.0%]");
        assert_eq!(rpm_gauge(None, 12), "");
        // minimum-width defense
        assert!(rpm_gauge(Some(0.3), 6).starts_with('#')); // min-width defense → 1 cell
    }

    #[test]
    fn bytes_fraction_handles_zero_total() {
        assert_eq!(bytes_fraction(0, 0), "");
        assert_eq!(
            bytes_fraction(150 * 1024 * 1024, 400 * 1024 * 1024),
            "150.0 MiB / 400.0 MiB"
        );
    }

    #[test]
    fn retry_badge_matches_model_policy() {
        use crate::demo;
        // errors mid-scan → retry
        let s = demo::sample(crate::demo::Scenario::Errors, 6);
        assert!(matches!(retry_badge(&s), Some(("⟳ RETRY", _))));
        // faulted → replace, not retry
        let f = demo::sample(crate::demo::Scenario::Faulted, 0);
        assert!(matches!(retry_badge(&f), Some(("✚ REPLACE", _))));
        // clean done → none
        let d = demo::sample(crate::demo::Scenario::Done, 0);
        assert!(retry_badge(&d).is_none());
    }

    #[test]
    fn surface_map_buckets_by_offset() {
        let mk = |off: u64| Ereport {
            vdev_path: "/dev/sdb".into(),
            vdev_guid: 1,
            io_offset: off,
            io_size: 4096,
            kind: crate::model::ErrKind::Checksum,
            ts: std::time::SystemTime::now(),
        };
        let size = 1 << 30; // 1 GiB
        let reps = vec![mk(0), mk(1000), mk(size / 2), mk(size - 1)];
        let grid = surface_grid(&reps.iter().collect::<Vec<_>>(), size, 2, 2);
        assert_eq!(grid[0][0], 2); // two at the start
        assert_eq!(grid[1][0], 1); // middle (row-major → [1][0])
        assert_eq!(grid[1][1], 1); // one at the end
        assert_eq!(surface_lines(&grid)[0], "░·");
        assert_eq!(surface_lines(&grid)[1], "░░");
    }

    #[test]
    fn hangul_ime_fallback_maps_hotkeys() {
        // The exact orcatui failure: q arrives as ㅂ in Korean IME mode.
        assert_eq!(ime_fallback('ㅂ'), 'q');
        // Digits row equivalents and navigation-relevant keys.
        assert_eq!(ime_fallback('ㅃ'), 'q'); // shift+q
        assert_eq!(ime_fallback('ㅌ'), 'x');
        assert_eq!(ime_fallback('ㅊ'), 'c');
        assert_eq!(ime_fallback('ㅍ'), 'v');
        // ASCII passes through untouched.
        assert_eq!(ime_fallback('q'), 'q');
        assert_eq!(ime_fallback('7'), '7');
        // Full coverage: every mapped value is a lowercase ASCII letter.
        for c in "ㅂㅈㄷㄱㅅㅛㅕㅑㅐㅔㅁㄴㅇㄹㅎㅗㅓㅏㅣㅋㅌㅊㅍㅠㅜㅡ".chars()
        {
            let out = ime_fallback(c);
            assert!(out.is_ascii_lowercase(), "{c} -> {out}");
        }
    }

    #[test]
    fn style_builder_smoke() {
        // ppalla 0.0.3 published-API smoke (style::StyleBuilder) — fg/bold
        // must convert to a ratatui Style correctly.
        let style: Style = StyleBuilder::new().foreground(Color::Red).bold().build();
        assert_eq!(style.fg, Some(Color::Red));
        assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
    }

    #[test]
    fn panel_title_style_marks_focused_panel_cyan_bold() {
        // Focused panel: the active-tab idiom (Cyan + bold).
        let focused = panel_title_style(PANEL_TREE, PANEL_TREE);
        assert_eq!(focused.fg, Some(Color::Cyan));
        assert!(focused.add_modifier.contains(Modifier::BOLD));
        // The other panel stays unstyled (no fg, no attributes).
        let unfocused = panel_title_style(PANEL_TREE, PANEL_MAP);
        assert_eq!(unfocused.fg, None);
        assert!(!unfocused.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn panel_border_style_marks_focused_panel_cyan_only() {
        // Focused border: Cyan accent without bold (title carries the
        // emphasis; the border must not over-shadow it).
        let focused = panel_border_style(PANEL_MAP, PANEL_MAP);
        assert_eq!(focused.fg, Some(Color::Cyan));
        assert!(!focused.add_modifier.contains(Modifier::BOLD));
        // Unfocused border: default.
        let unfocused = panel_border_style(PANEL_MAP, PANEL_MAP + 1);
        assert_eq!(unfocused.fg, None);
        assert!(!unfocused.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_style_healthy_inactive_keeps_existing_behavior() {
        // (a) Regression guard: healthy + not scanning + inactive is the
        // exact pre-health DarkGray tab — the feature is invisible on
        // healthy pools.
        let s = tab_style(PoolHealth::Healthy, false, false);
        assert_eq!(s.fg, Some(Color::DarkGray));
        assert!(!s.add_modifier.contains(Modifier::BOLD));
        assert_eq!(tab_markers(PoolHealth::Healthy, false), "");
        // Scanning-only healthy tab keeps the existing yellow.
        let sc = tab_style(PoolHealth::Healthy, true, false);
        assert_eq!(sc.fg, Some(Color::Yellow));
        assert_eq!(tab_markers(PoolHealth::Healthy, true), "⟳");
    }

    #[test]
    fn tab_style_degraded_inactive_is_yellow_with_marker() {
        // (b) Degraded + inactive → Yellow + '!'.
        let s = tab_style(PoolHealth::Degraded, false, false);
        assert_eq!(s.fg, Some(Color::Yellow));
        assert_eq!(pool_health_marker(PoolHealth::Degraded), "!");
        // Degraded + scanning: same yellow, both markers coexist.
        assert_eq!(tab_markers(PoolHealth::Degraded, true), "!⟳");
    }

    #[test]
    fn tab_style_fault_inactive_is_red_with_replace_marker() {
        // (c) Fault + inactive → Red + '✚' (REPLACE badge vocabulary).
        let s = tab_style(PoolHealth::Fault, false, false);
        assert_eq!(s.fg, Some(Color::Red));
        assert_eq!(pool_health_marker(PoolHealth::Fault), "✚");
        // Fault outranks the scanning yellow — a faulted pool under
        // resilver must stay red, with both markers shown ("✚⟳").
        let fs = tab_style(PoolHealth::Fault, true, false);
        assert_eq!(fs.fg, Some(Color::Red));
        assert_eq!(tab_markers(PoolHealth::Fault, true), "✚⟳");
    }

    #[test]
    fn tab_style_active_fault_keeps_cyan_bold_idiom() {
        // (d) Active + Fault: the learned active-tab idiom (Cyan+bold) is
        // preserved; the fault is carried by the marker glyph instead of
        // the color — dual encoding keeps it readable even here.
        let s = tab_style(PoolHealth::Fault, true, true);
        assert_eq!(s.fg, Some(Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));
        assert_eq!(tab_markers(PoolHealth::Fault, true), "✚⟳");
        // Active + healthy: unchanged idiom, scan marker only.
        let h = tab_style(PoolHealth::Healthy, true, true);
        assert_eq!(h.fg, Some(Color::Cyan));
        assert!(h.add_modifier.contains(Modifier::BOLD));
        assert_eq!(tab_markers(PoolHealth::Healthy, true), "⟳");
    }

    #[test]
    fn heat_cell_colors_are_named_ansi_only() {
        // RGB literals bypass the terminal theme — every density glyph
        // must map to a NAMED ANSI color so the theme can remap it.
        for c in ['·', '░', '▒', '▓', '█'] {
            assert!(
                !matches!(heat_cell_color(c), Color::Rgb(_, _, _)),
                "{c} must not map to an RGB literal"
            );
        }
        // Two-stepped low ramp: ░=Yellow, ▒=LightYellow (distinct).
        assert_eq!(heat_cell_color('░'), Color::Yellow);
        assert_eq!(heat_cell_color('▒'), Color::LightYellow);
        // Anchors: zero-density = label color, high-density = Red.
        assert_eq!(heat_cell_color('·'), Color::DarkGray);
        assert_eq!(heat_cell_color('▓'), Color::Red);
        assert_eq!(heat_cell_color('█'), Color::Red);
        // Unknown glyphs fall back to the label color like the renderer's
        // old inline match did.
        assert_eq!(heat_cell_color('x'), Color::DarkGray);
    }

    #[test]
    fn strip_colors_resets_colors_keeps_attributes_and_glyphs() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        buf[(0, 0)]
            .set_symbol("A")
            .set_fg(Color::Red)
            .set_bg(Color::Blue)
            .set_style(Style::new().add_modifier(Modifier::BOLD));
        buf[(1, 0)].set_symbol("░").set_fg(Color::Yellow);

        strip_colors(&mut buf);

        // Colors gone — fg AND bg reset to the terminal default.
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
        assert_eq!(buf[(0, 0)].bg, Color::Reset);
        assert_eq!(buf[(1, 0)].fg, Color::Reset);
        // Attributes survive: bold is an encoding channel, not decoration.
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        // Glyphs survive untouched (density/char channel intact).
        assert_eq!(buf[(0, 0)].symbol(), "A");
        assert_eq!(buf[(1, 0)].symbol(), "░");
    }
}
