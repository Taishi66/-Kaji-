#![recursion_limit = "256"]

use anyhow::Result;
use kaji_cli::cli::cli;

/// Enable ANSI/VT escape sequence processing on Windows Console Host.
///
/// Without this, spinners and progress bars from cliclack/indicatif render as
/// repeated new lines instead of updating in place, because Windows Console Host
/// does not process ANSI escapes by default.
#[cfg(windows)]
fn enable_windows_vt_processing() {
    // colors_supported() has the side effect of calling SetConsoleMode with
    // ENABLE_VIRTUAL_TERMINAL_PROCESSING on the underlying console handle.
    let _ = console::Term::stdout().features().colors_supported();
    let _ = console::Term::stderr().features().colors_supported();
}

/// Apply `--profile <name>` from the raw argv before config is first touched.
///
/// `Config::global()` can be initialized as early as `setup_logging` (otel/layers
/// read it), which runs before clap parses. Scanning argv here guarantees the
/// profile is visible to `Config::default()` regardless of the code path.
fn apply_profile_from_argv() {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                if let Some(value) = args.get(i + 1) {
                    std::env::set_var("KAJI_PROFILE", value);
                    return;
                }
            }
            flag if flag.starts_with("--profile=") => {
                if let Some(value) = flag.strip_prefix("--profile=") {
                    if !value.is_empty() {
                        std::env::set_var("KAJI_PROFILE", value);
                        return;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
}

async fn run() -> Result<()> {
    apply_profile_from_argv();

    // Les hooks déclarés en config n'existent que dans ce binaire : une suite
    // de tests qui construit un `Agent` ne doit jamais exécuter les hooks de la
    // machine qui la lance (`kaji::hooks::enable_config_hooks`).
    kaji::hooks::enable_config_hooks();

    if let Err(e) = kaji_cli::logging::setup_logging(None) {
        eprintln!("Warning: Failed to initialize logging: {}", e);
    }

    let result = cli().await;

    #[cfg(feature = "otel")]
    if kaji::otel::otlp::is_otlp_initialized() {
        kaji::otel::otlp::shutdown_otlp();
    }

    result
}

fn main() -> Result<()> {
    #[cfg(windows)]
    enable_windows_vt_processing();

    let handle = std::thread::Builder::new()
        .name("kaji-cli-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to build Tokio runtime");
            runtime.block_on(run())
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn kaji-cli main thread: {}", e))?;

    handle
        .join()
        .map_err(|_| anyhow::anyhow!("kaji-cli main thread panicked"))?
}
