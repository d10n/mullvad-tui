// SPDX-License-Identifier: GPL-3.0-or-later

//! Logs page - tail-follow of the most recent entries from the in-app
//! ring buffer. Layout is a single bordered panel filling the body area.
//!
//! The page has no focusable widgets - the tab bar is the only focus
//! target - so [`render`] takes no `FocusRegistry` and registers
//! nothing into it. That's the one signature asymmetry vs. the other
//! `pages::*::render` functions; everything else (Status, Account,
//! Settings family) registers buttons and is driven by arrows + Enter.
//!
//! `Home`/`End`/`PgUp`/`PgDn` drive a manual scroll offset on the
//! page state (`crate::app::pages::logs::PageState`). The default
//! `None` offset auto-tails the latest entry; pressing `Home`/`PgUp`
//! pins the viewport so new entries below the user don't disturb
//! the view, and `End` (or paging back to the bottom) re-engages
//! auto-tail.

use ansi_to_tui::IntoText;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Stylize},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use tracing::Level;

use crate::{
    app::App,
    logging::{LogEntry, LogSource},
    tui::components,
};

/// Render the Logs page body. Oldest visible entry at the top, newest at
/// the bottom - same convention as `tail -f`. When the buffer is empty
/// shows a single placeholder line so the panel never renders blank.
///
/// Long entries reflow within the panel: any line longer than the
/// inner panel width wraps onto subsequent visual rows rather than
/// being truncated. The bottom of the panel always shows the newest
/// entry; older entries scroll off the top via [`Paragraph::scroll`]
/// when their wrapped extent exceeds the available height.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let entries = app.log_buffer();
    let block = Block::bordered().title("Logs");
    let inner = block.inner(area);

    let lines: Vec<Line<'static>> = if entries.is_empty() {
        vec![Line::from("No log entries yet.")]
    } else {
        entries.iter().map(format_log_line).collect()
    };

    // Render the page frame first; the scrollbar (if any) is then
    // painted *on top of* the right border between the corners, so the
    // bar visually replaces that stretch of border. The paragraph
    // therefore gets the full inner width - the bar isn't competing
    // for content cells.
    frame.render_widget(block, area);

    let paragraph = Paragraph::new(lines)
        // Wrap long lines to the next row instead of truncating at
        // the panel's right border. `trim: false` preserves leading
        // whitespace inside an entry (multi-line daemon output, the
        // `[daemon] ` prefix) so wraps land at word boundaries
        // without re-flowing internal indentation.
        .wrap(Wrap { trim: false });

    // Manual scroll: the page state tracks a user-pinned offset
    // (`None` = auto-tail, `Some(n)` = pin top of viewport at row
    // `n`). `effective_scroll` resolves that against the current
    // total/viewport so a scrolled-up viewport doesn't follow new
    // entries until the user pages back to the bottom.
    let total_rows = u16::try_from(paragraph.line_count(inner.width)).unwrap_or(u16::MAX);
    let scroll = app
        .logs_page_state()
        .effective_scroll(total_rows, inner.height);
    // Cache dimensions for the input dispatch - `Home`/`End`/`PgUp`
    // /`PgDn` need them to clamp scroll moves and don't otherwise
    // see the panel rect.
    app.logs_page_state()
        .record_dimensions(total_rows, inner.height);

    frame.render_widget(paragraph.scroll((scroll, 0)), inner);

    // Overlay the scrollbar on the right border, only across rows
    // between the corners so the box-drawing corners stay intact.
    if total_rows > inner.height && area.width >= 1 && area.height >= 3 {
        let bar_area = Rect::new(
            area.x + area.width - 1,
            area.y + 1,
            1,
            area.height.saturating_sub(2),
        );
        components::render_vertical_scrollbar(
            frame,
            bar_area,
            total_rows as usize,
            scroll as usize,
            inner.height as usize,
        );
    }
}

/// Format one captured event as a styled ratatui `Line`.
///
/// **TUI entries** are uniformly colored by `tracing::Level`
/// (red/yellow/green/cyan/gray) - the structured event has no
/// embedded styling, so a single span suffices.
///
/// **Daemon entries** ship pre-formatted strings that may carry ANSI
/// escape sequences (the daemon emits colored output by default). We
/// parse those via `ansi-to-tui` so the rendered line picks up the
/// daemon's own color choices (level-coded reds/yellows/etc.) instead
/// of showing raw `\x1b[…m` sequences as garbage. A fixed magenta
/// `[daemon] ` prefix stays in front so daemon and TUI entries are
/// still visually separable in an intermixed stream.
///
/// Edge cases:
/// - **Parse failure**: fall back to a single magenta span with the stripped-of-trailing-newline
///   text. Better to render plain than to error out.
/// - **Multi-line ANSI input** (rare; daemon log lines are normally one line): take the first
///   parsed line. Each `LogEntry` is one row in the panel, so flattening to one line keeps the
///   layout regular.
fn format_log_line(entry: &LogEntry) -> Line<'static> {
    match &entry.source {
        LogSource::Tui { level, .. } => {
            let color = match *level {
                Level::ERROR => Color::Red,
                Level::WARN => Color::Yellow,
                Level::INFO => Color::Green,
                Level::DEBUG => Color::Cyan,
                Level::TRACE => Color::DarkGray,
            };
            Line::raw(entry.to_string()).fg(color)
        }
        LogSource::Daemon { line } => {
            let trimmed = line.trim_end_matches('\n');
            let prefix = Span::raw("[daemon] ").magenta();
            let mut spans = vec![prefix];
            match trimmed.into_text() {
                Ok(text) => {
                    if let Some(first) = text.lines.into_iter().next() {
                        spans.extend(first.spans);
                    }
                }
                Err(_) => spans.push(trimmed.to_string().magenta()),
            }
            Line::from(spans)
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Local;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::logging::LogSource;

    fn tui_entry(message: &str) -> LogEntry {
        LogEntry {
            timestamp: Local::now(),
            source: LogSource::Tui {
                level: Level::INFO,
                target: "tests".to_string(),
                message: message.to_string(),
            },
        }
    }

    fn render_screen(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let buf = terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                render(frame, area, app);
            })
            .unwrap();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn auto_tail_shows_the_most_recent_entries() {
        let mut app = App::new();
        for i in 0..50 {
            app.append_log_entry(tui_entry(&format!("entry-{i:02}")));
        }
        let screen = render_screen(&app, 60, 12);
        // Panel height 12, two border rows, ~10 inner rows. The
        // tail should include entry-49 (latest); entry-00 should
        // have scrolled off the top.
        let saw_latest = screen.iter().any(|line| line.contains("entry-49"));
        let saw_oldest = screen.iter().any(|line| line.contains("entry-00"));
        assert!(
            saw_latest && !saw_oldest,
            "auto-tail should show the latest entry, not the oldest; screen:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn scroll_to_top_pins_the_oldest_entries_into_view() {
        let mut app = App::new();
        for i in 0..50 {
            app.append_log_entry(tui_entry(&format!("entry-{i:02}")));
        }
        // First render to populate `last_dimensions` on the page
        // state - Home/PgUp call paths read those when clamping.
        let _ = render_screen(&app, 60, 12);
        app.logs_page_state().scroll_to_top();
        let screen = render_screen(&app, 60, 12);
        let saw_oldest = screen.iter().any(|line| line.contains("entry-00"));
        let saw_latest = screen.iter().any(|line| line.contains("entry-49"));
        assert!(
            saw_oldest && !saw_latest,
            "scroll_to_top should reveal the oldest entry and hide the latest; screen:\n{}",
            screen.join("\n"),
        );
    }

    #[test]
    fn scrollbar_overlays_the_right_border_between_corners() {
        // The scrollbar lives *on top of* the right border between the
        // top/bottom corners. The corners themselves stay intact (so
        // the page still reads as a bordered box) while the inner rows
        // show the bar instead of the border glyph.
        let mut app = App::new();
        for i in 0..200 {
            app.append_log_entry(tui_entry(&format!("entry-{i:03}")));
        }
        let width = 60;
        let height = 12;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let buf = terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                render(frame, area, &app);
            })
            .unwrap();

        // Top-right corner intact.
        assert_eq!(
            buf.buffer[(width - 1, 0)].symbol(),
            "┐",
            "top-right corner must stay intact",
        );
        // Bottom-right corner intact.
        assert_eq!(
            buf.buffer[(width - 1, height - 1)].symbol(),
            "┘",
            "bottom-right corner must stay intact",
        );

        // Every inner row at the rightmost column should be a scrollbar
        // glyph (`║` track or `█` thumb - see
        // [`crate::tui::components::render_vertical_scrollbar`]) -
        // *not* the original border `│` and not blank. The bar replaces
        // the border in this stretch.
        let track_or_thumb = |sym: &str| sym == "║" || sym == "█";
        for row in 1..height - 1 {
            let cell = buf.buffer[(width - 1, row)].symbol();
            assert!(
                track_or_thumb(cell),
                "inner row {row} at the rightmost column should be a scrollbar glyph, got {cell:?}",
            );
        }
    }

    #[test]
    fn page_down_from_top_eventually_re_engages_tail() {
        let mut app = App::new();
        for i in 0..50 {
            app.append_log_entry(tui_entry(&format!("entry-{i:02}")));
        }
        let _ = render_screen(&app, 60, 12);
        app.logs_page_state().scroll_to_top();
        // Walk down until the auto-tail kicks back in. Each PgDn
        // advances by viewport (~10 rows) and the buffer is ~50 / ~10 = ~5
        // pages tall, so 5 PgDns should land at the bottom.
        for _ in 0..5 {
            let (total, viewport) = app.logs_page_state().last_dimensions();
            app.logs_page_state().page_down(total, viewport);
            let _ = render_screen(&app, 60, 12);
        }
        let screen = render_screen(&app, 60, 12);
        let saw_latest = screen.iter().any(|line| line.contains("entry-49"));
        assert!(
            saw_latest,
            "PgDn x N should reach the bottom and show the latest entry; screen:\n{}",
            screen.join("\n"),
        );
    }
}
