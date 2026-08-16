//! Conflict resolution.
//!
//! Shown when a merge stops. Three panes — base, ours, theirs — read from the
//! index stages rather than parsed out of the `<<<<<<<` markers in the working
//! file: the index holds all three as real blobs, whereas the markers cannot
//! express the common ancestor at all under the default `merge.conflictStyle`,
//! and break outright on a file that legitimately contains seven angle
//! brackets.
//!
//! The merged pane is editable. Taking one side wholesale is a button, but a
//! real conflict usually wants pieces of both, and forcing the user out to an
//! editor for that is the point at which a git GUI stops being useful.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use git::merge::{self, Resolution};

use crate::state::AppState;

pub struct ConflictView {
    pub root: gtk::Widget,
    /// The three side-by-side panes, exposed so a window breakpoint can stack
    /// them — three columns of text do not fit a tiled half-screen.
    pub sides: gtk::Box,
    state: AppState,
    file_list: gtk::ListBox,
    base_buf: gtk::TextBuffer,
    ours_buf: gtk::TextBuffer,
    theirs_buf: gtk::TextBuffer,
    merged_buf: gtk::TextBuffer,
    base_frame: gtk::Widget,
    status: gtk::Label,
    finish_btn: gtk::Button,
    abort_btn: gtk::Button,
    /// Path being resolved.
    current: Rc<RefCell<Option<String>>>,
    /// Called after the conflict state changes, so the rest of the window can
    /// refresh — resolving the last conflict ends the merge.
    on_change: RefCell<Option<Rc<dyn Fn()>>>,
}

impl ConflictView {
    pub fn new(state: AppState) -> Rc<Self> {
        let file_list = gtk::ListBox::new();
        file_list.add_css_class("navigation-sidebar");
        file_list.set_selection_mode(gtk::SelectionMode::Single);

        let (base_view, base_buf) = read_only_pane();
        let (ours_view, ours_buf) = read_only_pane();
        let (theirs_view, theirs_buf) = read_only_pane();

        let merged_view = gtk::TextView::new();
        merged_view.set_monospace(true);
        let merged_buf = merged_view.buffer();

        let status = gtk::Label::new(Some("No conflicts"));
        status.set_xalign(0.0);
        status.add_css_class("dim-label");

        let finish_btn = gtk::Button::with_label("Commit merge");
        finish_btn.add_css_class("suggested-action");
        finish_btn.set_sensitive(false);

        let abort_btn = gtk::Button::with_label("Abort merge");
        abort_btn.add_css_class("destructive-action");
        abort_btn.set_sensitive(false);

        let ours_btn = gtk::Button::with_label("Use ours");
        let theirs_btn = gtk::Button::with_label("Use theirs");
        let save_btn = gtk::Button::with_label("Save merged");
        save_btn.add_css_class("suggested-action");

        let base_frame = titled_pane("Base (common ancestor)", &base_view);

        let sides = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sides.set_homogeneous(true);

        let root = build_layout(
            &sides,
            &file_list,
            &base_frame,
            &ours_view,
            &theirs_view,
            &merged_view,
            &status,
            &ours_btn,
            &theirs_btn,
            &save_btn,
            &finish_btn,
            &abort_btn,
        );

        let view = Rc::new(Self {
            root,
            sides,
            state,
            file_list,
            base_buf,
            ours_buf,
            theirs_buf,
            merged_buf,
            base_frame,
            status,
            finish_btn,
            abort_btn,
            current: Rc::new(RefCell::new(None)),
            on_change: RefCell::new(None),
        });

        view.wire(&ours_btn, &theirs_btn, &save_btn);
        view
    }

    pub fn connect_changed(&self, f: Rc<dyn Fn()>) {
        *self.on_change.borrow_mut() = Some(f);
    }

    fn notify(&self) {
        if let Some(f) = self.on_change.borrow().as_ref() {
            f();
        }
    }

    fn wire(self: &Rc<Self>, ours_btn: &gtk::Button, theirs_btn: &gtk::Button, save: &gtk::Button) {
        {
            let this = self.clone();
            ours_btn.connect_clicked(move |_| this.resolve(Resolution::Ours));
        }
        {
            let this = self.clone();
            theirs_btn.connect_clicked(move |_| this.resolve(Resolution::Theirs));
        }
        {
            let this = self.clone();
            save.connect_clicked(move |_| this.save_merged());
        }
        {
            let this = self.clone();
            self.finish_btn.connect_clicked(move |_| this.finish());
        }
        {
            let this = self.clone();
            self.abort_btn
                .connect_clicked(move |_| this.confirm_abort());
        }
    }

    /// True when a merge is in progress and has unresolved paths.
    pub fn has_conflicts(&self) -> bool {
        self.state
            .with(|s| merge::conflicted_paths(&s.repo).map(|p| !p.is_empty()))
            .and_then(Result::ok)
            .unwrap_or(false)
    }

    pub fn refresh(self: &Rc<Self>) {
        while let Some(child) = self.file_list.first_child() {
            self.file_list.remove(&child);
        }

        let paths = self
            .state
            .with(|s| merge::conflicted_paths(&s.repo))
            .and_then(Result::ok)
            .unwrap_or_default();

        let in_merge = self
            .state
            .with(|s| git::branch::operation_in_progress(&s.repo))
            .flatten()
            .is_some();

        self.abort_btn.set_sensitive(in_merge);
        // Concluding is only possible once every conflict is staged; git
        // refuses otherwise, and a button that always fails is worse than a
        // disabled one.
        self.finish_btn.set_sensitive(in_merge && paths.is_empty());

        self.status.set_text(&match (in_merge, paths.len()) {
            (false, _) => "No merge in progress".to_string(),
            (true, 0) => "All conflicts resolved — commit the merge".to_string(),
            (true, 1) => "1 file still conflicted".to_string(),
            (true, n) => format!("{n} files still conflicted"),
        });

        for path in &paths {
            let label = gtk::Label::new(Some(path));
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_margin_start(8);
            label.set_margin_end(8);
            label.set_margin_top(6);
            label.set_margin_bottom(6);

            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&label));
            self.file_list.append(&row);

            let this = self.clone();
            let path = path.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| this.show(&path));
            row.add_controller(click);
        }

        // Keep showing the current file if it is still conflicted; otherwise
        // move to the next one so resolving a list is a straight run rather
        // than a click back into the sidebar each time.
        let current = self.current.borrow().clone();
        match current {
            Some(p) if paths.contains(&p) => self.show(&p),
            _ => match paths.first() {
                Some(p) => self.show(&p.clone()),
                None => self.clear(),
            },
        }
    }

    fn show(self: &Rc<Self>, path: &str) {
        *self.current.borrow_mut() = Some(path.to_string());

        let Some(Ok(sides)) = self.state.with(|s| merge::conflict_sides(&s.repo, path)) else {
            return;
        };

        // An add/add conflict has no common ancestor. Hiding the pane says so
        // more clearly than an empty box the user would read as a bug.
        let has_base = sides.base.is_some();
        self.base_frame.set_visible(has_base);
        self.base_buf.set_text(sides.base.as_deref().unwrap_or(""));

        self.ours_buf.set_text(sides.ours.as_deref().unwrap_or(""));
        self.theirs_buf
            .set_text(sides.theirs.as_deref().unwrap_or(""));

        // The merged pane starts from the working file, which git has already
        // filled with conflict markers — that is the usual starting point for
        // hand-editing, and it shows exactly which regions disagree.
        let working = self
            .state
            .with(|s| {
                s.repo
                    .workdir()
                    .map(|w| w.join(path))
                    .and_then(|p| std::fs::read_to_string(p).ok())
            })
            .flatten()
            .unwrap_or_default();
        self.merged_buf.set_text(&working);
    }

    fn clear(&self) {
        *self.current.borrow_mut() = None;
        for buf in [
            &self.base_buf,
            &self.ours_buf,
            &self.theirs_buf,
            &self.merged_buf,
        ] {
            buf.set_text("");
        }
    }

    fn resolve(self: &Rc<Self>, choice: Resolution) {
        let Some(path) = self.current.borrow().clone() else {
            return;
        };
        match self
            .state
            .with(|s| merge::resolve_with(&s.repo, &path, choice))
        {
            Some(Err(e)) => self.report("Could not resolve", &e.to_string()),
            None => self.report("Could not resolve", "No repository open"),
            Some(Ok(())) => {}
        }
        self.refresh();
        self.notify();
    }

    fn save_merged(self: &Rc<Self>) {
        let Some(path) = self.current.borrow().clone() else {
            return;
        };
        let text = self
            .merged_buf
            .text(
                &self.merged_buf.start_iter(),
                &self.merged_buf.end_iter(),
                false,
            )
            .to_string();

        // Saving a file that still contains markers would stage a broken file
        // and mark the conflict resolved — the single most common way to commit
        // `<<<<<<< HEAD` into a repository.
        if has_conflict_markers(&text) {
            self.report(
                "Conflict markers remain",
                "The merged text still contains <<<<<<<, ======= or >>>>>>> \
                 markers. Remove them before saving, or the markers will be \
                 committed as file content.",
            );
            return;
        }

        match self
            .state
            .with(|s| merge::resolve_with_content(&s.repo, &path, &text))
        {
            Some(Err(e)) => self.report("Could not save", &e.to_string()),
            None => self.report("Could not save", "No repository open"),
            Some(Ok(())) => {}
        }
        self.refresh();
        self.notify();
    }

    fn finish(self: &Rc<Self>) {
        match self.state.with(|s| merge::commit_merge(&s.repo)) {
            Some(Err(e)) => self.report("Could not conclude the merge", &e.to_string()),
            None => self.report("Could not conclude the merge", "No repository open"),
            Some(Ok(())) => {}
        }
        self.refresh();
        self.notify();
    }

    fn confirm_abort(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Abort the merge?"),
            Some(
                "The working tree returns to the state it had before the merge \
                 started. Any conflict resolution done so far is discarded.",
            ),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("abort", "Abort merge");
        dialog.set_response_appearance("abort", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));

        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "abort" {
                return;
            }
            match this.state.with(|s| merge::abort(&s.repo)) {
                Some(Err(e)) => this.report("Could not abort", &e.to_string()),
                None => this.report("Could not abort", "No repository open"),
                Some(Ok(())) => {}
            }
            this.refresh();
            this.notify();
        });

        if let Some(root) = self.root.root().and_downcast::<gtk::Window>() {
            dialog.present(Some(&root));
        }
    }

    fn report(&self, title: &str, message: &str) {
        let dialog = adw::AlertDialog::new(Some(title), Some(message));
        dialog.add_response("ok", "OK");
        if let Some(root) = self.root.root().and_downcast::<gtk::Window>() {
            dialog.present(Some(&root));
        }
    }
}

/// Whether text still carries git's conflict markers.
///
/// Anchored to the line start and requiring exactly seven characters, which is
/// what git writes. A loose `contains("<<<<<<<")` would fire on a file that
/// discusses conflict markers — documentation about merging, for instance.
pub fn has_conflict_markers(text: &str) -> bool {
    text.lines().any(|l| {
        (l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>"))
            || (l == "=======" || l.starts_with("======= "))
    })
}

fn read_only_pane() -> (gtk::TextView, gtk::TextBuffer) {
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_monospace(true);
    view.set_cursor_visible(false);
    let buf = view.buffer();
    (view, buf)
}

fn titled_pane(title: &str, view: &gtk::TextView) -> gtk::Widget {
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label.set_margin_start(6);
    label.set_margin_top(4);
    label.set_margin_bottom(4);

    let scroll = gtk::ScrolledWindow::builder()
        .child(view)
        .vexpand(true)
        .hexpand(true)
        .build();

    let boxed = gtk::Box::new(gtk::Orientation::Vertical, 0);
    boxed.append(&label);
    boxed.append(&scroll);
    boxed.upcast()
}

#[allow(clippy::too_many_arguments)]
fn build_layout(
    sides: &gtk::Box,
    file_list: &gtk::ListBox,
    base_frame: &gtk::Widget,
    ours: &gtk::TextView,
    theirs: &gtk::TextView,
    merged: &gtk::TextView,
    status: &gtk::Label,
    ours_btn: &gtk::Button,
    theirs_btn: &gtk::Button,
    save_btn: &gtk::Button,
    finish_btn: &gtk::Button,
    abort_btn: &gtk::Button,
) -> gtk::Widget {
    // Ours and theirs side by side above, the editable merge below: the three
    // are read top-to-bottom in the order the work happens.
    sides.append(base_frame);
    sides.append(&titled_pane("Ours (current branch)", ours));
    sides.append(&titled_pane("Theirs (incoming)", theirs));

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_margin_start(6);
    actions.set_margin_end(6);
    actions.append(ours_btn);
    actions.append(theirs_btn);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&spacer);
    actions.append(save_btn);

    let merged_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let merged_label = gtk::Label::new(Some("Merged result (editable)"));
    merged_label.set_xalign(0.0);
    merged_label.add_css_class("heading");
    merged_label.set_margin_start(6);
    merged_box.append(&merged_label);
    merged_box.append(
        &gtk::ScrolledWindow::builder()
            .child(merged)
            .vexpand(true)
            .build(),
    );
    merged_box.append(&actions);

    let panes = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(sides)
        .end_child(&merged_box)
        .shrink_start_child(true)
        .shrink_end_child(true)
        .build();

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_margin_start(12);
    footer.set_margin_end(12);
    footer.set_margin_top(6);
    footer.set_margin_bottom(6);
    status.set_hexpand(true);
    // Ellipsize, or a long status pushes Abort and Commit past the window edge.
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    footer.append(status);
    footer.append(abort_btn);
    footer.append(finish_btn);

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&panes);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&footer);

    let list_scroll = gtk::ScrolledWindow::builder()
        .child(file_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(200)
        .build();

    let split = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&list_scroll)
        .end_child(&right)
        .resize_start_child(false)
        .shrink_start_child(true)
        .position(220)
        .build();

    split.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_markers_git_writes() {
        let conflicted = "\
line one
<<<<<<< HEAD
ours
=======
theirs
>>>>>>> other-branch
line two
";
        assert!(has_conflict_markers(conflicted));
    }

    #[test]
    fn clean_text_has_no_markers() {
        assert!(!has_conflict_markers("one\ntwo\nthree\n"));
        assert!(!has_conflict_markers(""));
    }

    #[test]
    fn prose_about_conflict_markers_is_not_a_conflict() {
        // A README explaining merge conflicts must not be rejected as one.
        let doc = "\
When a merge conflicts, git writes markers into the file.
The section between the ======= divider shows both sides.
Search for a line of seven < characters to find them.
";
        assert!(
            !has_conflict_markers(doc),
            "markers are anchored to the line start; prose mentioning them is not a conflict"
        );
    }

    #[test]
    fn a_divider_must_be_the_whole_line() {
        // git writes exactly "=======" on its own line.
        assert!(has_conflict_markers("a\n=======\nb\n"));
        // A row of equals signs used as a heading underline is not a conflict.
        assert!(!has_conflict_markers("Heading\n===========\nbody\n"));
    }

    #[test]
    fn markers_must_start_the_line() {
        assert!(!has_conflict_markers(
            "  <<<<<<< indented is not git's output"
        ));
        assert!(has_conflict_markers("<<<<<<< HEAD"));
    }
}
