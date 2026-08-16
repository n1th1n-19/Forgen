//! Fetch, pull and push from the UI, with live progress.
//!
//! Threading: a transfer is a blocking child process, so it runs on a plain
//! `std::thread` rather than on the tokio runtime — a subprocess that streams
//! stderr for thirty seconds would occupy a runtime worker to no purpose.
//! Progress crosses back over an `async_channel` and is applied on the glib
//! main context, which is the only place widgets may be touched.
//!
//! The repository is re-opened inside the worker rather than shared. `Repo`
//! holds a `gix::Repository`, which is not `Sync`, and sending one across a
//! thread boundary would be both unsound and unnecessary — opening is cheap
//! next to a network round trip.

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use git::remote::{self, Progress, PushMode};
use git::Repo;

/// Which transfer to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Fetch,
    Pull,
    Push,
    PushForceWithLease,
}

impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fetch => "Fetching",
            Self::Pull => "Pulling",
            Self::Push | Self::PushForceWithLease => "Pushing",
        }
    }
}

enum Update {
    Progress(Progress),
    Done(Result<(), String>),
}

/// Run `op` in the background, reporting progress and calling `on_done`.
///
/// `on_done` runs on the main context, so it may touch widgets directly.
pub fn run(
    window: &adw::ApplicationWindow,
    repo_path: PathBuf,
    branch: String,
    op: Operation,
    on_done: Rc<dyn Fn(Result<(), String>)>,
) {
    let (tx, rx) = async_channel::bounded::<Update>(64);

    // The token is read here, on the main thread, before the worker starts:
    // the keyring call can block on a D-Bus prompt, and doing it inside the
    // transfer would stall a thread that is meant to be showing progress.
    let token = default_token();

    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let repo = Repo::open(&repo_path).map_err(|e| e.to_string())?;
            let mut sink = |p: Progress| {
                // A full channel means the UI is behind; dropping an
                // intermediate progress frame is correct — the next one
                // supersedes it anyway.
                let _ = tx.try_send(Update::Progress(p));
            };
            let token = token.as_deref();

            match op {
                Operation::Fetch => remote::fetch(&repo, None, token, &mut sink),
                Operation::Pull => remote::pull(&repo, None, false, token, &mut sink),
                Operation::Push | Operation::PushForceWithLease => {
                    let mode = if op == Operation::PushForceWithLease {
                        PushMode::ForceWithLease
                    } else {
                        PushMode::Normal
                    };
                    // Publishing an unpublished branch needs --set-upstream, or
                    // git fails with advice instead of doing the obvious thing.
                    let set_upstream = !remote::has_upstream(&repo, &branch);
                    remote::push(
                        &repo,
                        "origin",
                        &branch,
                        mode,
                        set_upstream,
                        token,
                        &mut sink,
                    )
                }
            }
            .map_err(|e| e.to_string())
        })();

        let _ = tx.send_blocking(Update::Done(result));
    });

    let toast_overlay = find_toast_overlay(window);
    let window = window.clone();

    glib::spawn_future_local(async move {
        let mut last_phase = String::new();
        while let Ok(update) = rx.recv().await {
            match update {
                Update::Progress(p) => {
                    // Only announce phase changes. Percentages tick many times
                    // a second and would make a toast unreadable.
                    if p.phase != last_phase {
                        last_phase = p.phase.clone();
                        tracing::debug!(phase = %p.phase, percent = ?p.percent, "transfer");
                    }
                }
                Update::Done(result) => {
                    match (&result, &toast_overlay) {
                        (Ok(()), Some(overlay)) => {
                            overlay.add_toast(adw::Toast::new(&format!("{} complete", op.label())));
                        }
                        (Err(message), _) => {
                            // Transfer failures carry git's own diagnostics and
                            // are usually actionable — a dialog, not a toast
                            // that vanishes before it is read.
                            let dialog = adw::AlertDialog::new(
                                Some(&format!("{} failed", op.label())),
                                Some(message),
                            );
                            dialog.add_response("ok", "OK");
                            dialog.present(Some(&window));
                        }
                        _ => {}
                    }
                    on_done(result);
                    return;
                }
            }
        }
    });
}

/// Token for the default github.com account, if one is signed in.
///
/// `None` is normal and not an error: SSH remotes authenticate through
/// ssh-agent and need nothing from us.
fn default_token() -> Option<String> {
    let db = db::Db::open_default().ok()?;
    let row = db.default_account(auth::DEFAULT_HOST).ok()??;
    let account = auth::Account {
        host: row.host,
        login: row.login,
        is_default_for_host: true,
    };
    let token = auth::store::load(&account).ok()?;
    Some(token.access.expose().to_string())
}

/// Locate the window's toast overlay, if the layout has one.
fn find_toast_overlay(window: &adw::ApplicationWindow) -> Option<adw::ToastOverlay> {
    window.content().and_downcast::<adw::ToastOverlay>()
}
