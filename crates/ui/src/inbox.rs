//! The notifications inbox.
//!
//! Account-wide rather than repository-scoped, which makes it the one page in
//! a repository-centric window that is not about the open repository. That is
//! the point: the reason to open a git client in the morning is usually
//! something someone else did.
//!
//! Triage is keyboard-first. A list of fifty notifications is only tractable
//! if clearing one costs a keystroke, so `e` marks read and `u` unsubscribes
//! without moving the hands.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use github::notifications::Notification;

use crate::pulls::Target;

enum Msg {
    Loaded(Result<(Vec<Notification>, Duration), String>),
    Acted(Result<String, String>),
}

pub struct InboxView {
    pub root: gtk::Widget,
    rt: tokio::runtime::Handle,
    list: gtk::ListBox,
    status: gtk::Label,
    spinner: gtk::Spinner,
    read_btn: gtk::Button,
    unsub_btn: gtk::Button,
    open_btn: gtk::Button,
    client: RefCell<Option<Target>>,
    items: Rc<RefCell<Vec<Notification>>>,
    selected: Cell<Option<usize>>,
    /// Handle of the running poll timer, so switching away stops it.
    poll: RefCell<Option<glib::SourceId>>,
}

impl InboxView {
    pub fn new(rt: tokio::runtime::Handle) -> Rc<Self> {
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::Single);

        let status = gtk::Label::new(Some("Not signed in"));
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let spinner = gtk::Spinner::new();

        let read_btn = gtk::Button::with_label("Mark read");
        read_btn.set_tooltip_text(Some("Mark read (e)"));
        let unsub_btn = gtk::Button::with_label("Unsubscribe");
        unsub_btn.set_tooltip_text(Some("Stop notifying about this thread (u)"));
        let open_btn = gtk::Button::from_icon_name("web-browser-symbolic");
        open_btn.set_tooltip_text(Some("Open on GitHub"));

        for b in [&read_btn, &unsub_btn] {
            b.set_sensitive(false);
        }
        open_btn.set_sensitive(false);

        let root = build_layout(&list, &status, &spinner, &read_btn, &unsub_btn, &open_btn);

        let view = Rc::new(Self {
            root,
            rt,
            list,
            status,
            spinner,
            read_btn,
            unsub_btn,
            open_btn,
            client: RefCell::new(None),
            items: Rc::new(RefCell::new(Vec::new())),
            selected: Cell::new(None),
            poll: RefCell::new(None),
        });

        view.wire();
        view
    }

    fn wire(self: &Rc<Self>) {
        {
            let this = self.clone();
            self.list.connect_row_selected(move |_, row| {
                this.selected.set(row.map(|r| r.index() as usize));
                let has = this.selected.get().is_some();
                this.read_btn.set_sensitive(has);
                this.unsub_btn.set_sensitive(has);
                this.open_btn.set_sensitive(has);
            });
        }
        {
            let this = self.clone();
            self.read_btn
                .connect_clicked(move |_| this.act(Action::Read));
        }
        {
            let this = self.clone();
            self.unsub_btn
                .connect_clicked(move |_| this.act(Action::Unsubscribe));
        }
        {
            let this = self.clone();
            self.open_btn.connect_clicked(move |_| this.open_web());
        }

        // Keyboard triage. Attached to the list rather than the window so the
        // keys are inert while the user is typing anywhere else.
        let keys = gtk::EventControllerKey::new();
        {
            let this = self.clone();
            keys.connect_key_pressed(move |_, key, _, _| match key {
                gtk::gdk::Key::e => {
                    this.act(Action::Read);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::u => {
                    this.act(Action::Unsubscribe);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            });
        }
        self.list.add_controller(keys);
    }

    pub fn set_client(self: &Rc<Self>, target: Option<Target>) {
        *self.client.borrow_mut() = target;
        self.items.borrow_mut().clear();
        clear(&self.list);
        self.status.set_text(if self.client.borrow().is_some() {
            "Loading…"
        } else {
            "Not signed in"
        });
    }

    pub fn has_client(&self) -> bool {
        self.client.borrow().is_some()
    }

    /// Load once and start polling.
    pub fn start(self: &Rc<Self>) {
        self.refresh();
    }

    /// Stop polling — called when the page is left, so a background tab is not
    /// spending rate limit on a view nobody is reading.
    pub fn stop(&self) {
        if let Some(id) = self.poll.borrow_mut().take() {
            id.remove();
        }
    }

    pub fn refresh(self: &Rc<Self>) {
        let Some(target) = self.client.borrow().clone() else {
            return;
        };

        self.spinner.start();
        let (tx, rx) = async_channel::bounded::<Msg>(4);

        self.rt.spawn(async move {
            let result = target
                .client
                .notifications(false)
                .await
                .map(|inbox| (inbox.notifications, inbox.poll_after))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Loaded(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Loaded(result)) = rx.recv().await {
                this.spinner.stop();
                match result {
                    Ok((items, after)) => {
                        this.populate(items);
                        this.schedule_poll(after);
                    }
                    Err(e) => this.status.set_text(&format!("Could not load: {e}")),
                }
            }
        });
    }

    /// Arm the next poll at the interval GitHub asked for.
    fn schedule_poll(self: &Rc<Self>, after: Duration) {
        self.stop();
        let this = self.clone();
        let id = glib::timeout_add_seconds_local(after.as_secs().max(60) as u32, move || {
            this.refresh();
            // One-shot: `refresh` arms the next one from the fresh header, so
            // a changed X-Poll-Interval takes effect immediately rather than
            // after the old interval expires.
            glib::ControlFlow::Break
        });
        *self.poll.borrow_mut() = Some(id);
    }

    fn populate(self: &Rc<Self>, mut items: Vec<Notification>) {
        clear(&self.list);

        // Things asked of this user first. A review request buried under fifty
        // watched-repository updates is a review that does not happen.
        items.sort_by_key(|n| !n.is_direct());

        let direct = items.iter().filter(|n| n.is_direct()).count();
        self.status.set_text(&match (items.len(), direct) {
            (0, _) => "Inbox empty".to_string(),
            (n, 0) => format!("{n} unread"),
            (n, d) => format!("{n} unread · {d} for you"),
        });

        for n in &items {
            self.list.append(&row(n));
        }
        *self.items.borrow_mut() = items;
        self.selected.set(None);
    }

    fn act(self: &Rc<Self>, action: Action) {
        let (Some(target), Some(index)) = (self.client.borrow().clone(), self.selected.get())
        else {
            return;
        };
        let Some(item) = self.items.borrow().get(index).cloned() else {
            return;
        };
        let id = item.id.clone();

        // Remove the row immediately. Triage is a rhythm, and waiting on a
        // round trip between each keystroke breaks it; a failure puts the row
        // back with the reason.
        self.items.borrow_mut().remove(index);
        if let Some(row) = self.list.row_at_index(index as i32) {
            self.list.remove(&row);
        }

        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = match action {
                Action::Read => target.client.mark_read(&id).await,
                Action::Unsubscribe => target.client.unsubscribe(&id).await,
            };
            let _ = tx
                .send(Msg::Acted(result.map(|()| id).map_err(|e| e.to_string())))
                .await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Acted(result)) = rx.recv().await {
                match result {
                    Ok(_) => {}
                    Err(e) => {
                        // The optimistic removal was wrong; reload rather than
                        // trying to splice the row back at the right index.
                        this.status.set_text(&format!("Could not update: {e}"));
                        this.refresh();
                    }
                }
            }
        });
    }

    fn open_web(&self) {
        let Some(index) = self.selected.get() else {
            return;
        };
        let items = self.items.borrow();
        let Some(item) = items.get(index) else { return };

        // The API URL is not a web URL. Build the browser link from the parts
        // instead of trying to rewrite api.github.com in place.
        let Some((owner, repo)) = item.owner_and_repo() else {
            return;
        };
        let url = match (item.number(), item.subject.kind.as_str()) {
            (Some(n), "PullRequest") => format!("https://github.com/{owner}/{repo}/pull/{n}"),
            (Some(n), "Issue") => format!("https://github.com/{owner}/{repo}/issues/{n}"),
            // A CheckSuite or Release has no per-item page worth guessing at;
            // the repository is the closest useful destination.
            _ => format!("https://github.com/{owner}/{repo}"),
        };

        let launcher = gtk::UriLauncher::new(&url);
        launcher.launch(
            self.root.root().and_downcast::<gtk::Window>().as_ref(),
            None::<&gtk::gio::Cancellable>,
            |_| {},
        );
    }
}

#[derive(Clone, Copy)]
enum Action {
    Read,
    Unsubscribe,
}

fn row(n: &Notification) -> gtk::ListBoxRow {
    let title = gtk::Label::new(Some(&n.subject.title));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_hexpand(true);

    let meta = gtk::Label::new(Some(&format!(
        "{} · {} · {}",
        n.repository.full_name,
        n.subject.kind,
        n.reason.replace('_', " ")
    )));
    meta.set_xalign(0.0);
    meta.add_css_class("dim-label");
    meta.add_css_class("caption");
    meta.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.append(&title);
    text.append(&meta);

    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row_box.set_margin_start(10);
    row_box.set_margin_end(10);
    row_box.set_margin_top(8);
    row_box.set_margin_bottom(8);

    // A dot rather than a word: it marks the rows worth reading first without
    // adding another column of text to scan.
    if n.is_direct() {
        let dot = gtk::Label::new(Some("●"));
        dot.add_css_class("accent");
        row_box.append(&dot);
    }
    row_box.append(&text);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn build_layout(
    list: &gtk::ListBox,
    status: &gtk::Label,
    spinner: &gtk::Spinner,
    read_btn: &gtk::Button,
    unsub_btn: &gtk::Button,
    open_btn: &gtk::Button,
) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_margin_start(12);
    header.set_margin_end(12);
    header.set_margin_top(8);
    header.set_margin_bottom(8);
    status.set_hexpand(true);
    header.append(status);
    header.append(spinner);
    header.append(open_btn);
    header.append(unsub_btn);
    header.append(read_btn);

    let scroll = gtk::ScrolledWindow::builder()
        .child(list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&scroll);
    content.upcast()
}

#[cfg(test)]
mod tests {
    use github::notifications::{Notification, NotificationRepo, Subject};

    fn n(reason: &str, kind: &str, url: Option<&str>) -> Notification {
        Notification {
            id: "1".into(),
            unread: true,
            reason: reason.into(),
            updated_at: None,
            subject: Subject {
                title: "t".into(),
                url: url.map(str::to_string),
                kind: kind.into(),
            },
            repository: NotificationRepo {
                name: "r".into(),
                full_name: "o/r".into(),
            },
        }
    }

    /// Mirrors `open_web`'s mapping. The API URL is not a browser URL, so the
    /// link is built from parts rather than by rewriting the host.
    fn web_url(item: &Notification) -> Option<String> {
        let (owner, repo) = item.owner_and_repo()?;
        Some(match (item.number(), item.subject.kind.as_str()) {
            (Some(x), "PullRequest") => format!("https://github.com/{owner}/{repo}/pull/{x}"),
            (Some(x), "Issue") => format!("https://github.com/{owner}/{repo}/issues/{x}"),
            _ => format!("https://github.com/{owner}/{repo}"),
        })
    }

    #[test]
    fn pull_requests_and_issues_get_their_own_urls() {
        let pr = n(
            "review_requested",
            "PullRequest",
            Some("https://api.github.com/repos/o/r/pulls/7"),
        );
        assert_eq!(web_url(&pr).unwrap(), "https://github.com/o/r/pull/7");

        let issue = n(
            "mention",
            "Issue",
            Some("https://api.github.com/repos/o/r/issues/9"),
        );
        assert_eq!(web_url(&issue).unwrap(), "https://github.com/o/r/issues/9");
    }

    #[test]
    fn a_check_suite_falls_back_to_the_repository() {
        // Real case: CI activity arrives with a null subject URL, so there is
        // no per-item page to link to.
        let ci = n("ci_activity", "CheckSuite", None);
        assert_eq!(web_url(&ci).unwrap(), "https://github.com/o/r");
    }

    #[test]
    fn direct_notifications_sort_first() {
        let mut items = [
            n("subscribed", "Issue", None),
            n("review_requested", "PullRequest", None),
            n("ci_activity", "CheckSuite", None),
            n("mention", "Issue", None),
        ];
        items.sort_by_key(|x| !x.is_direct());

        assert!(items[0].is_direct() && items[1].is_direct());
        assert!(!items[2].is_direct() && !items[3].is_direct());
    }
}
