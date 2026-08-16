//! The Pull Requests page.
//!
//! Reachable only when the repository has a recognisable GitHub remote and an
//! account is signed in. Without both, the page hides rather than showing an
//! empty list the user cannot act on.
//!
//! Everything here is a network round trip, so the pattern is the same as
//! [`crate::sync`]: work on the tokio runtime, results back over an
//! `async_channel`, widgets touched only on the glib main context.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

use github::pulls::{self, PullFile, PullRequest, PullState};
use github::Provenance;

use crate::diff_view::DiffView;
use crate::state::AppState;

/// The repository a page is bound to, plus how to reach it.
#[derive(Clone)]
pub struct Target {
    pub owner: String,
    pub repo: String,
    pub client: Arc<github::Client>,
}

enum Msg {
    List(Result<(Vec<PullRequest>, Provenance), String>),
    Files(Result<Vec<PullFile>, String>),
}

pub struct PullsView {
    pub root: gtk::Widget,
    state: AppState,
    rt: tokio::runtime::Handle,
    list: gtk::ListBox,
    diff: Rc<DiffView>,
    file_list: gtk::ListBox,
    title: gtk::Label,
    subtitle: gtk::Label,
    status: gtk::Label,
    spinner: gtk::Spinner,
    checkout_btn: gtk::Button,
    open_web_btn: gtk::Button,
    target: RefCell<Option<Target>>,
    selected: Rc<RefCell<Option<PullRequest>>>,
    files: Rc<RefCell<Vec<PullFile>>>,
    on_checkout: Rc<dyn Fn()>,
}

impl PullsView {
    pub fn new(state: AppState, rt: tokio::runtime::Handle, on_checkout: Rc<dyn Fn()>) -> Rc<Self> {
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);

        let file_list = gtk::ListBox::new();
        file_list.add_css_class("navigation-sidebar");
        file_list.set_selection_mode(gtk::SelectionMode::Single);

        let diff = DiffView::new();

        let title = gtk::Label::new(Some("Select a pull request"));
        title.set_xalign(0.0);
        title.add_css_class("title-4");
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_wrap(false);

        let subtitle = gtk::Label::new(None);
        subtitle.set_xalign(0.0);
        subtitle.add_css_class("dim-label");
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let spinner = gtk::Spinner::new();

        let checkout_btn = gtk::Button::with_label("Check out");
        checkout_btn.add_css_class("suggested-action");
        checkout_btn.set_sensitive(false);

        let open_web_btn = gtk::Button::from_icon_name("web-browser-symbolic");
        open_web_btn.set_tooltip_text(Some("Open on GitHub"));
        open_web_btn.set_sensitive(false);

        let root = build_layout(
            &list,
            &file_list,
            &diff.root,
            &title,
            &subtitle,
            &status,
            &spinner,
            &checkout_btn,
            &open_web_btn,
        );

        let view = Rc::new(Self {
            root,
            state,
            rt,
            list,
            diff,
            file_list,
            title,
            subtitle,
            status,
            spinner,
            checkout_btn,
            open_web_btn,
            target: RefCell::new(None),
            selected: Rc::new(RefCell::new(None)),
            files: Rc::new(RefCell::new(Vec::new())),
            on_checkout,
        });

        {
            let this = view.clone();
            view.checkout_btn.connect_clicked(move |_| this.checkout());
        }
        {
            let this = view.clone();
            view.open_web_btn.connect_clicked(move |_| this.open_web());
        }

        view
    }

    /// Point the page at a repository, or at nothing.
    pub fn set_target(self: &Rc<Self>, target: Option<Target>) {
        *self.target.borrow_mut() = target;
        self.clear();
    }

    pub fn has_target(&self) -> bool {
        self.target.borrow().is_some()
    }

    fn clear(&self) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        while let Some(c) = self.file_list.first_child() {
            self.file_list.remove(&c);
        }
        self.diff.clear();
        self.title.set_text("Select a pull request");
        self.subtitle.set_text("");
        *self.selected.borrow_mut() = None;
        self.files.borrow_mut().clear();
        self.checkout_btn.set_sensitive(false);
        self.open_web_btn.set_sensitive(false);
    }

    /// Fetch the open pull requests.
    pub fn refresh(self: &Rc<Self>) {
        let Some(target) = self.target.borrow().clone() else {
            self.status.set_text("No GitHub remote for this repository");
            return;
        };

        self.spinner.start();
        self.status.set_text("Loading pull requests…");

        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .pulls(&target.owner, &target.repo, PullState::Open)
                .await
                .map(|r| (r.data, r.provenance))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::List(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::List(result)) = rx.recv().await {
                this.spinner.stop();
                match result {
                    Ok((pulls, provenance)) => this.populate(pulls, provenance),
                    Err(e) => this.status.set_text(&format!("Could not load: {e}")),
                }
            }
        });
    }

    fn populate(self: &Rc<Self>, pulls: Vec<PullRequest>, provenance: Provenance) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }

        // Say when data is not live. A cached list that looks current is worse
        // than no list, because it silently hides a PR opened five minutes ago.
        self.status.set_text(&match (pulls.len(), provenance) {
            (0, _) => "No open pull requests".to_string(),
            (n, Provenance::OfflineCache) => format!("{n} open — offline, showing cached data"),
            (n, Provenance::Revalidated) => format!("{n} open"),
            (n, Provenance::Fresh) => format!("{n} open"),
        });

        for pr in &pulls {
            self.list.append(&self.row(pr));
        }
    }

    fn row(self: &Rc<Self>, pr: &PullRequest) -> gtk::ListBoxRow {
        let title = gtk::Label::new(Some(&pr.title));
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let author = pr
            .user
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_else(|| "unknown".into());
        let mut meta = format!("#{} · {author}", pr.number);
        if pr.is_draft() {
            meta.push_str(" · draft");
        }
        if pr.is_from_fork() {
            meta.push_str(" · fork");
        }

        let subtitle = gtk::Label::new(Some(&meta));
        subtitle.set_xalign(0.0);
        subtitle.add_css_class("dim-label");
        subtitle.add_css_class("caption");

        let boxed = gtk::Box::new(gtk::Orientation::Vertical, 2);
        boxed.set_margin_start(8);
        boxed.set_margin_end(8);
        boxed.set_margin_top(6);
        boxed.set_margin_bottom(6);
        boxed.append(&title);
        boxed.append(&subtitle);

        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&boxed));

        let this = self.clone();
        let pr = pr.clone();
        let click = gtk::GestureClick::new();
        click.connect_released(move |_, _, _, _| this.select(&pr));
        row.add_controller(click);

        row
    }

    fn select(self: &Rc<Self>, pr: &PullRequest) {
        *self.selected.borrow_mut() = Some(pr.clone());
        self.title.set_text(&pr.title);
        self.subtitle.set_text(&format!(
            "#{} · {} → {}",
            pr.number, pr.head.branch, pr.base.branch
        ));
        self.checkout_btn.set_sensitive(true);
        self.open_web_btn.set_sensitive(pr.html_url.is_some());

        let Some(target) = self.target.borrow().clone() else {
            return;
        };
        let number = pr.number;

        self.spinner.start();
        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .pull_files(&target.owner, &target.repo, number)
                .await
                .map(|r| r.data)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Files(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Files(result)) = rx.recv().await {
                this.spinner.stop();
                match result {
                    Ok(files) => this.populate_files(files),
                    Err(e) => this.status.set_text(&format!("Could not load files: {e}")),
                }
            }
        });
    }

    fn populate_files(self: &Rc<Self>, files: Vec<PullFile>) {
        while let Some(c) = self.file_list.first_child() {
            self.file_list.remove(&c);
        }
        self.diff.clear();

        for (i, f) in files.iter().enumerate() {
            let label = gtk::Label::new(Some(&f.filename));
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_hexpand(true);

            let counts = gtk::Label::new(Some(&format!("+{} −{}", f.additions, f.deletions)));
            counts.add_css_class("dim-label");
            counts.add_css_class("caption");
            counts.add_css_class("monospace");

            let boxed = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            boxed.set_margin_start(8);
            boxed.set_margin_end(8);
            boxed.set_margin_top(4);
            boxed.set_margin_bottom(4);
            boxed.append(&label);
            boxed.append(&counts);

            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&boxed));
            self.file_list.append(&row);

            let this = self.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| this.show_file(i));
            row.add_controller(click);
        }

        *self.files.borrow_mut() = files;
        if !self.files.borrow().is_empty() {
            self.show_file(0);
        }
    }

    fn show_file(self: &Rc<Self>, index: usize) {
        let files = self.files.borrow();
        let Some(file) = files.get(index) else { return };

        match &file.patch {
            Some(patch) => {
                // GitHub sends the hunks without the `diff --git` header, so
                // one is synthesised — the parser keys off it to start a file,
                // and the staging code needs it verbatim if this ever becomes
                // an apply target.
                let old = file.previous_filename.as_deref().unwrap_or(&file.filename);
                let full = format!(
                    "diff --git a/{old} b/{}\n--- a/{old}\n+++ b/{}\n{patch}",
                    file.filename, file.filename
                );
                match git::diff::parse(&full).into_iter().next() {
                    Some(d) => self.diff.show(&d),
                    None => self.diff.clear(),
                }
            }
            // Binary, or a diff GitHub declined to send because it is too
            // large. Neither is an error; both need saying.
            None => {
                let note = git::diff::FileDiff {
                    old_path: file.filename.clone(),
                    new_path: file.filename.clone(),
                    header: vec![],
                    hunks: vec![],
                    is_binary: true,
                };
                self.diff.show(&note);
            }
        }
    }

    fn checkout(self: &Rc<Self>) {
        let Some(pr) = self.selected.borrow().clone() else {
            return;
        };
        let branch = pr.local_branch();

        let result = self.state.with(|s| {
            git::remote::fetch_pull_request(
                &s.repo,
                "origin",
                pr.number,
                &branch,
                None,
                &mut |_| {},
            )
            .and_then(|()| git::branch::checkout(&s.repo, &branch, false))
        });

        match result {
            Some(Ok(git::branch::CheckoutOutcome::Switched)) => {
                (self.on_checkout)();
                self.status.set_text(&format!("Checked out {branch}"));
            }
            Some(Ok(git::branch::CheckoutOutcome::Blocked { paths, .. })) => self.report(
                "Cannot switch branches",
                &format!(
                    "The pull request was fetched into {branch}, but switching \
                     would overwrite uncommitted changes:\n\n{}",
                    paths.join("\n")
                ),
            ),
            Some(Err(e)) => self.report("Checkout failed", &e.to_string()),
            None => self.report("Checkout failed", "No repository open"),
        }
    }

    fn open_web(&self) {
        let Some(url) = self
            .selected
            .borrow()
            .as_ref()
            .and_then(|p| p.html_url.clone())
        else {
            return;
        };
        let launcher = gtk::UriLauncher::new(&url);
        launcher.launch(
            self.root.root().and_downcast::<gtk::Window>().as_ref(),
            None::<&gtk::gio::Cancellable>,
            |_| {},
        );
    }

    fn report(&self, title: &str, message: &str) {
        let dialog = adw::AlertDialog::new(Some(title), Some(message));
        dialog.add_response("ok", "OK");
        if let Some(root) = self.root.root().and_downcast::<gtk::Window>() {
            dialog.present(Some(&root));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_layout(
    list: &gtk::ListBox,
    file_list: &gtk::ListBox,
    diff_root: &gtk::Widget,
    title: &gtk::Label,
    subtitle: &gtk::Label,
    status: &gtk::Label,
    spinner: &gtk::Spinner,
    checkout_btn: &gtk::Button,
    open_web_btn: &gtk::Button,
) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_margin_start(12);
    header.set_margin_end(12);
    header.set_margin_top(8);
    header.set_margin_bottom(8);

    let titles = gtk::Box::new(gtk::Orientation::Vertical, 2);
    titles.set_hexpand(true);
    titles.append(title);
    titles.append(subtitle);

    header.append(&titles);
    header.append(spinner);
    header.append(open_web_btn);
    header.append(checkout_btn);

    let files_scroll = gtk::ScrolledWindow::builder()
        .child(file_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .height_request(140)
        .build();

    let detail = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&files_scroll)
        .end_child(diff_root)
        .resize_start_child(false)
        .shrink_start_child(true)
        .shrink_end_child(true)
        .position(160)
        .build();

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&header);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&detail);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_margin_start(12);
    footer.set_margin_end(12);
    footer.set_margin_top(4);
    footer.set_margin_bottom(4);
    footer.append(status);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&footer);

    let list_scroll = gtk::ScrolledWindow::builder()
        .child(list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(260)
        .build();

    gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&list_scroll)
        .end_child(&right)
        .resize_start_child(false)
        .shrink_start_child(true)
        .shrink_end_child(true)
        .position(280)
        .build()
        .upcast()
}

/// Work out which GitHub repository an open repo corresponds to.
///
/// Prefers `origin`, then any remote that parses as a forge URL. Returns `None`
/// for a repository with no remotes, or only remotes on hosts we cannot map to
/// an owner and name.
pub fn detect_repo(state: &AppState) -> Option<(String, String)> {
    let remotes = state
        .with(|s| git::remote::list(&s.repo))
        .and_then(Result::ok)?;

    remotes
        .iter()
        .find(|r| r.name == "origin")
        .and_then(|r| pulls::parse_remote(&r.fetch_url))
        .or_else(|| {
            remotes
                .iter()
                .find_map(|r| pulls::parse_remote(&r.fetch_url))
        })
}
