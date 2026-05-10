// SPDX-License-Identifier: GPL-3.0-or-later

//! In-app log capture for the TUI's Logs panel.
//!
//! Installs a global [`tracing_subscriber`] with two layers stacked under an
//! `RUST_LOG`-respecting [`EnvFilter`]:
//! - the standard env filter (default `info`), with the workspace's [`silence_crates`] applied so
//!   noisy upstream deps don't drown the panel;
//! - a [`RingBufferLayer`] that captures every event surviving the filter and forwards it through
//!   an `mpsc::Sender<LogEntry>` to the TUI run loop.
//!
//! The receiver is returned to the caller; [`crate::tui::run`] drains it into
//! [`crate::app::App::append_log_entry`] which keeps a bounded ring buffer.
//! Send is non-blocking ([`mpsc::Sender::try_send`]) so a slow / saturated
//! consumer drops entries rather than stalling the emitter - the TUI log is
//! a debugging convenience, not a reliable record.

use std::fmt;

use chrono::{DateTime, Local};
use mullvad_logging::{EnvFilter, LevelFilter, silence_crates};
use tokio::sync::mpsc;
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{
    Layer,
    layer::{Context, SubscriberExt},
    util::SubscriberInitExt,
};

/// One captured log line, sized for the panel renderer's needs.
///
/// Kept owned (no borrows) so it can travel through an `mpsc` channel and
/// then sit in the App's ring buffer until evicted. The `source` enum
/// distinguishes structured TUI tracing events from the daemon's
/// pre-formatted log lines, which the renderer styles differently.
#[derive(Clone, Debug)]
pub struct LogEntry {
    /// Local arrival time. For TUI entries, the timestamp the event
    /// fired; for daemon entries, the time we received the line off
    /// the gRPC stream (the daemon's own timestamp is embedded in the
    /// pre-formatted line).
    pub timestamp: DateTime<Local>,
    pub source: LogSource,
}

/// Which subsystem produced this log entry - the TUI's own
/// `tracing::*` macros or the daemon's `log_listen` stream.
#[derive(Clone, Debug)]
pub enum LogSource {
    /// Structured event from the TUI's own `tracing` subscriber. The
    /// renderer maps `level` to color.
    Tui {
        level: Level,
        target: String,
        message: String,
    },
    /// Pre-formatted line from the daemon's `log_listen` stream. The
    /// daemon embeds its own timestamp + level + target inside the
    /// string, so we don't try to parse them out - just render the
    /// line in a distinct color so the user can tell daemon and TUI
    /// entries apart even when intermixed.
    Daemon { line: String },
}

impl fmt::Display for LogEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            LogSource::Tui {
                level,
                target,
                message,
            } => write!(
                f,
                "{} {:<5} {}: {}",
                self.timestamp.format("%H:%M:%S%.3f"),
                level,
                target,
                message,
            ),
            LogSource::Daemon { line } => {
                // Daemon lines are already self-formatted (timestamp
                // + level + target + message); strip any trailing
                // newline so the renderer's per-line layout doesn't
                // get a blank row beneath each daemon entry.
                write!(f, "[daemon] {}", line.trim_end_matches('\n'))
            }
        }
    }
}

/// Tracing layer that captures every event surviving the outer EnvFilter and
/// forwards it as a [`LogEntry`] through `tx`.
struct RingBufferLayer {
    tx: mpsc::Sender<LogEntry>,
}

impl<S: Subscriber> Layer<S> for RingBufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let entry = LogEntry {
            timestamp: Local::now(),
            source: LogSource::Tui {
                level: *event.metadata().level(),
                target: event.metadata().target().to_string(),
                message: visitor.message,
            },
        };
        // Non-blocking: a saturated channel drops the entry. The emitter must
        // never stall; tracing events fire from arbitrary tasks/threads.
        let _ = self.tx.try_send(entry);
    }
}

/// `tracing` `Visit` impl that extracts only the canonical `message` field
/// (what `tracing::info!("hello {x}")` ends up writing). Structured fields
/// are ignored; the panel only renders the rendered message string.
#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.message, "{value:?}");
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        }
    }
}

/// Install the global subscriber and return both ends of the log
/// channel. Must be called once at process start, before any `tracing!`
/// macros fire (early events would otherwise be discarded by the no-op
/// default subscriber).
///
/// The `Sender` is cloneable - the run loop hands a clone to the
/// daemon-log-listener task so the daemon's `log_listen` stream
/// intermixes its lines into the same ring buffer the TUI's tracing
/// goes to. The `Receiver` is the run loop's drain into
/// [`crate::app::App::append_log_entry`].
///
/// `level_override` (typically wired to `--log-level`) wins over the
/// `RUST_LOG` env var; if neither is set the default is `info`. Invalid
/// directives fall back to `info` rather than panicking - the TUI staying
/// up beats strict CLI validation here.
pub fn init(level_override: Option<&str>) -> (mpsc::Sender<LogEntry>, mpsc::Receiver<LogEntry>) {
    // 1024 entries of slack: at 50 lines/sec (very chatty) the TUI loop has
    // ~20 seconds to drain before backpressure starts dropping entries.
    let (tx, rx) = mpsc::channel(1024);
    let env_filter = match level_override {
        Some(directive) => EnvFilter::try_new(directive)
            .unwrap_or_else(|_| EnvFilter::new(LevelFilter::INFO.to_string())),
        None => EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(LevelFilter::INFO.to_string())),
    };
    let env_filter = silence_crates(env_filter);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(RingBufferLayer { tx: tx.clone() })
        .init();

    (tx, rx)
}
