//! forqen — a native GitHub client for Linux.

mod cli;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;

/// Cached responses older than this are dropped at startup.
///
/// Pruning at startup rather than on a timer: an unbounded cache is a disk
/// leak, but evicting mid-session throws away exactly the pages the user is
/// moving between.
const CACHE_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

fn main() -> std::process::ExitCode {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    // A subcommand runs without ever opening a display, so this happens before
    // any GTK setup — sign-in over SSH is one of the cases it exists for.
    if let Some(command) = cli::Command::parse(&argv) {
        init_logging();
        return command.run();
    }

    gui()
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "forqen=info,warn".into()),
        )
        .init();
}

fn gui() -> std::process::ExitCode {
    init_logging();
    warn_if_allocator_unbounded();

    // One runtime for the whole process, shared by every network call. Two
    // worker threads: the workload is entirely IO-bound HTTP, so more threads
    // would add glibc malloc arenas without adding throughput.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "could not start the async runtime");
            return std::process::ExitCode::FAILURE;
        }
    };
    let handle = runtime.handle().clone();

    match db::Db::open_default() {
        Ok(store) => {
            match store.prune(CACHE_MAX_AGE) {
                Ok(n) if n > 0 => tracing::info!(pruned = n, "dropped stale cache entries"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "cache prune failed"),
            }
            // Held so the UI can reach it once account switching lands in R2.
            let _store = Arc::new(store);
        }
        // A missing or unwritable cache degrades performance, not correctness:
        // every request simply goes to the network. Not a reason to refuse to start.
        Err(e) => tracing::warn!(error = %e, "running without a local cache"),
    }

    let initial_repo = match repo_from_args() {
        Ok(p) => p,
        Err(message) => {
            eprintln!("forqen: {message}");
            eprintln!("usage: forqen [PATH]");
            return std::process::ExitCode::FAILURE;
        }
    };

    let app = adw::Application::builder()
        .application_id(ui::APP_ID)
        .build();

    app.connect_activate(move |app| {
        let window = ui::build_window(app, handle.clone(), initial_repo.as_deref());
        window.present();
    });

    // GTK parses argv itself and would treat our path argument as an unknown
    // option, so arguments are handled above and GTK is given none.
    let code = app.run_with_args::<&str>(&[]);
    if code == glib::ExitCode::SUCCESS {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// Optional repository path from the command line.
///
/// Hand-parsed rather than pulled in via clap: one optional positional argument
/// does not justify a dependency, and `--help` for a GUI app is three lines.
fn repo_from_args() -> Result<Option<PathBuf>, String> {
    let mut args = std::env::args_os().skip(1);
    let Some(first) = args.next() else {
        return Ok(None);
    };

    match first.to_str() {
        Some("-h") | Some("--help") => {
            println!("forqen — native GitHub client\n");
            println!("usage: forqen [PATH]");
            println!("       forqen login   [--host HOST]   adopt the gh CLI's token");
            println!("       forqen logout  <login> [--host HOST]");
            println!("       forqen accounts\n");
            println!("  PATH   repository to open; defaults to the most recent one");
            std::process::exit(0);
        }
        Some("--version") | Some("-V") => {
            println!("forqen {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        _ => {}
    }

    if args.next().is_some() {
        return Err("expected at most one path".into());
    }

    let path = PathBuf::from(first);
    if !path.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    Ok(Some(path))
}

/// glibc allocates a fresh 64MB arena per thread on 64-bit, so a threaded
/// process can show alarming RSS from allocator fragmentation that has nothing
/// to do with the data it holds.
///
/// `MALLOC_ARENA_MAX` is read at the first `malloc`, which happens long before
/// `main` — setting it here would do nothing. It is set instead in the
/// `.desktop` `Exec=` line and in the Flatpak manifest, and this check exists
/// so a launch that bypassed both is visible in the log rather than silently
/// costing 100MB.
fn warn_if_allocator_unbounded() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        tracing::warn!(
            "MALLOC_ARENA_MAX is unset; RSS may be inflated by per-thread \
             glibc arenas. Launch via the .desktop entry or Flatpak, or run \
             `MALLOC_ARENA_MAX=2 forqen`."
        );
    }
}

// Re-exported by the adw prelude, but naming it keeps `main`'s signature clear.
use gtk::glib;
