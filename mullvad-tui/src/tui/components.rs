// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    prelude::Stylize,
    style::{Color, Style},
    symbols,
    text::Line,
    widgets::{Block, Clear, Paragraph, Wrap},
};

use crate::app::{
    FocusKind, FocusRegistry, FocusableWidget, PageFocus, PageId, TOP_LEVEL_PAGES, WidgetId,
};

/// Minimum terminal dimensions. Below this the dense pages lose
/// their bottom-anchored action buttons or truncate trailing labels;
/// we render `render_small_terminal_warning` instead.
pub(crate) const MIN_TERMINAL_WIDTH: u16 = 46;
pub(crate) const MIN_TERMINAL_HEIGHT: u16 = 30;

/// Maximum width the app renders into. On terminals wider than this
/// we center the app column and leave the surplus blank - the page
/// layouts are vertical lists with right-aligned trailing buttons,
/// and stretching them across a wide terminal makes the gap between
/// label and button uncomfortably wide. Matches [`PAGE_COLUMN_WIDTH`]
/// so per-page `centered_column` calls become near-no-ops once the
/// surrounding cap is in place.
const MAX_APP_WIDTH: u16 = 50;

/// Maximum height the app renders into. Above this we center the app
/// column vertically and draw a border around it so the user can see
/// where the app area ends.
const MAX_APP_HEIGHT: u16 = 36;

/// Column width sub-page renderers clamp their layout area to so that
/// label/button row pairs stay visually adjacent. Matches
/// [`MAX_APP_WIDTH`], so on terminals at the cap this is a no-op; on
/// terminals smaller than the cap (down to the 46-col floor) it
/// shrinks with the available area via the `if area.width <=
/// max_width` short-circuit in [`centered_column`].
///
/// Sub-pages that want more (e.g. logs, the relay-selector modal)
/// bypass this and use the full body width.
pub const PAGE_COLUMN_WIDTH: u16 = 50;

/// Carve a horizontally-centered column out of `area` whose width is
/// at most `max_width`. Existing row helpers already left-align labels
/// and right-anchor buttons inside the `area` they're given, so
/// narrowing the area at the page-renderer level shrinks the
/// label/button gap automatically without per-row changes.
///
/// On terminals narrower than `max_width` this is a no-op: returns
/// `area` unchanged so a small terminal still uses every column.
pub fn centered_column(area: Rect, max_width: u16) -> Rect {
    if area.width <= max_width {
        return area;
    }
    let pad = (area.width - max_width) / 2;
    Rect::new(area.x + pad, area.y, max_width, area.height)
}

/// Regions of the app frame after [`split_layout`] carves chrome off
/// the clamped app rect. The title lives in the bordered block's top
/// rule (drawn by `split_layout` itself), not in a dedicated header
/// row, so the inner area starts straight at the tab bar:
///
/// ```text
/// ┌───────mullvad-tui────[x]┐
/// │ tabs     (tab bar)      │   1 row
/// │ breadcrumb (sub-page)   │   1 row, only on sub-pages
/// │  body   (page content)  │   variable, 1ch padding L/R
/// │                         │   1-row margin
/// │ hint_bar (key hints)    │   2 rows (keys / labels)
/// └─────────────────────────┘
/// ```
///
/// `body` is the marginned region pages render their content into;
/// `body_full_width` is the same vertical slice without the 1ch L/R
/// padding, exposed for full-bleed elements (e.g. the Status page's
/// 3D map) that should ignore the body margin.
pub struct LayoutRegions {
    pub tabs: Rect,
    pub breadcrumb: Option<Rect>,
    pub body: Rect,
    pub body_full_width: Rect,
    pub hint_bar: Rect,
}

/// Horizontal padding subtracted from each side of the body region
/// so page content doesn't sit flush against the bordered chrome.
/// `body_full_width` reverses this padding for full-bleed elements
/// (the Status page's 3D map) that should ignore the margin.
pub const BODY_HORIZONTAL_PADDING: u16 = 1;

/// Carve the app rectangle out of the full terminal area, draw a
/// titled bordered block around it, and split the inside vertically
/// into the chrome regions defined by [`LayoutRegions`]. The app rect
/// is centered both horizontally and vertically and capped at
/// [`MAX_APP_WIDTH`] x [`MAX_APP_HEIGHT`]; any surplus is left blank.
///
/// `title` is the app banner that lives in the bordered block's top
/// rule (no dedicated header row). `is_sub_page` controls whether a
/// 1-row breadcrumb slot is reserved between the tab bar and the body.
///
/// The border & title are drawn here as a side effect so callers
/// don't have to coordinate "compute the rect" + "draw the border" +
/// "split the inside" separately.
pub fn split_layout(
    frame: &mut Frame<'_>,
    area: Rect,
    is_sub_page: bool,
    title: &str,
    page_focus: &PageFocus,
    registry: &mut FocusRegistry,
) -> LayoutRegions {
    let outer = clamp_to_app_area(area);
    let block = Block::bordered()
        .title(title.to_string())
        .border_style(Style::new().light_green())
        .title_alignment(Alignment::Center);
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    // `[x]` close button overlaid on the top border, just inside the
    // `┐` corner. Three cells wide (`[`, `x`, `]`) - registered as
    // its own focus row *before* the tab bar so the chrome rows
    // stack: row 0 = `[x]`, row 1 = tabs (rendered next by the run
    // loop). Both rows are recognized as `TabBar`-class chrome by
    // the focus engine, so `Tab` cycling and `first_body_widget`
    // continue to skip them as one unit.
    if outer.width >= 5 && outer.height >= 1 {
        let close_rect = Rect::new(outer.x + outer.width - 4, outer.y, 3, 1);
        let focused = page_focus.focused == Some(WINDOW_CLOSE);
        let style = if focused {
            Style::new().yellow()
        } else {
            Style::new().light_green()
        };
        frame.render_widget(Paragraph::new("[x]").style(style), close_rect);
        registry.register(FocusableWidget {
            id: WINDOW_CLOSE,
            rect: close_rect,
            kind: FocusKind::WindowClose,
        });
        registry.end_row();
    }

    // Two constraint vectors: the breadcrumb row only appears on
    // sub-pages so its slot disappears entirely on top-level pages
    // (rather than being drawn empty).
    if is_sub_page {
        let [tabs, breadcrumb, _gap1, body_full_width, _gap2, hint_bar] = Layout::vertical([
            Constraint::Length(1), // tabs
            Constraint::Length(1), // breadcrumb
            Constraint::Length(1), // 1-row margin between breadcrumb and body
            Constraint::Min(1),    // body
            Constraint::Length(1), // 1-row margin between body and hint bar
            Constraint::Length(2), // hint bar (key row + label row)
        ])
        .areas(inner);
        LayoutRegions {
            tabs,
            breadcrumb: Some(breadcrumb),
            body: pad_body_horizontally(body_full_width),
            body_full_width,
            hint_bar,
        }
    } else {
        let [tabs, _gap1, body_full_width, _gap2, hint_bar] = Layout::vertical([
            Constraint::Length(1), // tabs
            Constraint::Length(1), // 1-row margin between tabs and body
            Constraint::Min(1),    // body
            Constraint::Length(1), // 1-row margin between body and hint bar
            Constraint::Length(2), // hint bar (key row + label row)
        ])
        .areas(inner);
        LayoutRegions {
            tabs,
            breadcrumb: None,
            body: pad_body_horizontally(body_full_width),
            body_full_width,
            hint_bar,
        }
    }
}

/// Shrink `body` horizontally by [`BODY_HORIZONTAL_PADDING`] cells on
/// each side, clamping to a non-negative width on absurdly narrow
/// terminals (the small-terminal warning will already have kicked in
/// at that size, but the layout math still needs to produce a valid
/// rect).
fn pad_body_horizontally(body: Rect) -> Rect {
    let pad = BODY_HORIZONTAL_PADDING;
    let total_pad = pad.saturating_mul(2);
    if body.width <= total_pad {
        return Rect::new(body.x, body.y, 0, body.height);
    }
    Rect::new(body.x + pad, body.y, body.width - total_pad, body.height)
}

/// Clamp `area` to at most [`MAX_APP_WIDTH`] x [`MAX_APP_HEIGHT`],
/// centered both horizontally and vertically inside the original
/// area. Returns the original `area` unchanged on whichever axis it's
/// already within the cap.
fn clamp_to_app_area(area: Rect) -> Rect {
    let width = area.width.min(MAX_APP_WIDTH);
    let height = area.height.min(MAX_APP_HEIGHT);
    let pad_x = (area.width - width) / 2;
    let pad_y = (area.height - height) / 2;
    Rect::new(area.x + pad_x, area.y + pad_y, width, height)
}

pub fn is_small_terminal(area: Rect) -> bool {
    area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT
}

pub fn render_small_terminal_warning(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Terminal too small"),
            Line::from(""),
            Line::from("Please resize to at least"),
            Line::from(format!(
                "{} cols x {} rows",
                MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT
            )),
            Line::from(""),
            Line::from(format!("Currently {} x {}", area.width, area.height)),
            Line::from(""),
            Line::from("Press q to quit"),
        ])
        .alignment(Alignment::Center)
        .block(Block::bordered()),
        clamp_to_app_area(area),
    );
}

/// Reserved start of the tab-row widget-id slice. The four tab
/// buttons auto-increment from this base; per-page widgets pick ids
/// from the `0x10..` range to avoid collision.
const TAB_BASE: u32 = 0x00;

/// Stable widget ids for the four top-level tabs. Variants
/// auto-increment from [`TAB_BASE`] so adding or reordering tabs is
/// one line; the `tab_page_for_widget` / `tab_widget_id` mappers stay
/// the source of truth for the `PageId` / widget correspondence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
enum TabWidget {
    Status = TAB_BASE,
    Account,
    Settings,
    Logs,
}

impl TabWidget {
    const VARIANTS: &'static [Self] = &[Self::Status, Self::Account, Self::Settings, Self::Logs];

    const fn widget_id(self) -> WidgetId {
        WidgetId(self as u32)
    }

    fn page(self) -> PageId {
        match self {
            Self::Status => PageId::Status,
            Self::Account => PageId::Account,
            Self::Settings => PageId::Settings,
            Self::Logs => PageId::Logs,
        }
    }

    fn for_page(page: PageId) -> Option<Self> {
        match page {
            PageId::Status => Some(Self::Status),
            PageId::Account => Some(Self::Account),
            PageId::Settings => Some(Self::Settings),
            PageId::Logs => Some(Self::Logs),
            // Sub-pages don't get their own tab id; they inherit
            // their `top_level_root`'s.
            _ => None,
        }
    }
}

/// Look up the focus id for a given top-level page. Returns `None` for
/// sub-pages, which don't have their own tab. Use
/// [`tab_widget_id_for_top_level`] when you already know the page is
/// (or will be normalized to) a top-level root.
pub fn tab_widget_id(page: PageId) -> Option<WidgetId> {
    TabWidget::for_page(page).map(TabWidget::widget_id)
}

/// Infallible variant of [`tab_widget_id`] for callers that already
/// hold a top-level page (or a `top_level_root()` of one). Normalizes
/// internally so callers don't have to repeat the dance.
pub fn tab_widget_id_for_top_level(page: PageId) -> WidgetId {
    let root = page.top_level_root();
    tab_widget_id(root).expect("top_level_root yields a top-level page with a tab")
}

/// Inverse of `tab_widget_id`: which page does this focus id correspond
/// to? Returns `None` if the id isn't a tab.
pub fn tab_page_for_widget(id: WidgetId) -> Option<PageId> {
    TabWidget::VARIANTS
        .iter()
        .find(|t| t.widget_id() == id)
        .copied()
        .map(TabWidget::page)
}

/// Render the top-of-page tab bar via [`ratatui::widgets::Tabs`] and
/// register each tab in the focus registry so arrow keys can navigate
/// to them. The active tab (the page's `top_level_root`) renders with
/// an underline modifier baked into its `Line` style; the focused tab
/// (if any) gets the highlight style applied via `Tabs::select` +
/// `highlight_style`. The two indicators stack: a tab that's both
/// active and focused renders underlined and yellow.
pub fn render_tab_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    current_page: PageId,
    page_focus: &PageFocus,
    registry: &mut FocusRegistry,
) {
    use ratatui::widgets::Tabs;

    let active_root = current_page.top_level_root();
    let titles: Vec<Line<'static>> = TOP_LEVEL_PAGES
        .iter()
        .map(|&page| {
            let label = page.tab_label();
            let line = Line::from(format!("[{label}]"));
            if page == active_root {
                line.underlined()
            } else {
                line
            }
        })
        .collect();

    // Mirror ratatui's default `Tabs` layout (1-cell padding around
    // each label, 1-cell divider between tabs) so our focus rects
    // line up with the painted glyphs.
    const TAB_PADDING: u16 = 1;
    let total_w = titles
        .iter()
        .map(|t| t.width() + (TAB_PADDING * 2) as usize)
        .sum::<usize>()
        + titles.len().saturating_sub(1);
    let bar_area = area.centered_horizontally(Constraint::Length(total_w as u16));

    let constraints: Vec<Constraint> = titles
        .iter()
        .map(|t| Constraint::Length(t.width() as u16 + TAB_PADDING * 2))
        .collect();
    let tab_areas = Layout::horizontal(constraints).spacing(1).split(bar_area);

    for (i, &page) in TOP_LEVEL_PAGES.iter().enumerate() {
        // Shrink by `TAB_PADDING` on each side so the hit area covers
        // only the `[Label]` glyphs, not the surrounding pad cells.
        let painted = Rect {
            x: tab_areas[i].x + TAB_PADDING,
            width: tab_areas[i].width.saturating_sub(TAB_PADDING * 2),
            ..tab_areas[i]
        };
        registry.register(FocusableWidget {
            id: tab_widget_id_for_top_level(page),
            rect: painted,
            kind: FocusKind::TabButton,
        });
    }
    registry.end_row();

    // Highlight the focused tab via `highlight_style`; the underline
    // baked into `titles` independently marks the active root. When
    // no tab has focus, pass `None` so nothing gets highlighted -
    // otherwise the active tab would pick up the yellow as well.
    let focused_idx = page_focus
        .focused
        .and_then(tab_page_for_widget)
        .and_then(|p| TOP_LEVEL_PAGES.iter().position(|&pp| pp == p));

    let tabs = Tabs::new(titles)
        .select(focused_idx)
        .highlight_style(Style::new().yellow())
        .divider(symbols::DOT)
        .padding(" ", " ");

    frame.render_widget(tabs, bar_area);
}

/// Render the breadcrumb row that appears below the tab bar on
/// sub-pages. `segments` is the chain of crumbs from the top-level
/// root down to the current page (e.g.
/// `[("Settings", false), ("VPN", true)]`); the active segment (the
/// current page) renders underlined to match the active-tab
/// convention.
///
/// A `[<]` back button is pinned to the left edge of the row and
/// registered in the focus registry under [`BREADCRUMB_BACK`] so the
/// user can both keyboard- and mouse-activate it. Activation pops
/// one sub-page frame, same outcome as `Esc` on a sub-page. The
/// crumb chain itself stays centered in the remaining space.
pub fn render_breadcrumb(
    frame: &mut Frame<'_>,
    area: Rect,
    segments: &[(&str, bool)],
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    use ratatui::text::Span;

    if segments.is_empty() {
        return;
    }

    // Reserve `[<]` (3 cells) plus a 1-cell gap on the left; the
    // crumb chain centers in the remaining space.
    const BACK_LABEL: &str = "<";
    let back_width = (BACK_LABEL.len() as u16) + 2; // brackets
    let [back_area, _gap, crumbs_area] = Layout::horizontal([
        Constraint::Length(back_width),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    let back_focused = focused == Some(BREADCRUMB_BACK);
    let back_style = if back_focused {
        Style::new().yellow()
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(format!("[{BACK_LABEL}]")).style(back_style),
        back_area,
    );
    registry.register(FocusableWidget {
        id: BREADCRUMB_BACK,
        rect: back_area,
        kind: FocusKind::BreadcrumbBack,
    });
    registry.end_row();

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segments.len() * 2);
    for (i, (label, is_active)) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" > "));
        }
        let style = if *is_active {
            Style::new().underlined()
        } else {
            Style::new()
        };
        spans.push(Span::styled((*label).to_string(), style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        crumbs_area,
    );
}

/// Render the bottom hint bar as a 2-row x N-column table - keys on
/// the top row (each on a dim-gray `<kbd>`-style background), labels
/// on the bottom row. Each `(key, label)` pair occupies one column
/// sized to its widest content (key vs label), and the surplus row
/// width is distributed *around* the columns via [`Flex::SpaceAround`]:
/// each column gets equal padding on both sides, with the outer
/// margins half the size of the inter-column gaps. The result reads
/// as labeled affordances with breathing room on the edges rather
/// than a centered quartet flush against the chrome.
///
/// Caller must reserve **2** rows in the layout for `area`; see
/// [`split_layout`]'s `hint_bar` slot.
pub fn render_hint_bar(frame: &mut Frame<'_>, area: Rect, hints: &[(&str, &str)]) {
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;

    if hints.is_empty() {
        return;
    }

    let key_style = Style::new().white().on_dark_gray();

    // Vertical: top row = keys, bottom row = labels.
    let [key_row, label_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    // Each column's natural width is `max(key, label)` so the key
    // span (with its background fill) and the label below it both
    // fit. SpaceAround then equalises the padding on both sides of
    // every column (outer margins are half the inter-column gap),
    // so the leftmost/rightmost hints don't sit flush against the
    // chrome edges.
    let columns: Vec<Constraint> = hints
        .iter()
        .map(|(key, label)| {
            let w = UnicodeWidthStr::width(*key).max(UnicodeWidthStr::width(*label)) as u16;
            Constraint::Length(w)
        })
        .collect();
    let key_chunks = Layout::horizontal(columns.clone())
        .flex(Flex::SpaceAround)
        .split(key_row);
    let label_chunks = Layout::horizontal(columns)
        .flex(Flex::SpaceAround)
        .split(label_row);

    for (i, (key, label)) in hints.iter().enumerate() {
        let key_line = Line::from(Span::styled((*key).to_string(), key_style));
        frame.render_widget(
            Paragraph::new(key_line).alignment(Alignment::Center),
            key_chunks[i],
        );
        frame.render_widget(
            Paragraph::new((*label).to_string()).alignment(Alignment::Center),
            label_chunks[i],
        );
    }
}

// ---- Glyph primitives ----
//
// Small helpers used by per-page row builders: `[x]/[ ]` checkboxes,
// `(•)/( )` radios, `>/v/^` chevrons, and the standard 10-frame
// Braille spinner.

/// Glyph for a checkbox indicator. `[x]` checked, `[ ]` unchecked.
pub fn checkbox_glyph(checked: bool) -> &'static str {
    if checked { "[x]" } else { "[ ]" }
}

/// Glyph for a radio indicator. `(•)` selected, `( )` unselected.
pub fn radio_glyph(selected: bool) -> &'static str {
    if selected { "(•)" } else { "( )" }
}

/// Direction tag for [`chevron`]. `Down`/`Up` for tree expand/collapse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Chevron {
    Down,
    Up,
}

/// Block-cursor span for an inline text-input pill, in the pill's
/// reverse-video style (yellow on the supplied background). Centralizes
/// the glyph + styling used by the search anchor and MTU pill so they
/// stay visually identical.
pub fn cursor_glyph_span(bg: Color) -> ratatui::text::Span<'static> {
    ratatui::text::Span::styled("█", Style::new().yellow().bg(bg))
}

/// Glyph for a navigation chevron.
pub fn chevron(dir: Chevron) -> &'static str {
    match dir {
        Chevron::Down => "▼",
        Chevron::Up => "▲",
    }
}

/// Spinner frame for animated waiting indicators. `tick` is a
/// per-frame counter from the run loop's ticker (already used by the
/// globe camera animation). Cycles through the standard 10-frame
/// Braille spinner.
pub fn spinner_frame(tick: u64) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(tick % FRAMES.len() as u64) as usize]
}

/// `render_button` variant that prefixes a spinner glyph when the
/// caller indicates an operation tied to this button is in flight.
/// The label becomes `[⠋ Label]` (with the spinner frame derived
/// from `tick`); when `running` is false this is identical to
/// [`render_button`]. Used by the Status page's Connect / Disconnect /
/// Reconnect buttons.
#[expect(
    clippy::too_many_arguments,
    reason = "spinner variant adds running/tick on top of render_button's 6 args; \
              splitting into a struct would add a layer for one call site"
)]
pub fn render_button_running(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    style: Style,
    running: bool,
    tick: u64,
    registry: &mut FocusRegistry,
    id: WidgetId,
) {
    let text = if running {
        format!("[{} {label}]", spinner_frame(tick))
    } else {
        format!("[{label}]")
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
    registry.register(FocusableWidget {
        id,
        rect: area,
        kind: FocusKind::Button,
    });
}

/// Render a `<label>     [<button>]` row in the standard
/// label-fills-left, button-anchored-right shape. Computes the layout,
/// paints both halves, and registers `button_id` for focus. Callers
/// pre-format the label (so trailing summaries like `Direct only: On`
/// stay one allocation each) and supply the bare button text without
/// brackets.
pub fn render_label_button_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label: String,
    button_label: &str,
    button_id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let button_text_width = (button_label.len() as u16).saturating_add(2); // "[" + "]"
    let [label_area, _gap, button_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(button_text_width),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(label), label_area);
    render_button(
        frame,
        button_area,
        button_label,
        focused == Some(button_id),
        registry,
        button_id,
    );
    registry.end_row();
}

/// Render a single bracketed-label button at `area` and register it in the
/// focus registry as a `Button` kind. The visible label is `[label]`;
/// focused buttons render in yellow. The full `area` is the focusable
/// rect - callers control sizing/positioning via `Layout` splits.
pub fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    registry: &mut FocusRegistry,
    id: WidgetId,
) {
    let style = if focused {
        Style::new().yellow()
    } else {
        Style::new()
    };
    let text = format!("[{label}]");
    frame.render_widget(Paragraph::new(text).style(style), area);
    registry.register(FocusableWidget {
        id,
        rect: area,
        kind: FocusKind::Button,
    });
}

/// Danger variant of [`render_button`] - used by destructive actions
/// (Disconnect, Log out, Disconnect & quit). The focused style flips
/// the foreground to yellow so the focus highlight reads cleanly.
pub fn render_button_danger(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    registry: &mut FocusRegistry,
    id: WidgetId,
) {
    let style = if focused {
        Style::new().yellow()
    } else {
        Style::new().red()
    };
    let text = format!("[{label}]");
    frame.render_widget(Paragraph::new(text).style(style), area);
    registry.register(FocusableWidget {
        id,
        rect: area,
        kind: FocusKind::Button,
    });
}

/// Center `[label]` horizontally inside `area`, register the tight
/// button rect, and end the focus row. Single-button rows on
/// per-page renderers (e.g. `[Add address]`, `[Manage devices]`)
/// share this helper.
pub fn render_centered_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let button_area = centered_horizontal(area, (label.len() as u16).saturating_add(2));
    render_button(frame, button_area, label, focused == Some(id), registry, id);
    registry.end_row();
}

/// Danger variant of [`render_centered_button`] - red background, used
/// for destructive single-row actions (`[Log out]`, `[Disconnect & quit]`).
pub fn render_centered_button_danger(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let button_area = centered_horizontal(area, (label.len() as u16).saturating_add(2));
    render_button_danger(frame, button_area, label, focused == Some(id), registry, id);
    registry.end_row();
}

/// Render a text-input popup: bordered box with `title` in the
/// border, a `prompt` line at the top, the user's `buffer`
/// underneath, and a `[Cancel] [<submit_label>]` button row at the
/// bottom. The yellow focus highlight tracks `focus` so the user
/// can see which element Enter will activate.
///
/// The InputMode handler keeps consuming keys directly - the modal
/// is its own focus state machine (`InputFocus::next`/`prev` for
/// Tab/arrow nav), so the visual highlight is driven by `focus`
/// rather than the page focus engine. Buttons are still registered
/// against `registry` though, so mouse clicks can hit-test and
/// dispatch through `activate_focused` like any other button.
pub fn render_input_prompt(
    frame: &mut Frame<'_>,
    title: &str,
    prompt: &str,
    buffer: &str,
    submit_label: &str,
    focus: crate::tui::modals::InputFocus,
    registry: &mut FocusRegistry,
) {
    use crate::tui::modals::InputFocus;

    let popup = centered_rect(60, 30, frame.area());
    // Dim the page beneath the popup, matching the confirm/notification
    // overlays. Order matters: `dim_frame_below` walks the buffer cells
    // *outside* the popup rect, so it must run before `Clear` (and
    // before the popup's own widgets) - otherwise it would paint over
    // the popup's content.
    dim_frame_below(frame, popup);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_style(Style::new().light_green())
        .title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [prompt_area, buffer_area, _spacer, button_row] = Layout::vertical([
        Constraint::Length(1), // prompt
        Constraint::Length(1), // buffer
        Constraint::Min(1),    // spacer pushes the buttons to the bottom
        Constraint::Length(1), // [Cancel] [Submit] row
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(prompt).wrap(Wrap { trim: true }),
        prompt_area,
    );

    // Highlight the buffer line yellow when the field has focus so
    // the user can tell that typing will land here vs. on a button.
    let field_style = if matches!(focus, InputFocus::Field) {
        Style::new().yellow()
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(format!("> {buffer}")).style(field_style),
        buffer_area,
    );
    // Register the buffer rect under `INPUT_MODAL_FIELD` so a click
    // on the row resets the modal's internal focus to `Field` and
    // the user can type without having to Tab back over from a
    // button. Hit-test only - keyboard input never queries this id
    // because the modal's `handle_key` handles char/backspace
    // directly via its own focus state.
    registry.register(FocusableWidget {
        id: INPUT_MODAL_FIELD,
        rect: buffer_area,
        kind: FocusKind::TextInput,
    });

    let cancel_label = "Cancel";
    let cancel_w = (cancel_label.len() as u16) + 2;
    let submit_w = (submit_label.len() as u16) + 2;
    const GAP: u16 = 2;
    let row_w = cancel_w + GAP + submit_w;
    let centered_row = centered_horizontal(button_row, row_w);
    let [cancel_area, _gap, submit_area] = Layout::horizontal([
        Constraint::Length(cancel_w),
        Constraint::Length(GAP),
        Constraint::Length(submit_w),
    ])
    .areas(centered_row);

    render_modal_button(
        frame,
        cancel_area,
        cancel_label,
        matches!(focus, InputFocus::Cancel),
        INPUT_MODAL_CANCEL,
        registry,
    );
    render_modal_button(
        frame,
        submit_area,
        submit_label,
        matches!(focus, InputFocus::Submit),
        INPUT_MODAL_SUBMIT,
        registry,
    );
}

/// One labeled field in a [`render_multi_field_input_prompt`] call:
/// `label` is printed on its own row above the buffer; `buffer` is the
/// current contents of the field; `focused` flips the buffer row to
/// yellow so the user knows where typing will land.
pub struct InputField<'a> {
    pub label: &'a str,
    pub buffer: &'a str,
    pub focused: bool,
}

/// Multi-field text-input popup. Same shape as [`render_input_prompt`]
/// but with N labeled buffer rows above the `[Cancel] [<submit_label>]`
/// row. Each field renders as a "label" line + a `> buffer` line; the
/// buffer line turns yellow when its `focused` flag is set. The modal's
/// `handle_key` owns the focus state machine across fields - callers
/// just pass the per-field focused booleans they've already computed.
///
/// Only the *first* field row is registered for mouse hit-testing
/// under [`INPUT_MODAL_FIELD`], matching the single-field renderer's
/// behavior (clicking the buffer area re-focuses the field). Each
/// field gets its own id (`INPUT_MODAL_FIELD_BASE + index`) so a mouse
/// click on the v6 row, for instance, lands focus on the v6 buffer
/// rather than the first field.
#[expect(
    clippy::too_many_arguments,
    reason = "popup helper bundles title + prompt + per-field state + submit label \
              + cancel/submit focus + registry; splitting into a struct adds a layer \
              for one call site"
)]
pub fn render_multi_field_input_prompt(
    frame: &mut Frame<'_>,
    title: &str,
    prompt: &str,
    fields: &[InputField<'_>],
    submit_label: &str,
    cancel_focused: bool,
    submit_focused: bool,
    registry: &mut FocusRegistry,
) {
    let popup = centered_rect(60, 50, frame.area());
    dim_frame_below(frame, popup);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_style(Style::new().light_green())
        .title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Pre-wrap the prompt so the layout reserves the right number of
    // rows for it. With a fixed `Length(1)` slot, anything past the
    // first row gets clipped instead of wrapping onto the next line.
    let prompt_para = Paragraph::new(prompt).wrap(Wrap { trim: true });
    let prompt_height = u16::try_from(prompt_para.line_count(inner.width))
        .unwrap_or(u16::MAX)
        .max(1);

    // Vertical layout: wrapped prompt + per-field (label, buffer) pairs
    // + spacer + button row.
    let mut constraints: Vec<Constraint> =
        vec![Constraint::Length(prompt_height), Constraint::Length(1)];
    for _ in fields {
        constraints.push(Constraint::Length(1)); // label
        constraints.push(Constraint::Length(1)); // buffer
        constraints.push(Constraint::Length(1)); // gap
    }
    constraints.push(Constraint::Min(1)); // spacer
    constraints.push(Constraint::Length(1)); // button row
    let rows = Layout::vertical(constraints).split(inner);

    frame.render_widget(prompt_para, rows[0]);
    // rows[1] is the blank below the prompt. Each subsequent block of
    // three rows holds {label, buffer, gap} for one field; register the
    // buffer rect under `INPUT_MODAL_FIELD_BASE + i` so the click
    // dispatch can route a mouse click on (say) the v4 buffer to the
    // modal's `Ipv4` focus position rather than always landing on the
    // first field.
    let mut idx = 2;
    for (field_index, field) in fields.iter().enumerate() {
        frame.render_widget(Paragraph::new(field.label.to_string()), rows[idx]);
        let buffer_rect = rows[idx + 1];
        let style = if field.focused {
            Style::new().yellow()
        } else {
            Style::new()
        };
        frame.render_widget(
            Paragraph::new(format!("> {}", field.buffer)).style(style),
            buffer_rect,
        );
        if (field_index as u32) < INPUT_MODAL_FIELD_MAX {
            registry.register(FocusableWidget {
                id: WidgetId(INPUT_MODAL_FIELD_BASE.0 + field_index as u32),
                rect: buffer_rect,
                kind: FocusKind::TextInput,
            });
        }
        idx += 3; // label + buffer + gap
    }

    let button_row = rows[rows.len() - 1];
    let cancel_label = "Cancel";
    let cancel_w = (cancel_label.len() as u16) + 2;
    let submit_w = (submit_label.len() as u16) + 2;
    const GAP: u16 = 2;
    let row_w = cancel_w + GAP + submit_w;
    let centered_row = centered_horizontal(button_row, row_w);
    let [cancel_area, _gap, submit_area] = Layout::horizontal([
        Constraint::Length(cancel_w),
        Constraint::Length(GAP),
        Constraint::Length(submit_w),
    ])
    .areas(centered_row);

    render_modal_button(
        frame,
        cancel_area,
        cancel_label,
        cancel_focused,
        INPUT_MODAL_CANCEL,
        registry,
    );
    render_modal_button(
        frame,
        submit_area,
        submit_label,
        submit_focused,
        INPUT_MODAL_SUBMIT,
        registry,
    );
}

/// Paint a `[label]` button with a yellow highlight when `focused`,
/// and register the rect under `id` so mouse clicks can hit-test
/// it. The modal owns the *visual* focus highlight via
/// `InputFocus`; the registry entry exists purely for click
/// dispatch - keyboard input still flows through
/// `InputMode::handle_key` and never queries the focus engine.
fn render_modal_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    id: WidgetId,
    registry: &mut FocusRegistry,
) {
    let style = if focused {
        Style::new().yellow()
    } else {
        Style::new()
    };
    frame.render_widget(Paragraph::new(format!("[{label}]")).style(style), area);
    registry.register(FocusableWidget {
        id,
        rect: area,
        kind: FocusKind::Button,
    });
}

/// Reserved start of the cross-page-overlay widget-id slice. `0xF0..`
/// is far above every per-page slice (Status `0x10..`, Account
/// `0x20..`, Settings `0x40..`) so overlays can never collide with
/// page widgets even if a page slice grows.
const OVERLAY_BASE: u32 = 0xF0;

/// Stable widget ids for the cross-page overlay buttons. The
/// `OVERLAY_*` `pub const`s below preserve the public-facing names
/// callers already use; this enum is the single source of truth for
/// the underlying numeric values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
enum OverlayWidget {
    ConfirmReject = OVERLAY_BASE,
    ConfirmAccept,
    NotificationDismiss,
    /// `[Cancel]` button on an input modal. Mouse-clickable; the
    /// equivalent keyboard path (Esc / Enter on the focused
    /// `[Cancel]`) flows through `InputMode::handle_key` and never
    /// reaches the focus engine.
    InputModalCancel,
    /// `[Submit]` (or modal-specific verb like `[Redeem]`, `[Add]`)
    /// button on an input modal. Same key-vs-mouse split as
    /// `InputModalCancel`.
    InputModalSubmit,
    /// Sub-page breadcrumb back button (`[<]`). Visible only on
    /// sub-pages, where it pops one stack frame, same outcome as
    /// `Esc`.
    BreadcrumbBack,
    /// Text-entry buffer row inside an input modal. Registered for
    /// mouse hit-testing only - keyboard never lands here because
    /// the modal's internal `InputFocus::Field` already drives
    /// where typed chars go. Clicking this rect just resets the
    /// modal's internal focus to `Field` so the user can type
    /// after navigating away to a button.
    InputModalField,
    /// `[x]` button drawn on the top-right of the outer frame
    /// border. Activation calls `App::quit`. Mouse-clickable and
    /// keyboard-reachable via `Tab` / arrow Up from the tab bar.
    WindowClose,
}

impl OverlayWidget {
    const fn widget_id(self) -> WidgetId {
        WidgetId(self as u32)
    }
}

pub const OVERLAY_CONFIRM_REJECT: WidgetId = OverlayWidget::ConfirmReject.widget_id();
pub const OVERLAY_CONFIRM_ACCEPT: WidgetId = OverlayWidget::ConfirmAccept.widget_id();
pub const OVERLAY_NOTIFICATION_DISMISS: WidgetId = OverlayWidget::NotificationDismiss.widget_id();
pub const INPUT_MODAL_CANCEL: WidgetId = OverlayWidget::InputModalCancel.widget_id();
pub const INPUT_MODAL_SUBMIT: WidgetId = OverlayWidget::InputModalSubmit.widget_id();
pub const BREADCRUMB_BACK: WidgetId = OverlayWidget::BreadcrumbBack.widget_id();
pub const INPUT_MODAL_FIELD: WidgetId = OverlayWidget::InputModalField.widget_id();
pub const WINDOW_CLOSE: WidgetId = OverlayWidget::WindowClose.widget_id();
/// Base widget id for per-field buffer rects on a multi-field input
/// modal. Each field gets `INPUT_MODAL_FIELD_BASE + index`. Lets the
/// mouse-click dispatch route a click on (say) the IPv6 buffer to the
/// modal's *internal* per-field focus state, rather than treating every
/// field rect as the same generic [`INPUT_MODAL_FIELD`]. Anchored after
/// [`WINDOW_CLOSE`] so it doesn't alias.
pub const INPUT_MODAL_FIELD_BASE: WidgetId = WidgetId(WINDOW_CLOSE.0 + 1);
/// Cap on per-field rows in the focus registry. Three is enough for
/// every multi-field modal we have today (relay-override: hostname +
/// v4 + v6); the slack leaves room for a wider modal to grow without
/// reshuffling adjacent id ranges.
pub const INPUT_MODAL_FIELD_MAX: u32 = 8;

/// Decode a per-field widget id back to its 0-based field index, or
/// `None` if the id isn't in the multi-field range. Used by the click
/// dispatch in `tui/mod.rs` to route the click to the right buffer.
pub fn input_modal_field_index(widget: WidgetId) -> Option<usize> {
    let base = INPUT_MODAL_FIELD_BASE.0;
    if widget.0 >= base && widget.0 < base + INPUT_MODAL_FIELD_MAX {
        Some((widget.0 - base) as usize)
    } else {
        None
    }
}

/// Focus-engine-driven confirmation overlay. Renders a centered
/// bordered box with two focusable buttons - `[Cancel]` then
/// `[Confirm]`. The Cancel button registers first, so when a
/// confirmation modal opens after the registry is reset (page
/// widgets dropped from focus), the focus engine snaps to it as the
/// safer default.
///
/// Caller is responsible for clearing the registry of page widgets
/// before calling this - the overlay assumes its buttons are the only
/// focusable widgets on the frame, so arrow keys cycle within the
/// overlay rather than leaking back to the page beneath.
pub fn render_confirm_overlay(
    frame: &mut Frame<'_>,
    title: &str,
    message: &str,
    registry: &mut FocusRegistry,
    focused: Option<WidgetId>,
) {
    // First-frame highlight fix: on the frame the modal opens,
    // `focused` is still the page widget that triggered the modal,
    // so neither overlay button matches and nothing renders
    // highlighted. `set_focus_registry` will then snap focus to
    // Cancel for the *next* frame. Pre-empt that here so the
    // highlight appears immediately on the first frame the user sees.
    let effective = match focused {
        Some(id) if id == OVERLAY_CONFIRM_ACCEPT || id == OVERLAY_CONFIRM_REJECT => focused,
        _ => Some(OVERLAY_CONFIRM_REJECT),
    };

    // Size the popup to the message: width grows with the longest
    // line up to a cap (so long messages wrap rather than
    // monopolising the frame), and height follows the post-wrap row
    // count. Backdrop is dimmed; `Clear` wipes page content under
    // the popup before the box draws.
    let message_para = Paragraph::new(message.to_string())
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true });
    let popup = sized_overlay_rect(frame.area(), message, &message_para);
    dim_frame_below(frame, popup);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().light_green());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Inside the bordered box: wrapped message at top, blank line,
    // single-row button strip at the bottom.
    let [message_area, _blank, button_row] = Layout::vertical([
        Constraint::Min(1),    // message body
        Constraint::Length(1), // blank
        Constraint::Length(1), // button row
    ])
    .areas(inner);

    frame.render_widget(message_para, message_area);

    // Two halves of equal width; place each button centered inside its
    // half. Cancel is left, Confirm is right - matches the platform
    // convention where the safe-default button is on the left and the
    // commit-action button is on the right.
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(button_row);

    render_centered_button_row(
        frame,
        left,
        "Cancel",
        OVERLAY_CONFIRM_REJECT,
        effective,
        registry,
    );
    render_centered_button_row(
        frame,
        right,
        "Confirm",
        OVERLAY_CONFIRM_ACCEPT,
        effective,
        registry,
    );
    registry.end_row();
}

/// Focus-engine-driven notification overlay. Same shape as
/// [`render_confirm_overlay`] but with a single `[Dismiss]` button -
/// the registry-reset model gives the overlay exclusive focus while
/// it's open, so arrow keys can't navigate the page beneath and Enter
/// activates the dismiss button rather than whatever page widget was
/// focused before the notification appeared.
///
/// Caller is responsible for resetting the registry of page widgets
/// before calling this - see the matching pattern in
/// [`render_confirm_overlay`].
pub fn render_notification_overlay(
    frame: &mut Frame<'_>,
    message: &str,
    registry: &mut FocusRegistry,
    focused: Option<WidgetId>,
) {
    // First-frame highlight fix (see `render_confirm_overlay` for the
    // detailed rationale): show the dismiss button as focused on the
    // frame the modal opens, matching where `set_focus_registry` will
    // snap the focus after the render anyway.
    let effective = match focused {
        Some(id) if id == OVERLAY_NOTIFICATION_DISMISS => focused,
        _ => Some(OVERLAY_NOTIFICATION_DISMISS),
    };

    // Size the popup to its contents: width grows with the longest
    // line up to a cap (so longer messages wrap rather than
    // monopolising the frame), and height follows the post-wrap row
    // count.
    let message_para = Paragraph::new(message.to_string())
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true });
    let popup = sized_overlay_rect(frame.area(), message, &message_para);
    dim_frame_below(frame, popup);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title("Notification")
        .border_style(Style::new().light_green());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Inside the bordered box: wrapped message at top, blank line,
    // single-row button strip at the bottom (one centered button).
    let [message_area, _blank, button_row] = Layout::vertical([
        Constraint::Min(1),    // message body
        Constraint::Length(1), // blank
        Constraint::Length(1), // button row
    ])
    .areas(inner);

    frame.render_widget(message_para, message_area);

    render_centered_button_row(
        frame,
        button_row,
        "Dismiss",
        OVERLAY_NOTIFICATION_DISMISS,
        effective,
        registry,
    );
    registry.end_row();
}

/// Center `[label]` horizontally inside `area`, then register the tight
/// button rect (just the bracketed text) in the focus registry. Used by
/// [`render_confirm_overlay`] to lay out the two row buttons so their
/// focusable hit-rects don't claim the surrounding whitespace.
fn render_centered_button_row(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    id: WidgetId,
    focused: Option<WidgetId>,
    registry: &mut FocusRegistry,
) {
    let button_area = centered_horizontal(area, (label.len() as u16).saturating_add(2));
    render_button(frame, button_area, label, focused == Some(id), registry, id);
}

/// Centered 1-row sub-rectangle of `area` with the given `width`.
/// Replaces the recurring `let pad = area.width.saturating_sub(w) / 2;
/// Rect::new(area.x + pad, area.y, w, 1)` pattern (and clamps `width`
/// to `area.width` so a button that wouldn't fit at all still gets a
/// non-overflowing rect rather than spilling past the column).
pub fn centered_horizontal(area: Rect, width: u16) -> Rect {
    let width = width.min(area.width);
    let pad = area.width.saturating_sub(width) / 2;
    Rect::new(area.x + pad, area.y, width, 1)
}

/// Reserve the rightmost column of `area` for a vertical scrollbar
/// when `content_length > viewport`. Returns the (body, scrollbar)
/// rect pair; `scrollbar` is `None` when no overflow exists so callers
/// can keep the full width. Shrinks `body.width` by 1 column in the
/// overflow case - callers must lay out their content into `body`,
/// not the original `area`.
pub fn split_for_vertical_scrollbar(
    area: Rect,
    content_length: usize,
    viewport: usize,
) -> (Rect, Option<Rect>) {
    if content_length <= viewport || area.width < 2 {
        return (area, None);
    }
    let [body, bar] = Layout::horizontal([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    (body, Some(bar))
}

/// Render a vertical scrollbar at `area` representing `position`
/// within `content_length` items, given a `viewport` of how many
/// items are visible at once. `position` is the index of the *top*
/// row in the visible window (so it ranges from `0` to
/// `content_length - viewport`).
///
/// Uses a straightforward proportional thumb: thumb-length is
/// `viewport * track / content_length` (>= 1), thumb-start is
/// `position * (track - thumb) / max_position`. That maps the
/// extremes exactly: scroll-to-top puts the thumb's top at row 0,
/// scroll-to-bottom puts the thumb's bottom at the last track row.
/// `ratatui::widgets::Scrollbar`'s default math doesn't have this
/// property - it normalizes against `content_length - 1 + viewport`
/// instead of `content_length`, so the thumb's bottom never reaches
/// the track's bottom for a list at its scroll-end.
pub fn render_vertical_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    content_length: usize,
    position: usize,
    viewport: usize,
) {
    if content_length <= viewport || area.height == 0 || area.width == 0 {
        return;
    }
    let track_length = area.height as usize;
    // Thumb fills the same fraction of the track that the viewport
    // fills of the content, with a 1-cell minimum so a tiny window
    // over a huge list still has a visible thumb. Max = track-1 so
    // there's always at least one track cell visible above OR below
    // the thumb (lets the user tell the bar isn't a fixed column).
    let thumb_length = ((viewport * track_length) / content_length)
        .max(1)
        .min(track_length.saturating_sub(1).max(1));
    let max_position = content_length - viewport;
    let track_remaining = track_length - thumb_length;
    let position = position.min(max_position);
    // Round-to-nearest division: `(p * remaining + max/2) / max`
    // anchors the thumb to track[0] at position 0 and track[remaining]
    // (i.e. the bottom edge once thumb_length is added) at position
    // max_position. `checked_div` returns `None` when the list fits in
    // the viewport - guarded above, but expressed here so clippy's
    // `manual_checked_ops` lint stays happy.
    let thumb_start = (position * track_remaining + max_position / 2)
        .checked_div(max_position)
        .unwrap_or(0);
    let bar_x = area.x + area.width - 1;
    let buf = frame.buffer_mut();
    let track_glyph = symbols::line::DOUBLE_VERTICAL;
    let thumb_glyph = symbols::block::FULL;
    for offset in 0..track_length {
        let glyph = if offset >= thumb_start && offset < thumb_start + thumb_length {
            thumb_glyph
        } else {
            track_glyph
        };
        buf.set_string(bar_x, area.y + offset as u16, glyph, Style::new());
    }
}

/// Size a `[message] [Dismiss]` / `[Cancel] [Confirm]` overlay popup
/// to its contents.
///
/// Layout of the bordered popup (matching the call sites in
/// `render_notification_overlay` / `render_confirm_overlay`):
///
/// ```text
/// ┌ Title ───────────────┐
/// │ message...           │
/// │ ...(possibly wrapped)│
/// │                      │
/// │     [ Dismiss ]      │
/// └──────────────────────┘
/// ```
///
/// Width grows with the longest input line (up to `MAX_OVERLAY_W`) so
/// short toasts stay compact and long ones wrap rather than spanning
/// the whole frame. Height is then derived from the post-wrap row
/// count via `Paragraph::line_count` at the chosen inner width.
///
/// `MIN_OVERLAY_W` keeps the title and Dismiss/Confirm buttons from
/// looking cramped on tiny messages; `MAX_OVERLAY_W` is a soft cap that
/// also yields to a narrow `frame_area`.
fn sized_overlay_rect(frame_area: Rect, message: &str, message_para: &Paragraph<'_>) -> Rect {
    /// Border characters consume one cell on each side.
    const CHROME_W: u16 = 2;
    /// Minimum popup width - keeps the title and the centered button row
    /// from looking cramped even when the message is short.
    const MIN_OVERLAY_W: u16 = 30;
    /// Soft cap on popup width so long messages wrap into multiple
    /// rows instead of stretching across the whole frame.
    const MAX_OVERLAY_W: u16 = 60;
    /// Top/bottom borders + blank spacer + single button row = 4 rows
    /// of chrome around the (>=1-row) message body.
    const VERT_CHROME: u16 = 4;

    let max_line_w = message
        .lines()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0);
    let desired_w = u16::try_from(max_line_w)
        .unwrap_or(u16::MAX)
        .saturating_add(CHROME_W);
    // Cap to MAX_OVERLAY_W but never exceed the frame; ensure at least
    // MIN_OVERLAY_W where the frame is wide enough to allow it.
    let cap_w = MAX_OVERLAY_W.min(frame_area.width);
    let floor_w = MIN_OVERLAY_W.min(frame_area.width);
    let popup_w = desired_w.clamp(floor_w, cap_w);
    let inner_w = popup_w.saturating_sub(CHROME_W);

    let message_h = u16::try_from(message_para.line_count(inner_w))
        .unwrap_or(u16::MAX)
        .max(1);
    let popup_h = message_h.saturating_add(VERT_CHROME).min(frame_area.height);

    centered_rect_abs(popup_w, popup_h, frame_area)
}

/// Centered sub-rectangle at absolute width/height, clamped to `area`'s
/// extent on each axis. Counterpart to [`centered_rect`] which works
/// in percentages - overlays use absolute cell dimensions so the
/// percentage form would drift with terminal size.
fn centered_rect_abs(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x_pad = (area.width - width) / 2;
    let y_pad = (area.height - height) / 2;
    Rect::new(area.x + x_pad, area.y + y_pad, width, height)
}

/// Dim every cell in the frame's buffer that falls outside
/// `popup_area`, producing the dimmed-backdrop look used by overlays.
///
/// Call AFTER the underlying page renders and BEFORE the overlay box
/// renders, so the overlay's own cells stay vivid. Cells inside
/// `popup_area` are left untouched - the overlay box typically follows
/// up with a `Clear` over its rect to wipe the page content under the
/// popup before drawing its own borders/contents.
pub fn dim_frame_below(frame: &mut Frame<'_>, popup_area: Rect) {
    dim_buffer_outside(frame.buffer_mut(), popup_area);
}

/// Buffer-only counterpart to [`dim_frame_below`]; broken out so unit
/// tests can drive it without spinning up a full `Frame` /
/// `TestBackend`.
fn dim_buffer_outside(buf: &mut ratatui::buffer::Buffer, popup_area: Rect) {
    let buf_area = buf.area;
    let popup_right = popup_area.x.saturating_add(popup_area.width);
    let popup_bottom = popup_area.y.saturating_add(popup_area.height);
    for y in buf_area.y..buf_area.y.saturating_add(buf_area.height) {
        for x in buf_area.x..buf_area.x.saturating_add(buf_area.width) {
            // Inside the popup? Leave alone.
            if x >= popup_area.x && x < popup_right && y >= popup_area.y && y < popup_bottom {
                continue;
            }
            let cell = &mut buf[(x, y)];
            // `Style::reset()` patched onto the cell clears modifiers
            // and resets bg; `.fg(DarkGray)` then tints the symbol so
            // the page stays legible-but-clearly-backgrounded.
            cell.set_style(Style::reset().dark_gray());
        }
    }
}

fn centered_rect(horizontal_percent: u16, vertical_percent: u16, area: Rect) -> Rect {
    let [_, vertical_mid, _] = Layout::vertical([
        Constraint::Percentage((100 - vertical_percent) / 2),
        Constraint::Percentage(vertical_percent),
        Constraint::Percentage((100 - vertical_percent) / 2),
    ])
    .areas(area);

    let [_, horizontal_mid, _] = Layout::horizontal([
        Constraint::Percentage((100 - horizontal_percent) / 2),
        Constraint::Percentage(horizontal_percent),
        Constraint::Percentage((100 - horizontal_percent) / 2),
    ])
    .areas(vertical_mid);
    horizontal_mid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{FocusRegistry, PageFocus};
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    #[test]
    fn split_layout_paints_window_close_button_just_inside_top_right_corner() {
        // Top border row should read like
        // `┌──────title───[x]┐` - the close button paints its 3
        // cells immediately before the right corner glyph, and is
        // registered for click dispatch.
        //
        // The frame is clamped+centered to `MAX_APP_WIDTH x HEIGHT`,
        // so the right corner glyph isn't at the terminal's right
        // edge. Locate it on the top row and then walk left to
        // confirm the `[x]` button.
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = FocusRegistry::new();
        let page_focus = PageFocus::default();

        let buf = terminal
            .draw(|f| {
                let _ = split_layout(
                    f,
                    f.area(),
                    false,
                    "mullvad-tui",
                    &page_focus,
                    &mut registry,
                );
            })
            .unwrap();

        let corner_x = (0..buf.area.width)
            .find(|&x| buf.buffer[(x, 0)].symbol() == "┐")
            .expect("top-right corner should render");
        assert!(corner_x >= 3, "need 3 cells of room for `[x]` to the left");
        assert_eq!(buf.buffer[(corner_x - 1, 0)].symbol(), "]");
        assert_eq!(buf.buffer[(corner_x - 2, 0)].symbol(), "x");
        assert_eq!(buf.buffer[(corner_x - 3, 0)].symbol(), "[");

        assert!(
            registry.contains(WINDOW_CLOSE),
            "[x] must be registered as a focusable widget for click dispatch",
        );
    }

    #[test]
    fn render_tab_bar_layout_accounts_for_padding() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut registry = FocusRegistry::new();
        let area = Rect::new(0, 0, 80, 1);
        let current_page = PageId::Status;
        let page_focus = PageFocus::default();

        terminal
            .draw(|f| {
                render_tab_bar(f, area, current_page, &page_focus, &mut registry);
            })
            .unwrap();

        // Check that all 4 top-level tabs are registered.
        let widgets = registry.all_widgets();
        assert_eq!(widgets.len(), 4, "Should have 4 tab buttons registered");

        // The total width should be:
        // [Status]   : 8  + 2 = 10
        // [Account]  : 9  + 2 = 11
        // [Settings] : 10 + 2 = 12
        // [Logs]     : 6  + 2 = 8
        // Dividers   : 3 * 1 = 3
        // Total      : 10 + 11 + 12 + 8 + 3 = 44
        let expected_total_w = 44;

        // bar_area should be centered in 80 cells.
        let expected_x = (80 - expected_total_w) / 2;

        // Hit areas cover only the `[Label]` glyphs - the 1 cell of
        // pad on each side is excluded so hovering empty space between
        // brackets and the divider does not register as a tab.
        for (i, widget) in widgets.iter().enumerate() {
            assert_eq!(widget.kind, FocusKind::TabButton);
            assert_eq!(widget.rect.y, 0);
            assert_eq!(widget.rect.height, 1);
            if i == 0 {
                assert_eq!(widget.rect.x, expected_x + 1);
                assert_eq!(widget.rect.width, 8);
            } else if i == 3 {
                // The last one (Logs) sits at expected_x + 10+1 + 11+1
                // + 12+1 = expected_x + 36, then +1 for left pad.
                assert_eq!(widget.rect.x, expected_x + 36 + 1);
                assert_eq!(widget.rect.width, 6);
            }
        }
    }

    #[test]
    fn small_terminal_floor_at_46_by_30() {
        assert_eq!(MIN_TERMINAL_WIDTH, 46);
        assert_eq!(MIN_TERMINAL_HEIGHT, 30);
        // At the floor: not "small".
        assert!(!is_small_terminal(Rect::new(0, 0, 46, 30)));
        // One short on either axis: small.
        assert!(is_small_terminal(Rect::new(0, 0, 45, 30)));
        assert!(is_small_terminal(Rect::new(0, 0, 46, 29)));
        // Generous terminal: not small.
        assert!(!is_small_terminal(Rect::new(0, 0, 200, 60)));
    }

    #[test]
    fn app_area_caps_at_50_by_36() {
        assert_eq!(MAX_APP_WIDTH, 50);
        assert_eq!(MAX_APP_HEIGHT, 36);
        let clamped = clamp_to_app_area(Rect::new(0, 0, 200, 60));
        assert_eq!(clamped.width, MAX_APP_WIDTH);
        assert_eq!(clamped.height, MAX_APP_HEIGHT);
        // Centered inside the surplus.
        assert_eq!(clamped.x, (200 - MAX_APP_WIDTH) / 2);
        assert_eq!(clamped.y, (60 - MAX_APP_HEIGHT) / 2);
    }

    // ---- Glyphs ----

    #[test]
    fn checkbox_glyph_renders() {
        assert_eq!(checkbox_glyph(true), "[x]");
        assert_eq!(checkbox_glyph(false), "[ ]");
    }

    #[test]
    fn radio_glyph_is_three_cells_wide() {
        assert_eq!(radio_glyph(true), "(•)");
        assert_eq!(radio_glyph(false), "( )");
        // The glyph is exactly 3 columns wide (1 paren + 1 bullet + 1
        // paren). Critical because the toggle-row layout reserves
        // glyph.chars().count() cells on the right.
        assert_eq!(radio_glyph(true).chars().count(), 3);
    }

    #[test]
    fn chevron_glyphs_are_single_cell() {
        assert_eq!(chevron(Chevron::Down), "▼");
        assert_eq!(chevron(Chevron::Up), "▲");
    }

    #[test]
    fn spinner_cycles_through_ten_frames() {
        // Two full cycles, plus a partial - each frame is one cell wide
        // and the cycle length is 10.
        let observed: Vec<&str> = (0..25).map(spinner_frame).collect();
        // Frame 0 == frame 10 == frame 20.
        assert_eq!(observed[0], observed[10]);
        assert_eq!(observed[0], observed[20]);
        // 10 distinct frames over a single cycle.
        let unique: std::collections::HashSet<&&str> = observed[..10].iter().collect();
        assert_eq!(unique.len(), 10, "spinner has 10 distinct frames");
        // Each frame is exactly 1 column wide (Braille glyphs are
        // single-cell on neutral East-Asian Width).
        for frame in &observed {
            assert_eq!(frame.chars().count(), 1);
        }
    }

    // ---- Dim + absolute centering ----

    #[test]
    fn centered_rect_abs_centres_inside_surplus() {
        let area = Rect::new(0, 0, 100, 40);
        let r = centered_rect_abs(36, 7, area);
        assert_eq!(r.width, 36);
        assert_eq!(r.height, 7);
        assert_eq!(r.x, (100 - 36) / 2);
        assert_eq!(r.y, (40 - 7) / 2);
    }

    #[test]
    fn centered_rect_abs_clamps_to_area() {
        let area = Rect::new(0, 0, 20, 10);
        let r = centered_rect_abs(36, 7, area);
        // Width clamps; height fits.
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 7);
    }

    #[test]
    fn render_button_running_prefixes_spinner_when_running() {
        use ratatui::{Terminal, backend::TestBackend};

        let backend = TestBackend::new(20, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let id = WidgetId(0x101);

        // running=true -> label should contain a spinner glyph from
        // the standard 10-frame Braille set.
        let mut registry = FocusRegistry::default();
        let buf = terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 20, 1);
                render_button_running(
                    frame,
                    area,
                    "Connect",
                    Style::new(),
                    true,
                    0,
                    &mut registry,
                    id,
                );
            })
            .unwrap();
        let line: String = (0..buf.area.width)
            .map(|x| buf.buffer[(x, 0)].symbol())
            .collect();
        assert!(
            line.contains(spinner_frame(0)),
            "running button should include a spinner glyph: {line:?}",
        );
        assert!(line.contains("Connect"));
        assert!(registry.contains(id));
    }

    #[test]
    fn render_button_running_omits_spinner_when_idle() {
        use ratatui::{Terminal, backend::TestBackend};

        let backend = TestBackend::new(20, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let id = WidgetId(0x102);
        let mut registry = FocusRegistry::default();
        let buf = terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 20, 1);
                render_button_running(
                    frame,
                    area,
                    "Connect",
                    Style::new(),
                    false,
                    0,
                    &mut registry,
                    id,
                );
            })
            .unwrap();
        let line: String = (0..buf.area.width)
            .map(|x| buf.buffer[(x, 0)].symbol())
            .collect();
        assert!(
            !line.contains(spinner_frame(0)),
            "idle button should not include a spinner glyph: {line:?}",
        );
        assert!(line.contains("[Connect]"));
    }

    #[test]
    fn dim_buffer_outside_skips_popup_cells() {
        use ratatui::buffer::Buffer;

        // 20x10 buffer; popup at (5, 2) size 8x4. Fill every cell
        // with a marker symbol + bold modifier so we can detect when
        // dim has touched it.
        let area = Rect::new(0, 0, 20, 10);
        let mut buf = Buffer::empty(area);
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &mut buf[(x, y)];
                cell.set_symbol("X");
                cell.set_style(Style::new().white().bold());
            }
        }

        let popup = Rect::new(5, 2, 8, 4);
        dim_buffer_outside(&mut buf, popup);

        // Cells inside the popup retained their original style.
        let inside = &buf[(popup.x, popup.y)];
        assert_eq!(inside.symbol(), "X");
        assert_eq!(inside.fg, Color::White);
        assert!(
            inside.modifier.contains(ratatui::style::Modifier::BOLD),
            "popup cells should keep their original modifiers",
        );

        // A cell outside the popup is dimmed: fg=DarkGray, modifiers
        // cleared. Symbol is preserved (we only restyle, not erase).
        let outside = &buf[(0, 0)];
        assert_eq!(outside.symbol(), "X");
        assert_eq!(outside.fg, Color::DarkGray);
        assert!(
            !outside.modifier.contains(ratatui::style::Modifier::BOLD),
            "dim should clear modifiers on cells outside the popup",
        );

        // Just outside the popup edge - also dimmed.
        let edge = &buf[(popup.x + popup.width, popup.y)];
        assert_eq!(edge.fg, Color::DarkGray);
    }

    /// Drive [`render_vertical_scrollbar`] and return a Vec<bool> per
    /// track row: `true` = thumb cell, `false` = track cell. The
    /// scrollbar lives in the rightmost column of `area`.
    fn scrollbar_thumb_mask(
        area: Rect,
        content_length: usize,
        position: usize,
        viewport: usize,
    ) -> Vec<bool> {
        use ratatui::{Terminal, backend::TestBackend};

        let backend = TestBackend::new(area.x + area.width, area.y + area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        let buf = terminal
            .draw(|frame| {
                render_vertical_scrollbar(frame, area, content_length, position, viewport);
            })
            .unwrap();
        let bar_x = area.x + area.width - 1;
        (0..area.height)
            .map(|i| buf.buffer[(bar_x, area.y + i)].symbol() == symbols::block::FULL)
            .collect()
    }

    #[test]
    fn scrollbar_thumb_at_top_when_at_top() {
        // Content 50, viewport 28, scrolled to top. The thumb's
        // first cell must be row 0 of the track.
        let area = Rect::new(0, 0, 1, 28);
        let mask = scrollbar_thumb_mask(area, 50, 0, 28);
        assert!(mask[0], "thumb should start at the top, mask: {mask:?}");
    }

    #[test]
    fn scrollbar_thumb_at_bottom_when_at_bottom() {
        // Content 50, viewport 28, scrolled all the way down (top of
        // window at item 22). The thumb's last cell must be the
        // last row of the track. This is the bug the user reported
        // against ratatui's built-in Scrollbar - it puts the thumb's
        // bottom only at row ~18 of a 28-row track.
        let area = Rect::new(0, 0, 1, 28);
        let max_position = 50 - 28;
        let mask = scrollbar_thumb_mask(area, 50, max_position, 28);
        assert!(
            *mask.last().unwrap(),
            "thumb should reach the last track row when scrolled to the bottom, mask: {mask:?}",
        );
    }

    #[test]
    fn scrollbar_thumb_size_proportional_to_viewport_over_content() {
        // viewport / content = 28/50 = 56%, track 28 -> thumb ~ 15.
        let area = Rect::new(0, 0, 1, 28);
        let mask = scrollbar_thumb_mask(area, 50, 0, 28);
        let thumb_cells = mask.iter().filter(|&&b| b).count();
        assert_eq!(
            thumb_cells, 15,
            "expected ~28*28/50 cells, got {thumb_cells}"
        );
    }

    #[test]
    fn scrollbar_no_render_when_content_fits() {
        // content == viewport -> no overflow, no bar at all.
        let area = Rect::new(0, 0, 1, 10);
        let mask = scrollbar_thumb_mask(area, 10, 0, 10);
        assert!(
            mask.iter().all(|&b| !b),
            "no thumb should render when content fits, mask: {mask:?}",
        );
    }
}
