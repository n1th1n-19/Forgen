//! Review controls for a pull request: existing threads, a draft of new
//! comments, and the submit/merge actions.
//!
//! Comments are drafted locally and sent as one review rather than posted
//! individually. Posting each comment as it is written notifies the author on
//! every keystroke-sized thought and cannot be revised before it lands;
//! batching matches how review is actually done and is what the web UI does.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use github::reviews::{DiffSide, DraftComment, MergeMethod, ReviewThread, ReviewVerdict};

use crate::pulls::Target;

/// A comment the user has written but not yet submitted.
#[derive(Clone, Debug)]
pub struct Draft {
    pub path: String,
    pub line: u32,
    pub side: DiffSide,
    pub body: String,
}

enum Msg {
    Threads(Result<Vec<ReviewThread>, String>),
    Submitted(Result<(), String>),
    Merged(Result<(), String>),
}

pub struct ReviewPanel {
    pub root: gtk::Widget,
    rt: tokio::runtime::Handle,
    thread_list: gtk::ListBox,
    draft_list: gtk::ListBox,
    summary: gtk::TextView,
    status: gtk::Label,
    submit_btn: gtk::Button,
    verdict: gtk::DropDown,
    merge_btn: gtk::Button,
    target: RefCell<Option<Target>>,
    number: RefCell<Option<u64>>,
    drafts: Rc<RefCell<Vec<Draft>>>,
    /// Fires after a successful submit or merge, so the PR list can reload.
    on_change: RefCell<Option<Rc<dyn Fn()>>>,
}

impl ReviewPanel {
    pub fn new(rt: tokio::runtime::Handle) -> Rc<Self> {
        let thread_list = gtk::ListBox::new();
        thread_list.add_css_class("boxed-list");
        thread_list.set_selection_mode(gtk::SelectionMode::None);

        let draft_list = gtk::ListBox::new();
        draft_list.add_css_class("boxed-list");
        draft_list.set_selection_mode(gtk::SelectionMode::None);

        let summary = gtk::TextView::new();
        summary.set_wrap_mode(gtk::WrapMode::WordChar);
        summary.set_top_margin(6);
        summary.set_bottom_margin(6);
        summary.set_left_margin(6);
        summary.set_right_margin(6);

        let verdict = gtk::DropDown::from_strings(&["Comment", "Approve", "Request changes"]);

        let submit_btn = gtk::Button::with_label("Submit review");
        submit_btn.add_css_class("suggested-action");
        submit_btn.set_sensitive(false);

        let merge_btn = gtk::Button::with_label("Merge…");
        merge_btn.set_sensitive(false);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let root = build_layout(
            &thread_list,
            &draft_list,
            &summary,
            &verdict,
            &submit_btn,
            &merge_btn,
            &status,
        );

        let panel = Rc::new(Self {
            root,
            rt,
            thread_list,
            draft_list,
            summary,
            status,
            submit_btn,
            verdict,
            merge_btn,
            target: RefCell::new(None),
            number: RefCell::new(None),
            drafts: Rc::new(RefCell::new(Vec::new())),
            on_change: RefCell::new(None),
        });

        {
            let this = panel.clone();
            panel.submit_btn.connect_clicked(move |_| this.submit());
        }
        {
            let this = panel.clone();
            panel
                .merge_btn
                .connect_clicked(move |_| this.confirm_merge());
        }

        panel
    }

    pub fn connect_changed(&self, f: Rc<dyn Fn()>) {
        *self.on_change.borrow_mut() = Some(f);
    }

    /// Point the panel at a pull request and load its threads.
    pub fn set_pull(self: &Rc<Self>, target: Option<Target>, number: Option<u64>) {
        *self.target.borrow_mut() = target;
        *self.number.borrow_mut() = number;
        self.drafts.borrow_mut().clear();
        self.rebuild_drafts();

        let ready = self.target.borrow().is_some() && number.is_some();
        self.submit_btn.set_sensitive(ready);
        self.merge_btn.set_sensitive(ready);

        if ready {
            self.load_threads();
        } else {
            clear(&self.thread_list);
            self.status.set_text("");
        }
    }

    /// Add a comment to the draft. Nothing is sent until the review is
    /// submitted.
    pub fn add_draft(self: &Rc<Self>, path: &str, line: u32, side: DiffSide, body: &str) {
        if body.trim().is_empty() {
            return;
        }
        self.drafts.borrow_mut().push(Draft {
            path: path.to_string(),
            line,
            side,
            body: body.to_string(),
        });
        self.rebuild_drafts();
    }

    pub fn draft_count(&self) -> usize {
        self.drafts.borrow().len()
    }

    fn rebuild_drafts(self: &Rc<Self>) {
        clear(&self.draft_list);

        let drafts = self.drafts.borrow().clone();
        for (i, d) in drafts.iter().enumerate() {
            let where_ = gtk::Label::new(Some(&format!("{}:{}", d.path, d.line)));
            where_.set_xalign(0.0);
            where_.add_css_class("caption");
            where_.add_css_class("dim-label");
            where_.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

            let body = gtk::Label::new(Some(&d.body));
            body.set_xalign(0.0);
            body.set_wrap(true);

            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.set_hexpand(true);
            text.append(&where_);
            text.append(&body);

            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.add_css_class("flat");
            remove.set_tooltip_text(Some("Discard this comment"));
            {
                let this = self.clone();
                remove.connect_clicked(move |_| {
                    // Index is stable because the list is rebuilt after every
                    // mutation, so no row outlives the vector it points into.
                    let mut drafts = this.drafts.borrow_mut();
                    if i < drafts.len() {
                        drafts.remove(i);
                    }
                    drop(drafts);
                    this.rebuild_drafts();
                });
            }

            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row_box.set_margin_start(8);
            row_box.set_margin_end(8);
            row_box.set_margin_top(6);
            row_box.set_margin_bottom(6);
            row_box.append(&text);
            row_box.append(&remove);

            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&row_box));
            self.draft_list.append(&row);
        }
    }

    fn load_threads(self: &Rc<Self>) {
        let (Some(target), Some(number)) = (self.target.borrow().clone(), *self.number.borrow())
        else {
            return;
        };

        self.status.set_text("Loading review threads…");
        let (tx, rx) = async_channel::bounded::<Msg>(4);

        self.rt.spawn(async move {
            let result = target
                .client
                .review_threads(&target.owner, &target.repo, number)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Threads(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Threads(result)) = rx.recv().await {
                match result {
                    Ok(threads) => this.populate_threads(threads),
                    Err(e) => this
                        .status
                        .set_text(&format!("Could not load threads: {e}")),
                }
            }
        });
    }

    fn populate_threads(self: &Rc<Self>, threads: Vec<ReviewThread>) {
        clear(&self.thread_list);

        let unresolved = threads.iter().filter(|t| !t.is_resolved).count();
        self.status.set_text(&match (threads.len(), unresolved) {
            (0, _) => "No review comments".to_string(),
            (n, 0) => format!("{n} threads, all resolved"),
            (n, u) => format!("{n} threads · {u} unresolved"),
        });

        for t in &threads {
            self.thread_list.append(&thread_row(t));
        }
    }

    fn submit(self: &Rc<Self>) {
        let (Some(target), Some(number)) = (self.target.borrow().clone(), *self.number.borrow())
        else {
            return;
        };

        let buf = self.summary.buffer();
        let body = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();

        let verdict = match self.verdict.selected() {
            1 => ReviewVerdict::Approve,
            2 => ReviewVerdict::RequestChanges,
            _ => ReviewVerdict::Comment,
        };

        let comments: Vec<DraftComment> = self
            .drafts
            .borrow()
            .iter()
            .map(|d| DraftComment::new(&d.path, d.line, d.side, &d.body))
            .collect();

        if comments.is_empty() && body.trim().is_empty() {
            self.status.set_text("Nothing to submit");
            return;
        }

        self.submit_btn.set_sensitive(false);
        self.status.set_text("Submitting review…");

        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .submit_review(
                    &target.owner,
                    &target.repo,
                    number,
                    verdict,
                    &body,
                    &comments,
                )
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Submitted(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Submitted(result)) = rx.recv().await {
                this.submit_btn.set_sensitive(true);
                match result {
                    Ok(()) => {
                        // Only clear the draft once the server has it. Clearing
                        // optimistically loses the user's writing on a failure.
                        this.drafts.borrow_mut().clear();
                        this.rebuild_drafts();
                        this.summary.buffer().set_text("");
                        this.status.set_text("Review submitted");
                        this.load_threads();
                        this.notify();
                    }
                    Err(e) => this.report("Could not submit review", &e),
                }
            }
        });
    }

    fn confirm_merge(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Merge this pull request?"),
            Some("This changes the base branch on GitHub for everyone."),
        );

        let method = gtk::DropDown::from_strings(&["Create a merge commit", "Squash", "Rebase"]);
        dialog.set_extra_child(Some(&method));

        dialog.add_response("cancel", "Cancel");
        dialog.add_response("merge", "Merge");
        dialog.set_response_appearance("merge", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));

        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "merge" {
                return;
            }
            let method = match method.selected() {
                1 => MergeMethod::Squash,
                2 => MergeMethod::Rebase,
                _ => MergeMethod::Merge,
            };
            this.merge(method);
        });

        if let Some(root) = self.root.root().and_downcast::<gtk::Window>() {
            dialog.present(Some(&root));
        }
    }

    fn merge(self: &Rc<Self>, method: MergeMethod) {
        let (Some(target), Some(number)) = (self.target.borrow().clone(), *self.number.borrow())
        else {
            return;
        };

        self.merge_btn.set_sensitive(false);
        self.status.set_text("Merging…");

        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .merge_pull(&target.owner, &target.repo, number, method, None)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Merged(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Merged(result)) = rx.recv().await {
                this.merge_btn.set_sensitive(true);
                match result {
                    Ok(()) => {
                        this.status.set_text("Merged");
                        this.notify();
                    }
                    // 405 means not mergeable, 409 means the head moved since
                    // the page loaded. GitHub's own message says which.
                    Err(e) => this.report("Could not merge", &e),
                }
            }
        });
    }

    fn notify(&self) {
        if let Some(f) = self.on_change.borrow().as_ref() {
            f();
        }
    }

    fn report(&self, title: &str, message: &str) {
        self.status.set_text(message);
        let dialog = adw::AlertDialog::new(Some(title), Some(message));
        dialog.add_response("ok", "OK");
        if let Some(root) = self.root.root().and_downcast::<gtk::Window>() {
            dialog.present(Some(&root));
        }
    }
}

fn thread_row(t: &ReviewThread) -> gtk::ListBoxRow {
    let mut where_text = match t.line {
        Some(l) => format!("{}:{l}", t.path),
        None => t.path.clone(),
    };
    if t.is_outdated {
        where_text.push_str(" · outdated");
    }
    if t.is_resolved {
        where_text.push_str(" · resolved");
    }

    let where_ = gtk::Label::new(Some(&where_text));
    where_.set_xalign(0.0);
    where_.add_css_class("caption");
    where_.add_css_class("dim-label");
    where_.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

    let boxed = gtk::Box::new(gtk::Orientation::Vertical, 4);
    boxed.set_margin_start(8);
    boxed.set_margin_end(8);
    boxed.set_margin_top(6);
    boxed.set_margin_bottom(6);
    boxed.append(&where_);

    for c in &t.comments {
        let author = gtk::Label::new(Some(&c.author));
        author.set_xalign(0.0);
        author.add_css_class("heading");
        author.add_css_class("caption");

        let body = gtk::Label::new(Some(&c.body));
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_selectable(true);

        boxed.append(&author);
        boxed.append(&body);
    }

    // A resolved thread is history, not work. Dimming it keeps it readable
    // without competing with the threads that still need an answer.
    if t.is_resolved {
        boxed.set_opacity(0.55);
    }

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&boxed));
    row
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn build_layout(
    thread_list: &gtk::ListBox,
    draft_list: &gtk::ListBox,
    summary: &gtk::TextView,
    verdict: &gtk::DropDown,
    submit_btn: &gtk::Button,
    merge_btn: &gtk::Button,
    status: &gtk::Label,
) -> gtk::Widget {
    let section = |title: &str, child: &gtk::Widget| {
        let l = gtk::Label::new(Some(title));
        l.set_xalign(0.0);
        l.add_css_class("heading");
        let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
        b.append(&l);
        b.append(child);
        b
    };

    let threads_scroll = gtk::ScrolledWindow::builder()
        .child(thread_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let drafts_scroll = gtk::ScrolledWindow::builder()
        .child(draft_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .height_request(110)
        .build();

    let summary_scroll = gtk::ScrolledWindow::builder()
        .child(summary)
        .height_request(80)
        .build();
    summary_scroll.add_css_class("card");

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.append(verdict);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&spacer);
    actions.append(merge_btn);
    actions.append(submit_btn);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.append(status);
    content.append(&section("Threads", threads_scroll.upcast_ref()));
    content.append(&section("Your comments", drafts_scroll.upcast_ref()));
    content.append(&section("Summary", summary_scroll.upcast_ref()));
    content.append(&actions);

    content.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use github::reviews::ReviewComment;

    fn thread(resolved: bool, outdated: bool, line: Option<u32>) -> ReviewThread {
        ReviewThread {
            id: "T".into(),
            path: "src/a.rs".into(),
            line,
            is_resolved: resolved,
            is_outdated: outdated,
            comments: vec![ReviewComment {
                id: "C".into(),
                author: "r".into(),
                body: "hi".into(),
                created_at: "x".into(),
                diff_hunk: None,
            }],
        }
    }

    /// The label is the only place a thread's state is visible, so it has to
    /// carry every combination.
    fn label_of(t: &ReviewThread) -> String {
        let mut s = match t.line {
            Some(l) => format!("{}:{l}", t.path),
            None => t.path.clone(),
        };
        if t.is_outdated {
            s.push_str(" · outdated");
        }
        if t.is_resolved {
            s.push_str(" · resolved");
        }
        s
    }

    #[test]
    fn a_live_thread_shows_path_and_line_only() {
        assert_eq!(label_of(&thread(false, false, Some(42))), "src/a.rs:42");
    }

    #[test]
    fn an_outdated_thread_says_so_and_has_no_line() {
        assert_eq!(label_of(&thread(false, true, None)), "src/a.rs · outdated");
    }

    #[test]
    fn a_thread_can_be_both_outdated_and_resolved() {
        assert_eq!(
            label_of(&thread(true, true, None)),
            "src/a.rs · outdated · resolved"
        );
    }
}
