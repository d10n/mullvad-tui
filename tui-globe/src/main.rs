// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    f32::consts::{FRAC_PI_2, TAU},
    io,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    widgets::{Block, Borders},
};
use tui_globe::{Camera, Globe, MapData};

const TARGET_FPS: u64 = 30;
/// One full revolution every ~25 seconds.
const RADIANS_PER_SECOND: f32 = TAU / 25.0;

/// Radians of yaw/pitch added per cell of mouse drag. Tuned so a full-width
/// drag across an 80-cell terminal sweeps roughly a quarter turn.
const DRAG_SENSITIVITY: f32 = 0.02;
/// Multiplicative zoom step per scroll-wheel notch.
const ZOOM_STEP: f32 = 1.15;
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 16.0;
/// Don't let pitch reach the poles - past +-pi/2 the rotation becomes ambiguous
/// and the globe flips. Stop just shy of straight up/down.
const PITCH_LIMIT: f32 = FRAC_PI_2 * 0.95;

#[derive(Debug)]
struct ViewState {
    /// Yaw offset accumulated by the user; auto-spin is added on top.
    user_yaw: f32,
    /// Pitch is fully user-controlled (auto-spin doesn't touch it).
    pitch: f32,
    /// Zoom multiplier; > 1 magnifies.
    zoom: f32,
    /// Last cursor position seen during an active left-button drag.
    drag_anchor: Option<(u16, u16)>,
    /// Accumulated auto-spin yaw. Advances by `dt * RADIANS_PER_SECOND` each
    /// frame, but only when no drag is in progress - so holding the mouse
    /// down freezes the demo spin and the user keeps full manual control.
    spin_yaw: f32,
    /// Wall-clock time of the previous frame's spin update.
    last_tick: Instant,
}

impl ViewState {
    fn new(now: Instant) -> Self {
        Self {
            user_yaw: 0.0,
            pitch: 0.0,
            zoom: 1.0,
            drag_anchor: None,
            spin_yaw: 0.0,
            last_tick: now,
        }
    }
}

fn main() -> Result<()> {
    let map = MapData::embedded();
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = run(&mut term, &map);
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    term.show_cursor()?;
    result
}

fn run(term: &mut Terminal<CrosstermBackend<io::Stdout>>, map: &MapData) -> Result<()> {
    let frame_dt = Duration::from_millis(1000 / TARGET_FPS);
    let mut state = ViewState::new(Instant::now());
    loop {
        let now = Instant::now();
        let dt = now.duration_since(state.last_tick).as_secs_f32();
        state.last_tick = now;
        if state.drag_anchor.is_none() {
            state.spin_yaw = (state.spin_yaw + dt * RADIANS_PER_SECOND).rem_euclid(TAU);
        }
        let camera = Camera {
            yaw: state.spin_yaw + state.user_yaw,
            pitch: state.pitch,
            zoom: state.zoom,
        };
        term.draw(|f| {
            let area = f.area();
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" tui-globe - drag rotate · scroll zoom · q quit ");
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(Globe::new(map, camera), inner);
        })?;

        if event::poll(frame_dt)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    _ => {}
                },
                Event::Mouse(m) => handle_mouse(&mut state, m),
                _ => {}
            }
        }
    }
}

fn handle_mouse(state: &mut ViewState, m: MouseEvent) {
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            state.drag_anchor = Some((m.column, m.row));
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some((px, py)) = state.drag_anchor {
                let dx = i32::from(m.column) - i32::from(px);
                let dy = i32::from(m.row) - i32::from(py);
                state.user_yaw += dx as f32 * DRAG_SENSITIVITY;
                // Drag down -> globe tilts so the top hemisphere rotates toward
                // the camera (positive pitch); see the lib::rotate convention.
                state.pitch =
                    (state.pitch + dy as f32 * DRAG_SENSITIVITY).clamp(-PITCH_LIMIT, PITCH_LIMIT);
                state.drag_anchor = Some((m.column, m.row));
            }
        }
        MouseEventKind::Up(_) => state.drag_anchor = None,
        MouseEventKind::ScrollUp => state.zoom = (state.zoom * ZOOM_STEP).min(ZOOM_MAX),
        MouseEventKind::ScrollDown => state.zoom = (state.zoom / ZOOM_STEP).max(ZOOM_MIN),
        _ => {}
    }
}
