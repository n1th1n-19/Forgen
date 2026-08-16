//! GTK4 / libadwaita front end.
//!
//! Everything in this crate runs on the glib main context. Git work reads from
//! mmapped packfiles and is fast enough to stay inline (see
//! [`commit_list::row_factory`]); network work is handed to a tokio runtime and
//! returns over an `async_channel`.

pub mod changes;
pub mod commit_list;
pub mod login;
pub mod settings;
pub mod state;
pub mod sync;

use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;

use commit_list::CommitListModel;
use state::AppState;

pub const APP_ID: &str = "io.github.forqen.Forqen";

/// Build and present the main window.
///
/// `initial_repo` opens a repository immediately — the path from `forqen
/// /some/repo`, or the most recent one from settings.
pub fn build_window(
    app: &adw::Application,
    rt: tokio::runtime::Handle,
    initial_repo: Option<&Path>,
) -> adw::ApplicationWindow {
    let state = AppState::new();
    let prefs = settings::open();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("forqen")
        .build();

    // Geometry comes from settings, which also carry the schema's defaults —
    // hardcoding a size here would silently win over the user's last resize.
    settings::bind_window(prefs.as_ref(), &window);

    let model = CommitListModel::new(state.clone());
    state.set_row_budget(settings::commit_row_budget(prefs.as_ref()));

    // --- header -------------------------------------------------------------
    let header = adw::HeaderBar::new();

    let open_btn = gtk::Button::from_icon_name("folder-open-symbolic");
    open_btn.set_tooltip_text(Some("Open a repository"));
    header.pack_start(&open_btn);

    let account_btn = gtk::Button::from_icon_name("avatar-default-symbolic");
    account_btn.set_tooltip_text(Some("Sign in to GitHub"));
    header.pack_end(&account_btn);

    // Push is separated from fetch/pull because it is the only one that changes
    // someone else's copy of history; grouping it with read-only operations
    // makes it too easy to hit by reflex.
    let push_btn = gtk::Button::from_icon_name("send-to-symbolic");
    push_btn.set_tooltip_text(Some("Push to origin"));
    header.pack_end(&push_btn);

    let pull_btn = gtk::Button::from_icon_name("document-save-symbolic");
    pull_btn.set_tooltip_text(Some("Pull from origin"));
    header.pack_end(&pull_btn);

    let fetch_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
    fetch_btn.set_tooltip_text(Some("Fetch all remotes"));
    header.pack_end(&fetch_btn);

    // --- sidebar ------------------------------------------------------------
    let refs_list = gtk::ListBox::new();
    refs_list.add_css_class("navigation-sidebar");
    refs_list.set_selection_mode(gtk::SelectionMode::Single);

    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .child(&refs_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    // The repository name and branch live here, not in the main header.
    //
    // They started out as the main header's title widget, which silently broke
    // the app: `load_repo` replaced the title widget on every open, so the
    // ViewSwitcher set during construction was destroyed the moment a
    // repository loaded and the Changes page became unreachable. A header bar
    // has exactly one title widget, so the switcher gets it and the repository
    // context goes where it is more at home anyway — above the branch list.
    let sidebar_title = adw::WindowTitle::new("Branches", "");
    let sidebar = adw::ToolbarView::new();
    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.set_title_widget(Some(&sidebar_title));
    sidebar.add_top_bar(&sidebar_header);
    sidebar.set_content(Some(&sidebar_scroll));

    // --- commit list --------------------------------------------------------
    let selection = gtk::SingleSelection::new(Some(model.clone()));
    let column_view = gtk::ColumnView::builder()
        .model(&selection)
        .show_column_separators(false)
        .build();

    let column = gtk::ColumnViewColumn::builder()
        .title("History")
        .expand(true)
        .factory(&commit_list::row_factory(state.clone()))
        .build();
    column_view.append_column(&column);

    let list_scroll = gtk::ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .build();

    // --- detail pane --------------------------------------------------------
    let detail = gtk::Label::new(Some("Select a commit"));
    detail.set_wrap(true);
    detail.set_xalign(0.0);
    detail.set_yalign(0.0);
    detail.set_margin_top(12);
    detail.set_margin_bottom(12);
    detail.set_margin_start(12);
    detail.set_margin_end(12);
    detail.set_selectable(true);

    let detail_scroll = gtk::ScrolledWindow::builder()
        .child(&detail)
        .height_request(180)
        .build();

    let main_pane = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&list_scroll)
        .end_child(&detail_scroll)
        .resize_start_child(true)
        .build();

    // --- pages --------------------------------------------------------------
    let changes = changes::ChangesView::new(state.clone());

    let stack = adw::ViewStack::new();
    stack.add_titled_with_icon(&main_pane, Some("history"), "History", "view-list-symbolic");
    stack.add_titled_with_icon(
        &changes.root,
        Some("changes"),
        "Changes",
        "document-edit-symbolic",
    );

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    header.set_title_widget(Some(&switcher));

    // Status is re-read on entering the page rather than on a timer: polling
    // the working tree of a large repository every few seconds is real IO for
    // a view nobody is looking at.
    {
        let changes = changes.clone();
        let prefs = prefs.clone();
        stack.connect_visible_child_name_notify(move |s| {
            let Some(name) = s.visible_child_name() else {
                return;
            };
            if name == "changes" {
                changes.refresh();
            }
            if let Some(p) = &prefs {
                p.set_string("last-page", &name).ok();
            }
        });
    }

    let content = adw::ToolbarView::new();
    content.add_top_bar(&header);
    content.set_content(Some(&stack));

    let split = adw::NavigationSplitView::builder()
        .sidebar(&adw::NavigationPage::new(&sidebar, "Branches"))
        .content(&adw::NavigationPage::new(&content, "History"))
        .build();

    // The overlay wraps everything so `sync` can find it from the window and
    // post toasts without every call site threading a reference through.
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    window.set_content(Some(&toasts));

    // --- wiring -------------------------------------------------------------
    let views = Views {
        window: window.clone(),
        state: state.clone(),
        model: model.clone(),
        refs_list: refs_list.clone(),
        sidebar_title: sidebar_title.clone(),
        prefs: prefs.clone(),
    };

    {
        let views = views.clone();
        open_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Open a git repository")
                .modal(true)
                .build();

            let views = views.clone();
            dialog.select_folder(
                Some(&views.window.clone()),
                None::<&gtk::gio::Cancellable>,
                move |result| {
                    // Cancel and error both land here; neither is worth a dialog.
                    let Ok(folder) = result else { return };
                    let Some(path) = folder.path() else { return };
                    views.load_repo(&path);
                },
            );
        });
    }

    wire_selection(&selection, &state, &detail);
    wire_branch_switching(&refs_list, &views);

    // --- sync buttons --------------------------------------------------------
    for (button, op) in [
        (&fetch_btn, sync::Operation::Fetch),
        (&pull_btn, sync::Operation::Pull),
        (&push_btn, sync::Operation::Push),
    ] {
        let views = views.clone();
        let window_ = window.clone();
        let buttons = [fetch_btn.clone(), pull_btn.clone(), push_btn.clone()];

        button.connect_clicked(move |_| {
            let Some((path, branch)) = views.state.with(|s| {
                (
                    s.repo.workdir().map(std::path::Path::to_path_buf),
                    s.repo.current_branch(),
                )
            }) else {
                return;
            };
            let (Some(path), Some(branch)) = (path, branch) else {
                // Detached HEAD has nothing to push, and pulling into one is a
                // trap rather than a convenience.
                return;
            };

            // Disable all three for the duration: concurrent transfers on one
            // repository race over the index lock and fail confusingly.
            for b in &buttons {
                b.set_sensitive(false);
            }

            let views_ = views.clone();
            let buttons_ = buttons.clone();
            sync::run(
                &window_,
                path.clone(),
                branch,
                op,
                Rc::new(move |result| {
                    for b in &buttons_ {
                        b.set_sensitive(true);
                    }
                    // Refs and history both move on a successful transfer.
                    if result.is_ok() {
                        views_.load_repo(&path);
                    }
                }),
            );
        });
    }

    // A path on the command line wins; otherwise reopen the last repository, so
    // launching from the dock lands where the user left off rather than on an
    // empty window.
    let startup = initial_repo
        .map(Path::to_path_buf)
        .or_else(|| settings::recent(prefs.as_ref()).into_iter().next());
    if let Some(path) = startup {
        views.load_repo(&path);
    }

    // Restore the page last open. After the repository loads, so the Changes
    // page has status to show rather than rendering empty and then filling in.
    if let Some(p) = &prefs {
        let name = p.string("last-page");
        stack.set_visible_child_name(&name);
        if name == "changes" {
            changes.refresh();
        }
    }

    {
        let window_ = window.clone();
        let rt_ = rt.clone();
        account_btn.connect_clicked(move |_| {
            let cb: login::OnLogin = Rc::new(|account, _token| {
                tracing::info!(login = %account.login, host = %account.host, "signed in");
            });
            login::present(&window_, rt_.clone(), cb);
        });
    }

    window
}

/// The widgets a repository load has to touch.
///
/// Bundled because loading is triggered from three places — the open button,
/// the command line, and session restore — and threading six clones through
/// each closure by hand is where a missed `.clone()` turns into a borrow error
/// three layers deep in a callback.
#[derive(Clone)]
struct Views {
    window: adw::ApplicationWindow,
    state: AppState,
    model: CommitListModel,
    refs_list: gtk::ListBox,
    sidebar_title: adw::WindowTitle,
    prefs: Option<gtk::gio::Settings>,
}

impl Views {
    /// Open a repository and populate every view that shows it.
    fn load_repo(&self, path: &Path) {
        self.model.clear();

        if let Err(e) = self.state.open_repo(path) {
            let dialog =
                adw::AlertDialog::new(Some("Could not open repository"), Some(&e.to_string()));
            dialog.add_response("ok", "OK");
            dialog.present(Some(&self.window));
            return;
        }

        self.model.sync_length();
        populate_refs(&self.refs_list, &self.state);
        settings::push_recent(self.prefs.as_ref(), path);

        if let Some((name, branch)) = self.state.with(|s| s.name_and_branch()) {
            self.sidebar_title.set_title(&name);
            self.sidebar_title.set_subtitle(&branch);
            // The window title is what the task switcher and dock show.
            self.window.set_title(Some(&format!("{name} — {branch}")));
        }
    }
}

/// Double-click a local branch in the sidebar to switch to it.
///
/// Double-click rather than single: the sidebar is also how you select a branch
/// to look at, and a single click that silently rewrites the working tree is
/// the kind of thing that loses uncommitted work.
fn wire_branch_switching(list: &gtk::ListBox, views: &Views) {
    let views = views.clone();

    list.connect_row_activated(move |_, row| {
        let Some(label) = row.child().and_downcast::<gtk::Label>() else {
            return;
        };
        let name = label.text().to_string();

        // Only local branches are checkout targets. A remote branch would need
        // a tracking branch created first, and a tag would detach HEAD.
        let is_local = views
            .state
            .with(|s| {
                s.refs
                    .iter()
                    .any(|r| r.kind == git::refs::RefKind::LocalBranch && r.short == name)
            })
            .unwrap_or(false);
        if !is_local {
            return;
        }

        let Some(result) = views
            .state
            .with(|s| git::branch::checkout(&s.repo, &name, false))
        else {
            return;
        };

        match result {
            Ok(git::branch::CheckoutOutcome::Switched) => {
                if let Some(Some(path)) = views
                    .state
                    .with(|s| s.repo.workdir().map(|p| p.to_path_buf()))
                {
                    views.load_repo(&path);
                }
            }
            Ok(git::branch::CheckoutOutcome::Blocked { reason, paths }) => {
                let detail = match reason {
                    git::branch::CheckoutBlocker::OperationInProgress => format!(
                        "A {} is in progress. Finish or abort it first.",
                        paths.first().map(String::as_str).unwrap_or("operation")
                    ),
                    git::branch::CheckoutBlocker::WouldOverwrite => format!(
                        "Switching would overwrite uncommitted changes:\n\n{}\n\n\
                         Commit or stash them first.",
                        paths.join("\n")
                    ),
                };
                let dialog =
                    adw::AlertDialog::new(Some(&format!("Cannot switch to {name}")), Some(&detail));
                dialog.add_response("ok", "OK");
                dialog.present(Some(&views.window));
            }
            Err(e) => {
                let dialog = adw::AlertDialog::new(Some("Checkout failed"), Some(&e.to_string()));
                dialog.add_response("ok", "OK");
                dialog.present(Some(&views.window));
            }
        }
    });
}

fn populate_refs(list: &gtk::ListBox, state: &AppState) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    state.with(|s| {
        let mut last_kind = None;
        for r in &s.refs {
            if last_kind != Some(r.kind) {
                let header = gtk::Label::new(Some(match r.kind {
                    git::refs::RefKind::LocalBranch => "Local",
                    git::refs::RefKind::RemoteBranch => "Remotes",
                    git::refs::RefKind::Tag => "Tags",
                }));
                header.add_css_class("heading");
                header.add_css_class("dim-label");
                header.set_xalign(0.0);
                header.set_margin_top(8);
                header.set_margin_start(8);

                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&header));
                row.set_selectable(false);
                row.set_activatable(false);
                list.append(&row);
                last_kind = Some(r.kind);
            }

            let label = gtk::Label::new(Some(&r.short));
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_margin_start(16);
            label.set_margin_top(4);
            label.set_margin_bottom(4);

            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&label));
            list.append(&row);
        }
    });
}

/// Show the selected commit's full message and metadata.
fn wire_selection(selection: &gtk::SingleSelection, state: &AppState, detail: &gtk::Label) {
    let state = state.clone();
    let detail = detail.clone();

    selection.connect_selected_item_notify(move |sel| {
        let Some(item) = sel
            .selected_item()
            .and_downcast::<commit_list::CommitItem>()
        else {
            return;
        };
        let index = item.index() as usize;

        let text = state.with(|s| {
            let end = (index + 1).min(s.window.len());
            let _ = s.window.ensure(&s.repo, index..end);
            s.window.row(index).map(|row| {
                format!(
                    "{}\n\n{} <{}>\n{}",
                    row.summary,
                    row.author_name,
                    row.author_email,
                    row.id.to_hex()
                )
            })
        });

        if let Some(Some(text)) = text {
            detail.set_text(&text);
        }
    });
}
