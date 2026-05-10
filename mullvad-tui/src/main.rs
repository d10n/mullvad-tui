// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
mod integration;
mod logging;
#[cfg(test)]
mod test_support;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "mullvad-tui",
    version = mullvad_version::VERSION,
    about = "Terminal user interface for the Mullvad VPN daemon",
    long_about = None,
)]
struct Cli {
    /// Override the daemon's gRPC socket path. Defaults to the value of
    /// MULLVAD_RPC_SOCKET_PATH or the platform default. Useful for talking
    /// to a non-default daemon (e.g. a development build).
    #[arg(long, value_name = "PATH")]
    rpc_socket_path: Option<PathBuf>,

    /// Log filter, same syntax as RUST_LOG (e.g. `info` or
    /// `info,mullvad_tui=debug`). Wins over RUST_LOG; falls back to RUST_LOG,
    /// then `info`.
    #[arg(long, value_name = "FILTER")]
    log_level: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(path) = cli.rpc_socket_path.as_deref() {
        // SAFETY: `main` is single-threaded at this point - tokio's runtime
        // hasn't been entered yet (the `#[tokio::main]` macro expands to a
        // `Builder::new_multi_thread()...block_on(async { ... })` wrapper, and
        // the body of that future is what we're inside). No other thread can
        // observe a torn read of the env var before MullvadProxyClient::new()
        // (called inside `tui::run`) reads it via `mullvad_paths::get_rpc_socket_path()`.
        unsafe {
            std::env::set_var("MULLVAD_RPC_SOCKET_PATH", path);
        }
    }

    // Install the global tracing subscriber first so any early `tracing!`
    // calls are captured into the in-app Logs panel from the very start.
    // The Sender is cloned by the run loop for the daemon-log forwarder
    // so daemon and TUI logs intermix in the same ring buffer.
    let (log_tx, log_rx) = logging::init(cli.log_level.as_deref());
    let mut app = app::App::new();
    tui::run(&mut app, log_tx, log_rx).await
}
