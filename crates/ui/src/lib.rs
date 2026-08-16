//! GTK4 / libadwaita front end.
//!
//! Everything in this crate runs on the glib main context. Git work reads from
//! mmapped packfiles and is fast enough to stay inline (see
//! [`commit_list::row_factory`]); network work is handed to a tokio runtime and
//! returns over an `async_channel`.

pub mod changes;
pub mod commit_list;
pub mod conflicts;
pub mod diff_view;
pub mod login;
pub mod pulls;
pub mod settings;
pub mod stash;
pub mod state;
pub mod sync;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;

use commit_list::CommitListModel;
use state::AppState;

/// A callback slot filled in after construction.
///
/// Needed because the Pull Requests page must be able to reload the window, but
/// the window's `Views` does not exist until after the page is built. The cell
/// breaks the cycle without leaking either side.
type ReloadSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

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
    load_css();

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

    // Reveals the branch list once a narrow window has hidden it. Always
    // present rather than only when collapsed: a toggle that appears and
    // disappears shifts every other button sideways as the window resizes.
    let sidebar_btn = gtk::ToggleButton::new();
    sidebar_btn.set_icon_name("sidebar-show-symbolic");
    sidebar_btn.set_tooltip_text(Some("Show branches"));
    header.pack_start(&sidebar_btn);

    let open_btn = gtk::Button::from_icon_name("folder-open-symbolic");
    open_btn.set_tooltip_text(Some("Open a repository"));
    header.pack_start(&open_btn);

    let stash_btn = gtk::Button::from_icon_name("edit-paste-symbolic");
    stash_btn.set_tooltip_text(Some("Stashes"));
    header.pack_start(&stash_btn);

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
    let conflicts = conflicts::ConflictView::new(state.clone());

    // Checking out a pull request moves HEAD, so the whole window follows.
    let pulls_view = {
        let state_ = state.clone();
        let reload: ReloadSlot = Rc::new(RefCell::new(None));
        let reload_ = reload.clone();
        let view = pulls::PullsView::new(
            state_,
            rt.clone(),
            Rc::new(move || {
                if let Some(f) = reload_.borrow().as_ref() {
                    f();
                }
            }),
        );
        (view, reload)
    };
    let (pulls_page_view, pulls_reload) = pulls_view;

    let stack = adw::ViewStack::new();
    stack.add_titled_with_icon(&main_pane, Some("history"), "History", "view-list-symbolic");
    stack.add_titled_with_icon(
        &changes.root,
        Some("changes"),
        "Changes",
        "document-edit-symbolic",
    );

    // The Conflicts page exists only while a merge is stopped. A permanently
    // visible tab that is empty nine times out of ten trains people to ignore
    // it, which is the opposite of what it is for.
    let conflicts_page = stack.add_titled_with_icon(
        &conflicts.root,
        Some("conflicts"),
        "Conflicts",
        "dialog-warning-symbolic",
    );
    conflicts_page.set_visible(false);

    // Pull requests need both a GitHub remote and a signed-in account. Hidden
    // rather than empty: a tab that can never load anything is noise.
    let pulls_page = stack.add_titled_with_icon(
        &pulls_page_view.root,
        Some("pulls"),
        "Pull Requests",
        "mail-send-receive-symbolic",
    );
    pulls_page.set_visible(false);

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
        let conflicts_ = conflicts.clone();
        let pulls_ = pulls_page_view.clone();
        let prefs = prefs.clone();
        stack.connect_visible_child_name_notify(move |s| {
            let Some(name) = s.visible_child_name() else {
                return;
            };
            if name == "changes" {
                changes.refresh();
            }
            if name == "conflicts" {
                conflicts_.refresh();
            }
            if name == "pulls" {
                pulls_.refresh();
            }
            // Only the permanent pages are worth restoring — a Conflicts page
            // saved here would be gone by the next launch.
            if name != "conflicts" && name != "pulls" {
                if let Some(p) = &prefs {
                    p.set_string("last-page", &name).ok();
                }
            }
        });
    }

    let content = adw::ToolbarView::new();
    content.add_top_bar(&header);
    content.set_content(Some(&stack));

    // OverlaySplitView, not NavigationSplitView.
    //
    // NavigationSplitView collapses into a *navigation stack*: the sidebar
    // becomes a page you navigate away from, so on a narrow window the commit
    // list and diff disappear entirely until the user taps a branch. Overlay
    // keeps content on screen full-width and slides the branch list over it,
    // which is what a sidebar should do when space runs out.
    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .max_sidebar_width(260.0)
        .build();

    // The overlay wraps everything so `sync` can find it from the window and
    // post toasts without every call site threading a reference through.
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    window.set_content(Some(&toasts));

    // Collapse the branch sidebar on a narrow window.
    //
    // Not a hypothetical: COSMIC auto-tiles, so a window on a 1366px display
    // routinely gets half of it. At 680px the three columns — branches, file
    // lists, diff — left the diff about 130px wide and unreadable. The sidebar
    // is the one that can fold away without losing a workflow, since the branch
    // list is navigation rather than working surface.
    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        900.0,
        adw::LengthUnit::Px,
    ));
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    // Collapsed alone would leave the sidebar overlaying the content on open.
    // Hiding it too means a narrow window starts on the work, with the branch
    // list one button away.
    breakpoint.add_setter(&split, "show-sidebar", Some(&false.to_value()));
    // Three columns of conflict text need real width; stack them instead.
    breakpoint.add_setter(
        &conflicts.sides,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );

    // Narrower still: the file list and the diff cannot share a row either, so
    // the Changes page stacks them.
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        640.0,
        adw::LengthUnit::Px,
    ));
    narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
    narrow.add_setter(&split, "show-sidebar", Some(&false.to_value()));
    narrow.add_setter(
        &changes.paned,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    narrow.add_setter(
        &conflicts.sides,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    window.add_breakpoint(breakpoint);
    window.add_breakpoint(narrow);

    // Two-way: the button drives the sidebar, and a breakpoint hiding the
    // sidebar un-presses the button, so its state never lies about what is
    // on screen.
    split
        .bind_property("show-sidebar", &sidebar_btn, "active")
        .bidirectional()
        .sync_create()
        .build();

    // --- wiring -------------------------------------------------------------
    let views = Views {
        window: window.clone(),
        state: state.clone(),
        model: model.clone(),
        refs_list: refs_list.clone(),
        sidebar_title: sidebar_title.clone(),
        prefs: prefs.clone(),
        stack: stack.clone(),
        conflicts_page: conflicts_page.clone(),
        conflicts: conflicts.clone(),
        pulls_page: pulls_page.clone(),
        pulls: pulls_page_view.clone(),
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
    {
        let views_ = views.clone();
        conflicts.connect_changed(Rc::new(move || {
            // Concluding or aborting a merge moves HEAD, so history, refs and
            // the conflicts page all have to catch up together.
            if let Some(Some(path)) = views_
                .state
                .with(|s| s.repo.workdir().map(|p| p.to_path_buf()))
            {
                views_.load_repo(&path);
            }
        }));
    }

    // The remembered page is restored *before* the repository loads, not after.
    //
    // After was wrong: `load_repo` switches to the Conflicts page when a merge
    // is stopped, and a restore running later silently put the user back on
    // Changes with a conflict tab they had to notice for themselves. An
    // in-progress conflict outranks a remembered preference.
    if let Some(p) = &prefs {
        stack.set_visible_child_name(&p.string("last-page"));
    }

    {
        let views_ = views.clone();
        *pulls_reload.borrow_mut() = Some(Rc::new(move || {
            if let Some(Some(path)) = views_
                .state
                .with(|s| s.repo.workdir().map(|p| p.to_path_buf()))
            {
                views_.load_repo(&path);
            }
        }));
    }

    if let Some(path) = startup {
        views.load_repo(&path);
    }

    // Whatever page won, populate it — the stack's notify handler only fires on
    // a *change*, so a page that was already current never refreshed.
    match stack.visible_child_name().as_deref() {
        Some("conflicts") => conflicts.refresh(),
        _ => changes.refresh(),
    }

    // --- actions and accelerators -------------------------------------------
    //
    // Every header button also gets a GAction, so the whole app is reachable
    // from the keyboard and each entry appears in the shell's action list.
    // Registering them on the application rather than the window also puts them
    // on the session bus, which is how they can be driven without a pointer.
    install_actions(
        app,
        &open_btn,
        &stash_btn,
        &fetch_btn,
        &pull_btn,
        &push_btn,
        &account_btn,
    );

    {
        let views_ = views.clone();
        let window_ = window.clone();
        stash_btn.connect_clicked(move |_| {
            let views_inner = views_.clone();
            stash::StashDialog::present(
                &window_,
                views_inner.state.clone(),
                // Stashing and popping both rewrite the working tree, so every
                // view of it has to catch up.
                Rc::new(move || {
                    if let Some(Some(path)) = views_inner
                        .state
                        .with(|s| s.repo.workdir().map(|p| p.to_path_buf()))
                    {
                        views_inner.load_repo(&path);
                    }
                }),
            );
        });
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

/// Build an API client for the default account, if one is signed in.
///
/// Returns `None` when there is no account or the keyring is unreachable —
/// both mean the GitHub views cannot work, and neither is an error worth
/// interrupting the user over.
fn github_client() -> Option<std::sync::Arc<github::Client>> {
    let store = std::sync::Arc::new(db::Db::open_default().ok()?);
    let row = store.default_account(auth::DEFAULT_HOST).ok()??;
    let account = auth::Account {
        host: row.host,
        login: row.login,
        is_default_for_host: true,
    };
    let token = auth::store::load(&account).ok()?;
    github::Client::new(account, token.access, store)
        .ok()
        .map(std::sync::Arc::new)
}

/// Register one action per header button, plus its accelerator.
///
/// The actions activate the buttons rather than duplicating their handlers, so
/// there is exactly one implementation of each command and a disabled button
/// disables its shortcut for free.
fn install_actions(
    app: &adw::Application,
    open_btn: &gtk::Button,
    stash_btn: &gtk::Button,
    fetch_btn: &gtk::Button,
    pull_btn: &gtk::Button,
    push_btn: &gtk::Button,
    account_btn: &gtk::Button,
) {
    let entries: [(&str, &gtk::Button, &[&str]); 6] = [
        ("open", open_btn, &["<Control>o"]),
        ("stashes", stash_btn, &["<Control><Shift>s"]),
        ("fetch", fetch_btn, &["<Control>r"]),
        ("pull", pull_btn, &["<Control><Shift>p"]),
        ("push", push_btn, &["<Control>p"]),
        ("account", account_btn, &[]),
    ];

    for (name, button, accels) in entries {
        let action = gtk::gio::SimpleAction::new(name, None);
        let button = button.clone();
        action.connect_activate(move |_, _| {
            if button.is_sensitive() {
                button.emit_clicked();
            }
        });
        app.add_action(&action);
        if !accels.is_empty() {
            app.set_accels_for_action(&format!("app.{name}"), accels);
        }
    }
}

/// Install the application stylesheet once per display.
///
/// `APPLICATION` priority sits above the theme but below user overrides in
/// `~/.config/gtk-4.0/gtk.css`, so a user who wants different diff colours can
/// still have them.
fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(diff_view::CSS);

    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
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
    stack: adw::ViewStack,
    conflicts_page: adw::ViewStackPage,
    conflicts: Rc<conflicts::ConflictView>,
    pulls_page: adw::ViewStackPage,
    pulls: Rc<pulls::PullsView>,
}

impl Views {
    /// Reveal the Conflicts page while a merge is stopped, hide it otherwise,
    /// and jump to it the moment conflicts appear — a merge that stops with
    /// conflicts is not something to leave the user to discover.
    fn update_conflicts(&self) {
        let conflicted = self.conflicts.has_conflicts();
        let was_visible = self.conflicts_page.is_visible();

        self.conflicts_page.set_visible(conflicted);
        if conflicted {
            self.conflicts.refresh();
            if !was_visible {
                self.stack.set_visible_child_name("conflicts");
            }
        } else if self.stack.visible_child_name().as_deref() == Some("conflicts") {
            self.stack.set_visible_child_name("changes");
        }
    }

    /// Bind the Pull Requests page to this repository, or hide it.
    ///
    /// Requires both a parseable GitHub remote and a stored token. Either alone
    /// gives a page that can only ever show an error.
    fn update_pulls(&self) {
        let target = pulls::detect_repo(&self.state).and_then(|(owner, repo)| {
            let client = github_client()?;
            Some(pulls::Target {
                owner,
                repo,
                client,
            })
        });

        let available = target.is_some();
        self.pulls.set_target(target);
        self.pulls_page.set_visible(available);

        if !available && self.stack.visible_child_name().as_deref() == Some("pulls") {
            self.stack.set_visible_child_name("history");
        }
    }

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

        self.update_conflicts();
        self.update_pulls();

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
