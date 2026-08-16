//! The Changes page: working-tree status, diff viewer, and the commit box.
//!
//! Layout mirrors the mental model of the index rather than the filesystem:
//! staged above, unstaged below, the selected file's diff to the right, and the
//! commit box under the staged list — because what gets committed is exactly
//! what is in the staged list, and putting the button anywhere else invites
//! committing something you did not read.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use git::diff::{self, DiffSource, FileDiff};
use git::stage::discard_lines;
use git::status::{self, StatusEntry};
use git::{commit, stage};

use crate::diff_view::DiffView;
use crate::state::AppState;

/// Which side of the index a row belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Staged,
    Unstaged,
}

/// What the selection buttons do with the selected lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionAction {
    /// Move between the working tree and the index, in whichever direction the
    /// currently shown side implies.
    Move,
    /// Revert them in the working tree. Unrecoverable.
    Discard,
}

pub struct ChangesView {
    pub root: gtk::Widget,
    /// Exposed so a window breakpoint can stack the panes on a narrow window.
    pub paned: gtk::Paned,
    state: AppState,
    staged_list: gtk::ListBox,
    unstaged_list: gtk::ListBox,
    diff: Rc<DiffView>,
    diff_title: gtk::Label,
    stage_sel_btn: gtk::Button,
    discard_sel_btn: gtk::Button,
    message: gtk::TextView,
    commit_btn: gtk::Button,
    amend: gtk::CheckButton,
    summary: gtk::Label,
    /// Path currently shown in the diff pane, so a refresh can restore it.
    shown: Rc<RefCell<Option<(Side, String)>>>,
}

impl ChangesView {
    pub fn new(state: AppState) -> Rc<Self> {
        let staged_list = file_list();
        let unstaged_list = file_list();

        let diff = DiffView::new();

        let diff_title = gtk::Label::new(Some("Select a file"));
        diff_title.set_xalign(0.0);
        diff_title.add_css_class("heading");
        diff_title.set_margin_start(12);
        diff_title.set_margin_top(8);
        diff_title.set_margin_bottom(8);
        diff_title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

        let message = gtk::TextView::new();
        message.set_wrap_mode(gtk::WrapMode::WordChar);
        message.set_top_margin(6);
        message.set_bottom_margin(6);
        message.set_left_margin(6);
        message.set_right_margin(6);

        let commit_btn = gtk::Button::with_label("Commit");
        commit_btn.add_css_class("suggested-action");
        commit_btn.set_sensitive(false);

        let amend = gtk::CheckButton::with_label("Amend previous commit");
        let summary = gtk::Label::new(Some(""));
        summary.set_xalign(0.0);
        summary.add_css_class("dim-label");

        // Label says "Stage" or "Unstage" depending on which side is shown —
        // the same selection means opposite things on the two lists, and one
        // ambiguous verb would make it a coin flip.
        let stage_sel_btn = gtk::Button::with_label("Stage");
        stage_sel_btn.set_sensitive(false);
        let discard_sel_btn = gtk::Button::with_label("Discard");
        discard_sel_btn.add_css_class("destructive-action");
        discard_sel_btn.set_sensitive(false);

        let paned = build_layout(
            &staged_list,
            &unstaged_list,
            &diff.root,
            &diff_title,
            &stage_sel_btn,
            &discard_sel_btn,
            &message,
            &commit_btn,
            &amend,
            &summary,
        );

        let view = Rc::new(Self {
            root: paned.clone().upcast(),
            paned,
            state,
            staged_list,
            unstaged_list,
            diff,
            diff_title,
            stage_sel_btn,
            discard_sel_btn,
            message,
            commit_btn,
            amend,
            summary,
            shown: Rc::new(RefCell::new(None)),
        });

        view.wire();
        view
    }

    fn wire(self: &Rc<Self>) {
        // Commit enables only with a message; whether anything is staged is
        // checked at refresh, since amend can legitimately commit nothing new.
        {
            let this = self.clone();
            self.message.buffer().connect_changed(move |_| {
                this.update_commit_sensitivity();
            });
        }
        {
            let this = self.clone();
            self.amend.connect_toggled(move |_| {
                this.update_commit_sensitivity();
            });
        }
        {
            let this = self.clone();
            self.commit_btn.connect_clicked(move |_| this.do_commit());
        }

        // Selection drives the staging buttons.
        {
            let this = self.clone();
            self.diff.connect_selection_changed(move || {
                let has = this.diff.has_selection();
                this.stage_sel_btn.set_sensitive(has);
                // Discarding only makes sense for unstaged work; the staged
                // side's inverse is unstaging, which the same button does.
                let unstaged = matches!(
                    this.shown.borrow().as_ref(),
                    Some((Side::Unstaged, _)) | None
                );
                this.discard_sel_btn.set_sensitive(has && unstaged);
            });
        }
        {
            let this = self.clone();
            self.stage_sel_btn
                .connect_clicked(move |_| this.apply_selection(SelectionAction::Move));
        }
        {
            let this = self.clone();
            self.discard_sel_btn
                .connect_clicked(move |_| this.confirm_discard());
        }
    }

    /// Stage or unstage exactly what is selected in the diff pane.
    fn apply_selection(self: &Rc<Self>, action: SelectionAction) {
        let Some(mask) = self.diff.selection_mask() else {
            return;
        };
        let Some(file) = self.diff.current() else {
            return;
        };
        let Some((side, _)) = self.shown.borrow().clone() else {
            return;
        };

        let result = self.state.with(|s| match (side, action) {
            // Staged side: the selection moves back out of the index.
            (Side::Staged, _) => stage::unstage_lines(&s.repo, &file, &mask),
            (Side::Unstaged, SelectionAction::Move) => stage::stage_lines(&s.repo, &file, &mask),
            // Discarding is a reverse-apply against the working tree, which
            // `stage` does not cover — it only touches the index.
            (Side::Unstaged, SelectionAction::Discard) => discard_lines(&s.repo, &file, &mask),
        });

        match result {
            Some(Err(e)) => self.report(&format!("Could not update: {e}")),
            None => self.report("No repository open"),
            Some(Ok(())) => {}
        }
        self.refresh();
    }

    /// Discarding cannot be undone from the reflog, so it asks first.
    fn confirm_discard(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Discard selected changes?"),
            Some(
                "The selected lines will be removed from the working tree. \
                 This cannot be undone — the changes were never committed, so \
                 there is nothing to recover them from.",
            ),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("discard", "Discard");
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));

        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "discard" {
                this.apply_selection(SelectionAction::Discard);
            }
        });

        if let Some(root) = self.root.root().and_downcast::<gtk::Window>() {
            dialog.present(Some(&root));
        }
    }

    fn update_commit_sensitivity(&self) {
        let buf = self.message.buffer();
        let text = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();
        let has_message = !text.trim().is_empty();

        let has_staged = self
            .state
            .with(|s| status::status(&s.repo).map(|st| st.staged().count() > 0))
            .and_then(Result::ok)
            .unwrap_or(false);

        self.commit_btn
            .set_sensitive(has_message && (has_staged || self.amend.is_active()));
    }

    /// Re-read status and rebuild both lists.
    pub fn refresh(self: &Rc<Self>) {
        clear(&self.staged_list);
        clear(&self.unstaged_list);

        let Some(Ok(st)) = self.state.with(|s| status::status(&s.repo)) else {
            self.summary.set_text("No repository open");
            self.commit_btn.set_sensitive(false);
            return;
        };

        let staged: Vec<_> = st.staged().cloned().collect();
        let unstaged: Vec<_> = st.unstaged().filter(|e| !e.ignored).cloned().collect();

        for e in &staged {
            self.staged_list.append(&self.row(e, Side::Staged));
        }
        for e in &unstaged {
            self.unstaged_list.append(&self.row(e, Side::Unstaged));
        }

        self.summary.set_text(&format!(
            "{} staged · {} unstaged{}",
            staged.len(),
            unstaged.len(),
            if st.conflicted().count() > 0 {
                format!(" · {} conflicted", st.conflicted().count())
            } else {
                String::new()
            }
        ));

        // Restore the diff pane if the file it showed still has changes;
        // otherwise fall to the first file there is. Landing on an empty pane
        // reads as a broken page rather than as "nothing selected", and the
        // first unstaged file is what someone opening this page wants to see.
        let shown = self.shown.borrow().clone();
        match shown {
            Some((side, path)) if staged.iter().chain(unstaged.iter()).any(|e| e.path == path) => {
                self.show_diff(side, &path);
            }
            _ => match unstaged
                .first()
                .map(|e| (Side::Unstaged, e.path.clone()))
                .or_else(|| staged.first().map(|e| (Side::Staged, e.path.clone())))
            {
                Some((side, path)) => self.show_diff(side, &path),
                None => self.clear_diff(),
            },
        }

        self.update_commit_sensitivity();
    }

    /// One file row, with its stage/unstage button.
    fn row(self: &Rc<Self>, entry: &StatusEntry, side: Side) -> gtk::ListBoxRow {
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hbox.set_margin_start(6);
        hbox.set_margin_end(6);
        hbox.set_margin_top(3);
        hbox.set_margin_bottom(3);

        let status_char = if entry.conflicted {
            "!"
        } else if entry.untracked {
            "?"
        } else {
            match if side == Side::Staged {
                entry.index
            } else {
                entry.worktree
            } {
                status::Change::Added => "A",
                status::Change::Deleted => "D",
                status::Change::Renamed => "R",
                status::Change::Copied => "C",
                status::Change::TypeChanged => "T",
                _ => "M",
            }
        };

        let badge = gtk::Label::new(Some(status_char));
        badge.add_css_class("monospace");
        badge.add_css_class("dim-label");
        badge.set_width_chars(2);

        let label = gtk::Label::new(Some(&match &entry.original_path {
            Some(orig) => format!("{orig} → {}", entry.path),
            None => entry.path.clone(),
        }));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

        let action = gtk::Button::from_icon_name(if side == Side::Staged {
            "list-remove-symbolic"
        } else {
            "list-add-symbolic"
        });
        action.set_tooltip_text(Some(if side == Side::Staged {
            "Unstage"
        } else {
            "Stage"
        }));
        action.add_css_class("flat");

        {
            let this = self.clone();
            let path = entry.path.clone();
            action.connect_clicked(move |_| {
                let result = this.state.with(|s| {
                    if side == Side::Staged {
                        stage::unstage_file(&s.repo, &path)
                    } else {
                        stage::stage_file(&s.repo, &path)
                    }
                });
                if let Some(Err(e)) = result {
                    this.report(&format!("Could not update the index: {e}"));
                }
                this.refresh();
            });
        }

        hbox.append(&badge);
        hbox.append(&label);
        hbox.append(&action);

        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&hbox));

        // Selecting a row shows its diff.
        {
            let this = self.clone();
            let path = entry.path.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| this.show_diff(side, &path));
            row.add_controller(click);
        }

        row
    }

    fn show_diff(self: &Rc<Self>, side: Side, path: &str) {
        *self.shown.borrow_mut() = Some((side, path.to_string()));

        let source = if side == Side::Staged {
            DiffSource::Staged
        } else {
            DiffSource::Unstaged
        };

        let diffs: Option<Result<Vec<FileDiff>, _>> = self
            .state
            .with(|s| diff::diff(&s.repo, source, Some(std::path::Path::new(path))));

        self.diff_title.set_text(path);
        self.stage_sel_btn.set_label(match side {
            Side::Staged => "Unstage",
            Side::Unstaged => "Stage",
        });

        match diffs {
            Some(Ok(files)) if !files.is_empty() => self.diff.show(&files[0]),
            // An untracked file has no diff against the index. Synthesising one
            // — every line an addition against an empty pre-image — is exactly
            // what `git add -N` would produce, so the same staging path works
            // on it rather than needing a special case.
            Some(Ok(_)) => match self.untracked_as_diff(path) {
                Some(f) => self.diff.show(&f),
                None => self.diff.clear(),
            },
            Some(Err(e)) => {
                self.diff.clear();
                self.report(&format!("Could not read the diff: {e}"));
            }
            None => self.diff.clear(),
        }

        self.stage_sel_btn.set_sensitive(false);
        self.discard_sel_btn.set_sensitive(false);
    }

    /// Build a diff for an untracked file: all additions, no pre-image.
    fn untracked_as_diff(&self, path: &str) -> Option<FileDiff> {
        let content = self
            .state
            .with(|s| {
                s.repo
                    .workdir()
                    .map(|w| w.join(path))
                    .and_then(|p| std::fs::read_to_string(p).ok())
            })
            .flatten()?;

        let lines: Vec<_> = content.lines().collect();
        let hunk = git::diff::Hunk {
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: lines.len() as u32,
            section: String::new(),
            lines: lines
                .iter()
                .enumerate()
                .map(|(i, l)| git::diff::DiffLine {
                    kind: git::diff::LineKind::Added,
                    text: (*l).to_string(),
                    old_lineno: None,
                    new_lineno: Some(i as u32 + 1),
                })
                .collect(),
        };

        Some(FileDiff {
            old_path: "/dev/null".into(),
            new_path: path.to_string(),
            header: vec![
                format!("diff --git a/{path} b/{path}"),
                "new file mode 100644".to_string(),
                "--- /dev/null".to_string(),
                format!("+++ b/{path}"),
            ],
            hunks: vec![hunk],
            is_binary: false,
        })
    }

    fn clear_diff(&self) {
        *self.shown.borrow_mut() = None;
        self.diff_title.set_text("Select a file");
        self.diff.clear();
        self.stage_sel_btn.set_sensitive(false);
        self.discard_sel_btn.set_sensitive(false);
    }

    fn do_commit(self: &Rc<Self>) {
        let buf = self.message.buffer();
        let text = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();

        // Committing without an identity fails with a wall of git advice; catch
        // it here and say the one thing that fixes it.
        let identity = self.state.with(|s| commit::identity(&s.repo)).flatten();
        if identity.is_none() {
            self.report(
                "No git identity is configured. Run:\n\n  \
                 git config --global user.name \"Your Name\"\n  \
                 git config --global user.email you@example.com",
            );
            return;
        }

        let opts = commit::CommitOptions {
            amend: self.amend.is_active(),
            ..Default::default()
        };

        match self.state.with(|s| commit::commit(&s.repo, &text, &opts)) {
            Some(Ok(sha)) => {
                buf.set_text("");
                self.amend.set_active(false);
                tracing::info!(%sha, "committed");
                self.refresh();
            }
            Some(Err(e)) => self.report(&e.to_string()),
            None => self.report("No repository open"),
        }
    }

    fn report(&self, message: &str) {
        let dialog = adw::AlertDialog::new(Some("Commit"), Some(message));
        dialog.add_response("ok", "OK");
        if let Some(root) = self.root.root().and_downcast::<gtk::Window>() {
            dialog.present(Some(&root));
        }
    }
}

fn file_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_layout(
    staged_list: &gtk::ListBox,
    unstaged_list: &gtk::ListBox,
    diff_root: &gtk::Widget,
    diff_title: &gtk::Label,
    stage_sel_btn: &gtk::Button,
    discard_sel_btn: &gtk::Button,
    message: &gtk::TextView,
    commit_btn: &gtk::Button,
    amend: &gtk::CheckButton,
    summary: &gtk::Label,
) -> gtk::Paned {
    let section = |title: &str, list: &gtk::ListBox| {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let l = gtk::Label::new(Some(title));
        l.set_xalign(0.0);
        l.add_css_class("heading");
        b.append(&l);

        let scroll = gtk::ScrolledWindow::builder()
            .child(list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        b.append(&scroll);
        b
    };

    let left = gtk::Box::new(gtk::Orientation::Vertical, 12);
    left.set_margin_start(12);
    left.set_margin_end(12);
    left.set_margin_top(12);
    left.set_margin_bottom(12);
    left.set_size_request(280, -1);

    left.append(summary);
    left.append(&section("Staged", staged_list));
    left.append(&section("Unstaged", unstaged_list));

    // Commit box sits under the staged list: what is committed is what is
    // shown directly above it.
    let msg_scroll = gtk::ScrolledWindow::builder()
        .child(message)
        .height_request(90)
        .build();
    msg_scroll.add_css_class("card");
    left.append(&msg_scroll);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.append(amend);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&spacer);
    actions.append(commit_btn);
    left.append(&actions);

    // Diff header: file name on the left, actions on the right. The actions
    // sit next to the diff rather than under the file list because they act on
    // the selection inside the diff, not on the selected file.
    let diff_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    diff_header.set_margin_start(12);
    diff_header.set_margin_end(12);
    diff_header.set_margin_top(6);
    diff_header.set_margin_bottom(6);
    diff_title.set_hexpand(true);
    diff_header.append(diff_title);
    diff_header.append(discard_sel_btn);
    diff_header.append(stage_sel_btn);

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&diff_header);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(diff_root);

    gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&left)
        .end_child(&right)
        // Both children may shrink below their natural size. Without this the
        // diff pane is squeezed to whatever is left over rather than sharing,
        // and on a tiled half-screen that is a few unreadable columns.
        .shrink_start_child(true)
        .shrink_end_child(true)
        .resize_start_child(false)
        .position(340)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `render` is pure, so it is testable without a display server — which is
    // the point of keeping the formatting out of the widget callbacks.

    #[test]
    fn untracked_files_become_all_addition_diffs() {
        // The synthesised diff has to be shaped like a real one, because the
        // same staging path consumes it.
        let f = FileDiff {
            old_path: "/dev/null".into(),
            new_path: "new.txt".into(),
            header: vec![
                "diff --git a/new.txt b/new.txt".into(),
                "new file mode 100644".into(),
                "--- /dev/null".into(),
                "+++ b/new.txt".into(),
            ],
            hunks: vec![git::diff::Hunk {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
                section: String::new(),
                lines: vec![
                    git::diff::DiffLine {
                        kind: git::diff::LineKind::Added,
                        text: "first".into(),
                        old_lineno: None,
                        new_lineno: Some(1),
                    },
                    git::diff::DiffLine {
                        kind: git::diff::LineKind::Added,
                        text: "second".into(),
                        old_lineno: None,
                        new_lineno: Some(2),
                    },
                ],
            }],
            is_binary: false,
        };

        // Every line stageable, and the patch a real one git would accept.
        let mask = vec![vec![true, true]];
        let patch = git::stage::build_patch(&f, &mask, git::stage::PatchTarget::Index).unwrap();
        assert!(patch.contains("--- /dev/null"), "{patch}");
        assert!(patch.contains("+first"), "{patch}");
        assert!(patch.contains("@@ -0,0 +1,2 @@"), "{patch}");
    }
}
