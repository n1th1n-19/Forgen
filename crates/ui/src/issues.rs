//! The Issues page.
//!
//! Same shape as Pull Requests — list on the left, detail and conversation on
//! the right — because they are the same activity from the user's side, and
//! two different layouts for reading a thread would be two things to learn.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use github::issues::{Issue, IssueComment, IssueState};

use crate::pulls::Target;

enum Msg {
    List(Result<Vec<Issue>, String>),
    // Boxed: an Issue carries a dozen owned fields, so inlining it made this
    // enum an order of magnitude larger than its other variants — every send
    // would move that much whether or not it was a Detail.
    Detail(Result<Box<(Issue, Vec<IssueComment>)>, String>),
    Acted(Result<(), String>),
}

pub struct IssuesView {
    pub root: gtk::Widget,
    rt: tokio::runtime::Handle,
    list: gtk::ListBox,
    title: gtk::Label,
    subtitle: gtk::Label,
    body: gtk::Label,
    comments: gtk::ListBox,
    reply: gtk::TextView,
    status: gtk::Label,
    spinner: gtk::Spinner,
    comment_btn: gtk::Button,
    state_btn: gtk::Button,
    open_web_btn: gtk::Button,
    target: RefCell<Option<Target>>,
    items: Rc<RefCell<Vec<Issue>>>,
    selected: Cell<Option<u64>>,
    /// Whether the selected issue is open, so the state button can say which
    /// direction it moves rather than a single ambiguous verb.
    selected_open: Cell<bool>,
}

impl IssuesView {
    pub fn new(rt: tokio::runtime::Handle) -> Rc<Self> {
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);

        let comments = gtk::ListBox::new();
        comments.add_css_class("boxed-list");
        comments.set_selection_mode(gtk::SelectionMode::None);

        let title = gtk::Label::new(Some("Select an issue"));
        title.set_xalign(0.0);
        title.add_css_class("title-4");
        title.set_wrap(true);

        let subtitle = gtk::Label::new(None);
        subtitle.set_xalign(0.0);
        subtitle.add_css_class("dim-label");
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let body = gtk::Label::new(None);
        body.set_xalign(0.0);
        body.set_yalign(0.0);
        body.set_wrap(true);
        body.set_selectable(true);

        let reply = gtk::TextView::new();
        reply.set_wrap_mode(gtk::WrapMode::WordChar);
        reply.set_top_margin(6);
        reply.set_bottom_margin(6);
        reply.set_left_margin(6);
        reply.set_right_margin(6);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let spinner = gtk::Spinner::new();

        let comment_btn = gtk::Button::with_label("Comment");
        comment_btn.add_css_class("suggested-action");
        let state_btn = gtk::Button::with_label("Close");
        let open_web_btn = gtk::Button::from_icon_name("web-browser-symbolic");
        open_web_btn.set_tooltip_text(Some("Open on GitHub"));

        for b in [&comment_btn, &state_btn, &open_web_btn] {
            b.set_sensitive(false);
        }

        let root = build_layout(
            &list,
            &title,
            &subtitle,
            &body,
            &comments,
            &reply,
            &status,
            &spinner,
            &comment_btn,
            &state_btn,
            &open_web_btn,
        );

        let view = Rc::new(Self {
            root,
            rt,
            list,
            title,
            subtitle,
            body,
            comments,
            reply,
            status,
            spinner,
            comment_btn,
            state_btn,
            open_web_btn,
            target: RefCell::new(None),
            items: Rc::new(RefCell::new(Vec::new())),
            selected: Cell::new(None),
            selected_open: Cell::new(true),
        });

        {
            let this = view.clone();
            view.comment_btn
                .connect_clicked(move |_| this.post_comment());
        }
        {
            let this = view.clone();
            view.state_btn.connect_clicked(move |_| this.toggle_state());
        }
        {
            let this = view.clone();
            view.open_web_btn.connect_clicked(move |_| this.open_web());
        }

        view
    }

    pub fn set_target(self: &Rc<Self>, target: Option<Target>) {
        *self.target.borrow_mut() = target;
        self.clear();
    }

    fn clear(&self) {
        clear(&self.list);
        clear(&self.comments);
        self.title.set_text("Select an issue");
        self.subtitle.set_text("");
        self.body.set_text("");
        self.selected.set(None);
        self.items.borrow_mut().clear();
        for b in [&self.comment_btn, &self.state_btn, &self.open_web_btn] {
            b.set_sensitive(false);
        }
    }

    pub fn refresh(self: &Rc<Self>) {
        let Some(target) = self.target.borrow().clone() else {
            self.status.set_text("No GitHub remote for this repository");
            return;
        };

        self.spinner.start();
        self.status.set_text("Loading issues…");

        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .issues(&target.owner, &target.repo, IssueState::Open)
                .await
                .map(|r| r.data)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::List(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::List(result)) = rx.recv().await {
                this.spinner.stop();
                match result {
                    Ok(items) => this.populate(items),
                    Err(e) => this.status.set_text(&format!("Could not load: {e}")),
                }
            }
        });
    }

    fn populate(self: &Rc<Self>, items: Vec<Issue>) {
        clear(&self.list);
        self.status.set_text(&match items.len() {
            0 => "No open issues".to_string(),
            n => format!("{n} open"),
        });

        for issue in &items {
            self.list.append(&self.row(issue));
        }
        *self.items.borrow_mut() = items;
    }

    fn row(self: &Rc<Self>, issue: &Issue) -> gtk::ListBoxRow {
        let title = gtk::Label::new(Some(&issue.title));
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let mut meta = format!("#{} · {}", issue.number, issue.author());
        if let Some(c) = issue.comments.filter(|c| *c > 0) {
            meta.push_str(&format!(" · {c} comments"));
        }
        for l in issue.labels.iter().take(3) {
            meta.push_str(&format!(" · {}", l.name));
        }

        let subtitle = gtk::Label::new(Some(&meta));
        subtitle.set_xalign(0.0);
        subtitle.add_css_class("dim-label");
        subtitle.add_css_class("caption");
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);

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
        let number = issue.number;
        let click = gtk::GestureClick::new();
        click.connect_released(move |_, _, _, _| this.select(number));
        row.add_controller(click);

        row
    }

    fn select(self: &Rc<Self>, number: u64) {
        let Some(target) = self.target.borrow().clone() else {
            return;
        };
        self.selected.set(Some(number));
        self.spinner.start();

        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            // Issue and comments together: one without the other is half a
            // conversation, and showing the body then filling comments in later
            // makes the pane jump as it loads.
            let issue = target
                .client
                .issue(&target.owner, &target.repo, number)
                .await
                .map(|r| r.data);
            let comments = target
                .client
                .issue_comments(&target.owner, &target.repo, number)
                .await
                .map(|r| r.data);

            let result = match (issue, comments) {
                (Ok(i), Ok(c)) => Ok(Box::new((i, c))),
                (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
            };
            let _ = tx.send(Msg::Detail(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Detail(result)) = rx.recv().await {
                this.spinner.stop();
                match result {
                    Ok(payload) => {
                        let (issue, comments) = *payload;
                        this.show_detail(issue, comments)
                    }
                    Err(e) => this.status.set_text(&format!("Could not load issue: {e}")),
                }
            }
        });
    }

    fn show_detail(self: &Rc<Self>, issue: Issue, comments: Vec<IssueComment>) {
        self.title.set_text(&issue.title);
        self.subtitle.set_text(&format!(
            "#{} · {} · {}",
            issue.number,
            issue.author(),
            if issue.is_open() { "open" } else { "closed" }
        ));
        self.body.set_text(
            issue
                .body
                .as_deref()
                .filter(|b| !b.trim().is_empty())
                .unwrap_or("(no description)"),
        );

        self.selected_open.set(issue.is_open());
        self.state_btn
            .set_label(if issue.is_open() { "Close" } else { "Reopen" });
        for b in [&self.comment_btn, &self.state_btn, &self.open_web_btn] {
            b.set_sensitive(true);
        }

        clear(&self.comments);
        for c in &comments {
            self.comments.append(&comment_row(c));
        }
    }

    fn post_comment(self: &Rc<Self>) {
        let (Some(target), Some(number)) = (self.target.borrow().clone(), self.selected.get())
        else {
            return;
        };

        let buf = self.reply.buffer();
        let body = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();
        if body.trim().is_empty() {
            self.status.set_text("Nothing to post");
            return;
        }

        self.comment_btn.set_sensitive(false);
        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .comment_on_issue(&target.owner, &target.repo, number, &body)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Acted(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Acted(result)) = rx.recv().await {
                this.comment_btn.set_sensitive(true);
                match result {
                    Ok(()) => {
                        // Cleared only once the server has it — the same rule
                        // as review drafts. Losing what someone wrote because a
                        // request failed is the one unforgivable failure here.
                        this.reply.buffer().set_text("");
                        this.status.set_text("Comment posted");
                        this.select(number);
                    }
                    Err(e) => this.status.set_text(&format!("Could not post: {e}")),
                }
            }
        });
    }

    fn toggle_state(self: &Rc<Self>) {
        let (Some(target), Some(number)) = (self.target.borrow().clone(), self.selected.get())
        else {
            return;
        };
        let reopen = !self.selected_open.get();

        self.state_btn.set_sensitive(false);
        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .set_issue_state(&target.owner, &target.repo, number, reopen)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Acted(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Acted(result)) = rx.recv().await {
                this.state_btn.set_sensitive(true);
                match result {
                    Ok(()) => {
                        this.select(number);
                        this.refresh();
                    }
                    Err(e) => this.status.set_text(&format!("Could not update: {e}")),
                }
            }
        });
    }

    fn open_web(&self) {
        let Some(number) = self.selected.get() else {
            return;
        };
        let url = self
            .items
            .borrow()
            .iter()
            .find(|i| i.number == number)
            .and_then(|i| i.html_url.clone());
        let Some(url) = url else { return };

        let launcher = gtk::UriLauncher::new(&url);
        launcher.launch(
            self.root.root().and_downcast::<gtk::Window>().as_ref(),
            None::<&gtk::gio::Cancellable>,
            |_| {},
        );
    }
}

fn comment_row(c: &IssueComment) -> gtk::ListBoxRow {
    let author = gtk::Label::new(Some(c.author()));
    author.set_xalign(0.0);
    author.add_css_class("heading");
    author.add_css_class("caption");

    let body = gtk::Label::new(Some(&c.body));
    body.set_xalign(0.0);
    body.set_wrap(true);
    body.set_selectable(true);

    let boxed = gtk::Box::new(gtk::Orientation::Vertical, 4);
    boxed.set_margin_start(8);
    boxed.set_margin_end(8);
    boxed.set_margin_top(6);
    boxed.set_margin_bottom(6);
    boxed.append(&author);
    boxed.append(&body);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&boxed));
    row
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_layout(
    list: &gtk::ListBox,
    title: &gtk::Label,
    subtitle: &gtk::Label,
    body: &gtk::Label,
    comments: &gtk::ListBox,
    reply: &gtk::TextView,
    status: &gtk::Label,
    spinner: &gtk::Spinner,
    comment_btn: &gtk::Button,
    state_btn: &gtk::Button,
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
    header.append(state_btn);

    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_bottom(8);

    let thread = gtk::Box::new(gtk::Orientation::Vertical, 8);
    thread.append(body);
    thread.append(comments);

    let thread_scroll = gtk::ScrolledWindow::builder()
        .child(&thread)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let reply_scroll = gtk::ScrolledWindow::builder()
        .child(reply)
        .height_request(90)
        .build();
    reply_scroll.add_css_class("card");

    let reply_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    reply_box.set_margin_start(12);
    reply_box.set_margin_end(12);
    reply_box.set_margin_bottom(12);
    reply_box.append(&reply_scroll);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status.set_hexpand(true);
    actions.append(status);
    actions.append(comment_btn);
    reply_box.append(&actions);

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&header);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&thread_scroll);
    right.append(&reply_box);

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
