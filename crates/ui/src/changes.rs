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
use sourceview::prelude::*;

use git::diff::{self, DiffSource, FileDiff};
use git::status::{self, StatusEntry};
use git::{commit, stage};

use crate::state::AppState;

/// Which side of the index a row belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Staged,
    Unstaged,
}

pub struct ChangesView {
    pub root: gtk::Widget,
    state: AppState,
    staged_list: gtk::ListBox,
    unstaged_list: gtk::ListBox,
    diff_buffer: sourceview::Buffer,
    diff_title: gtk::Label,
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

        // The `diff` language ships with GtkSourceView, so hunk headers,
        // additions and removals are coloured by the user's chosen scheme
        // rather than by colours hardcoded here that would fight their theme.
        let diff_buffer = sourceview::Buffer::new(None);
        if let Some(lang) = sourceview::LanguageManager::default().language("diff") {
            diff_buffer.set_language(Some(&lang));
        }
        diff_buffer.set_highlight_syntax(true);

        // GtkSourceView does not follow libadwaita's dark mode: its default
        // scheme is light, so the diff pane renders as a white slab inside a
        // dark window. The scheme has to be selected explicitly and re-selected
        // whenever the system preference flips.
        apply_style_scheme(&diff_buffer);
        {
            let buffer = diff_buffer.clone();
            adw::StyleManager::default().connect_dark_notify(move |_| {
                apply_style_scheme(&buffer);
            });
        }

        let diff_view = sourceview::View::with_buffer(&diff_buffer);
        diff_view.set_editable(false);
        diff_view.set_monospace(true);
        diff_view.set_show_line_numbers(false);
        diff_view.set_cursor_visible(false);

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

        let view = Rc::new(Self {
            root: build_layout(
                &staged_list,
                &unstaged_list,
                &diff_view,
                &diff_title,
                &message,
                &commit_btn,
                &amend,
                &summary,
            ),
            state,
            staged_list,
            unstaged_list,
            diff_buffer,
            diff_title,
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

        // Restore the diff pane if the file it showed still has changes.
        let shown = self.shown.borrow().clone();
        match shown {
            Some((side, path)) if staged.iter().chain(unstaged.iter()).any(|e| e.path == path) => {
                self.show_diff(side, &path);
            }
            _ => self.clear_diff(),
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

        let text = match diffs {
            Some(Ok(files)) if !files.is_empty() => render(&files[0]),
            // An untracked file has no diff against the index; show the file so
            // the pane is not mysteriously blank.
            Some(Ok(_)) => self
                .state
                .with(|s| {
                    s.repo
                        .workdir()
                        .map(|w| w.join(path))
                        .and_then(|p| std::fs::read_to_string(p).ok())
                })
                .flatten()
                .map(|c| {
                    c.lines()
                        .map(|l| format!("+{l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| "(no textual diff)".into()),
            Some(Err(e)) => format!("Could not read the diff:\n{e}"),
            None => String::new(),
        };

        self.diff_title.set_text(path);
        self.diff_buffer.set_text(&text);
    }

    fn clear_diff(&self) {
        *self.shown.borrow_mut() = None;
        self.diff_title.set_text("Select a file");
        self.diff_buffer.set_text("");
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

/// Render a parsed diff back to patch text for display.
///
/// Round-tripping through our own parser rather than showing git's raw output
/// means the viewer displays exactly what the staging code sees — if the parser
/// mangles something, it is visible here rather than only in a rejected patch.
fn render(file: &FileDiff) -> String {
    if file.is_binary {
        return format!("Binary file {} differs", file.new_path);
    }

    let mut out = String::new();
    for hunk in &file.hunks {
        out.push_str(&hunk.header());
        out.push('\n');
        for line in &hunk.lines {
            out.push_str(&line.to_patch_line());
            out.push('\n');
        }
    }
    if out.is_empty() {
        out.push_str("(no changes)");
    }
    out
}

/// Pick a source style scheme matching the current light/dark preference.
///
/// Names are tried in order because which schemes exist varies with the
/// GtkSourceView version — `Adwaita-dark` arrived in 5.6, and falling back to
/// `classic` is better than leaving a white pane in a dark window.
fn apply_style_scheme(buffer: &sourceview::Buffer) {
    let dark = adw::StyleManager::default().is_dark();
    let candidates: &[&str] = if dark {
        &["Adwaita-dark", "solarized-dark", "oblivion", "classic"]
    } else {
        &["Adwaita", "solarized-light", "classic"]
    };

    let manager = sourceview::StyleSchemeManager::default();
    for name in candidates {
        if let Some(scheme) = manager.scheme(name) {
            buffer.set_style_scheme(Some(&scheme));
            return;
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
    diff_view: &sourceview::View,
    diff_title: &gtk::Label,
    message: &gtk::TextView,
    commit_btn: &gtk::Button,
    amend: &gtk::CheckButton,
    summary: &gtk::Label,
) -> gtk::Widget {
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
    left.set_size_request(320, -1);

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

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(diff_title);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(
        &gtk::ScrolledWindow::builder()
            .child(diff_view)
            .vexpand(true)
            .hexpand(true)
            .build(),
    );

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&left)
        .end_child(&right)
        .position(360)
        .resize_start_child(false)
        .build();

    paned.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::diff;

    // `render` is pure, so it is testable without a display server — which is
    // the point of keeping the formatting out of the widget callbacks.

    #[test]
    fn render_round_trips_a_diff_back_to_patch_text() {
        let f = diff::parse(
            "\
diff --git a/a.txt b/a.txt
index 1..2 100644
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@ ctx
 one
-two
+TWO
 three
",
        )
        .remove(0);

        let out = render(&f);
        assert!(out.starts_with("@@ -1,3 +1,3 @@ ctx\n"), "{out}");
        assert!(out.contains("-two\n"));
        assert!(out.contains("+TWO\n"));
        assert!(
            out.contains(" one\n"),
            "context must keep its leading space"
        );
        assert!(
            !out.contains("diff --git"),
            "the file header is shown in the title bar, not the pane"
        );
    }

    #[test]
    fn render_names_binary_files_instead_of_showing_bytes() {
        let f = diff::parse(
            "\
diff --git a/x.png b/x.png
index 1..2 100644
Binary files a/x.png and b/x.png differ
",
        )
        .remove(0);
        assert_eq!(render(&f), "Binary file x.png differs");
    }

    #[test]
    fn render_says_so_when_there_is_nothing_to_show() {
        let f = FileDiff {
            old_path: "a".into(),
            new_path: "a".into(),
            header: vec![],
            hunks: vec![],
            is_binary: false,
        };
        assert_eq!(render(&f), "(no changes)");
    }
}
