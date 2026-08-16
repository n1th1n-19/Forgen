//! Stash management, as a dialog rather than a page.
//!
//! Stashing is punctuation, not a place you work: you stash to get somewhere
//! else, and the next thing you do is in the Changes or History view. A
//! permanent tab would sit empty almost always. A dialog opened from the header
//! matches how the feature is actually used.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use git::stash::{self, Stash};

use crate::diff_view::DiffView;
use crate::state::AppState;

pub struct StashDialog {
    window: adw::Window,
    state: AppState,
    list: gtk::ListBox,
    diff: Rc<DiffView>,
    empty: gtk::Label,
    apply_btn: gtk::Button,
    pop_btn: gtk::Button,
    drop_btn: gtk::Button,
    selected: Rc<RefCell<Option<Stash>>>,
    on_change: Rc<dyn Fn()>,
}

impl StashDialog {
    /// Build and present the dialog. `on_change` fires whenever the stash stack
    /// or working tree changes, so the caller can refresh its own views.
    pub fn present(parent: &impl IsA<gtk::Window>, state: AppState, on_change: Rc<dyn Fn()>) {
        let window = adw::Window::builder()
            .transient_for(parent)
            .modal(true)
            .title("Stashes")
            .default_width(820)
            .default_height(560)
            .build();

        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);

        let diff = DiffView::new();

        let empty = gtk::Label::new(Some("No stashes"));
        empty.add_css_class("dim-label");
        empty.set_vexpand(true);

        let apply_btn = gtk::Button::with_label("Apply");
        apply_btn.set_tooltip_text(Some("Apply and keep the stash"));
        let pop_btn = gtk::Button::with_label("Pop");
        pop_btn.set_tooltip_text(Some("Apply and remove the stash"));
        pop_btn.add_css_class("suggested-action");
        let drop_btn = gtk::Button::with_label("Drop");
        drop_btn.add_css_class("destructive-action");

        for b in [&apply_btn, &pop_btn, &drop_btn] {
            b.set_sensitive(false);
        }

        let dialog = Rc::new(Self {
            window: window.clone(),
            state,
            list: list.clone(),
            diff: diff.clone(),
            empty: empty.clone(),
            apply_btn: apply_btn.clone(),
            pop_btn: pop_btn.clone(),
            drop_btn: drop_btn.clone(),
            selected: Rc::new(RefCell::new(None)),
            on_change,
        });

        let stash_btn = gtk::Button::with_label("Stash changes…");
        {
            let this = dialog.clone();
            stash_btn.connect_clicked(move |_| this.prompt_new());
        }
        {
            let this = dialog.clone();
            apply_btn.connect_clicked(move |_| this.act(Action::Apply));
        }
        {
            let this = dialog.clone();
            pop_btn.connect_clicked(move |_| this.act(Action::Pop));
        }
        {
            let this = dialog.clone();
            drop_btn.connect_clicked(move |_| this.confirm_drop());
        }

        window.set_content(Some(&build_layout(
            &list, &diff.root, &empty, &stash_btn, &apply_btn, &pop_btn, &drop_btn,
        )));

        dialog.refresh();
        window.present();
    }

    fn refresh(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let stashes = self
            .state
            .with(|s| stash::list(&s.repo))
            .and_then(Result::ok)
            .unwrap_or_default();

        self.empty.set_visible(stashes.is_empty());
        if stashes.is_empty() {
            *self.selected.borrow_mut() = None;
            self.diff.clear();
            for b in [&self.apply_btn, &self.pop_btn, &self.drop_btn] {
                b.set_sensitive(false);
            }
            return;
        }

        for entry in &stashes {
            let title = gtk::Label::new(Some(&entry.message));
            title.set_xalign(0.0);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);

            let subtitle = gtk::Label::new(Some(&match &entry.branch {
                Some(b) => format!("{} · on {b}", entry.name),
                None => entry.name.clone(),
            }));
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
            self.list.append(&row);

            let this = self.clone();
            let entry = entry.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| this.show(&entry));
            row.add_controller(click);
        }

        // Show the newest by default: the one just stashed is the one being
        // looked for almost every time this dialog is opened.
        let keep = self.selected.borrow().clone();
        match keep.and_then(|k| stashes.iter().find(|s| s.name == k.name).cloned()) {
            Some(still_there) => self.show(&still_there),
            None => self.show(&stashes[0]),
        }
    }

    fn show(self: &Rc<Self>, entry: &Stash) {
        *self.selected.borrow_mut() = Some(entry.clone());
        for b in [&self.apply_btn, &self.pop_btn, &self.drop_btn] {
            b.set_sensitive(true);
        }

        match self.state.with(|s| stash::show(&s.repo, &entry.name)) {
            Some(Ok(files)) if !files.is_empty() => self.diff.show(&files[0]),
            _ => self.diff.clear(),
        }
    }

    fn act(self: &Rc<Self>, action: Action) {
        let Some(entry) = self.selected.borrow().clone() else {
            return;
        };

        let result = self.state.with(|s| match action {
            Action::Apply => stash::apply(&s.repo, &entry.name),
            Action::Pop => stash::pop(&s.repo, &entry.name),
            Action::Drop => stash::drop(&s.repo, &entry.name),
        });

        match result {
            Some(Err(e)) => {
                // Applying onto a dirty tree conflicts, and git's message says
                // which path — worth showing verbatim.
                self.report("Stash operation failed", &e.to_string());
                return;
            }
            None => {
                self.report("Stash operation failed", "No repository open");
                return;
            }
            Some(Ok(())) => {}
        }

        // A pop or drop removes the entry, so the remembered selection is stale.
        if action != Action::Apply {
            *self.selected.borrow_mut() = None;
        }
        self.refresh();
        (self.on_change)();
    }

    fn confirm_drop(self: &Rc<Self>) {
        let Some(entry) = self.selected.borrow().clone() else {
            return;
        };
        let dialog = adw::AlertDialog::new(
            Some("Drop this stash?"),
            Some(&format!(
                "\"{}\" will be discarded. Git keeps dropped stashes in the \
                 reflog for a while, but nothing in this interface can bring \
                 one back.",
                entry.message
            )),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("drop", "Drop");
        dialog.set_response_appearance("drop", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));

        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "drop" {
                this.act(Action::Drop);
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Ask for a message and options, then stash.
    fn prompt_new(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(Some("Stash changes"), None);

        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("Message (optional)"));

        // Untracked files default to *included*. Excluding them is git's
        // default and a reliable way to believe your work is stashed while a
        // new file sits in the tree and follows you to the next branch.
        let untracked = gtk::CheckButton::with_label("Include untracked files");
        untracked.set_active(true);
        let keep_index = gtk::CheckButton::with_label("Keep staged changes in the index");

        let boxed = gtk::Box::new(gtk::Orientation::Vertical, 8);
        boxed.append(&entry);
        boxed.append(&untracked);
        boxed.append(&keep_index);
        dialog.set_extra_child(Some(&boxed));

        dialog.add_response("cancel", "Cancel");
        dialog.add_response("stash", "Stash");
        dialog.set_response_appearance("stash", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("stash"));

        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "stash" {
                return;
            }
            let message = entry.text().to_string();
            let message = (!message.trim().is_empty()).then_some(message);

            match this.state.with(|s| {
                stash::push(
                    &s.repo,
                    message.as_deref(),
                    untracked.is_active(),
                    keep_index.is_active(),
                )
            }) {
                Some(Err(e)) => this.report("Could not stash", &e.to_string()),
                None => this.report("Could not stash", "No repository open"),
                Some(Ok(())) => {
                    *this.selected.borrow_mut() = None;
                    this.refresh();
                    (this.on_change)();
                }
            }
        });

        dialog.present(Some(&self.window));
    }

    fn report(&self, title: &str, message: &str) {
        let dialog = adw::AlertDialog::new(Some(title), Some(message));
        dialog.add_response("ok", "OK");
        dialog.present(Some(&self.window));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Apply,
    Pop,
    Drop,
}

fn build_layout(
    list: &gtk::ListBox,
    diff_root: &gtk::Widget,
    empty: &gtk::Label,
    stash_btn: &gtk::Button,
    apply_btn: &gtk::Button,
    pop_btn: &gtk::Button,
    drop_btn: &gtk::Button,
) -> gtk::Widget {
    let header = adw::HeaderBar::new();
    header.pack_start(stash_btn);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.append(drop_btn);
    actions.append(apply_btn);
    actions.append(pop_btn);
    header.pack_end(&actions);

    let list_scroll = gtk::ScrolledWindow::builder()
        .child(list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(240)
        .build();

    // The empty label overlays the list rather than replacing it, so the layout
    // does not jump as the last stash is dropped.
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&list_scroll));
    overlay.add_overlay(empty);

    let split = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&overlay)
        .end_child(diff_root)
        .resize_start_child(false)
        .shrink_start_child(true)
        .shrink_end_child(true)
        .position(260)
        .build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&split));
    view.upcast()
}
