//! The Actions page: workflow runs, their jobs, and job logs.
//!
//! A CI page earns its place by answering one question quickly — *what broke* —
//! so the failing job is preselected and its log is fetched with it. Landing on
//! a list of green ticks and making the user hunt for the red one is what the
//! web page already does.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use github::actions::{Job, WorkflowRun};

use crate::pulls::Target;

enum Msg {
    Runs(Result<Vec<WorkflowRun>, String>),
    Jobs(Result<Vec<Job>, String>),
    Log(Result<String, String>),
    Acted(Result<(), String>),
}

pub struct ActionsView {
    pub root: gtk::Widget,
    rt: tokio::runtime::Handle,
    run_list: gtk::ListBox,
    job_list: gtk::ListBox,
    log: gtk::TextView,
    title: gtk::Label,
    status: gtk::Label,
    spinner: gtk::Spinner,
    rerun_btn: gtk::Button,
    cancel_btn: gtk::Button,
    open_web_btn: gtk::Button,
    target: RefCell<Option<Target>>,
    runs: Rc<RefCell<Vec<WorkflowRun>>>,
    jobs: Rc<RefCell<Vec<Job>>>,
    selected_run: Cell<Option<u64>>,
}

impl ActionsView {
    pub fn new(rt: tokio::runtime::Handle) -> Rc<Self> {
        let run_list = gtk::ListBox::new();
        run_list.add_css_class("navigation-sidebar");
        run_list.set_selection_mode(gtk::SelectionMode::Single);

        let job_list = gtk::ListBox::new();
        job_list.add_css_class("boxed-list");
        job_list.set_selection_mode(gtk::SelectionMode::Single);

        let log = gtk::TextView::new();
        log.set_editable(false);
        log.set_monospace(true);
        log.set_cursor_visible(false);
        log.set_left_margin(8);
        log.set_right_margin(8);

        let title = gtk::Label::new(Some("Select a run"));
        title.set_xalign(0.0);
        title.add_css_class("title-4");
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let spinner = gtk::Spinner::new();

        let rerun_btn = gtk::Button::with_label("Re-run failed");
        let cancel_btn = gtk::Button::with_label("Cancel");
        cancel_btn.add_css_class("destructive-action");
        let open_web_btn = gtk::Button::from_icon_name("web-browser-symbolic");
        open_web_btn.set_tooltip_text(Some("Open on GitHub"));

        for b in [&rerun_btn, &cancel_btn, &open_web_btn] {
            b.set_sensitive(false);
        }

        let root = build_layout(
            &run_list,
            &job_list,
            &log,
            &title,
            &status,
            &spinner,
            &rerun_btn,
            &cancel_btn,
            &open_web_btn,
        );

        let view = Rc::new(Self {
            root,
            rt,
            run_list,
            job_list,
            log,
            title,
            status,
            spinner,
            rerun_btn,
            cancel_btn,
            open_web_btn,
            target: RefCell::new(None),
            runs: Rc::new(RefCell::new(Vec::new())),
            jobs: Rc::new(RefCell::new(Vec::new())),
            selected_run: Cell::new(None),
        });

        {
            let this = view.clone();
            view.rerun_btn.connect_clicked(move |_| this.rerun());
        }
        {
            let this = view.clone();
            view.cancel_btn.connect_clicked(move |_| this.cancel());
        }
        {
            let this = view.clone();
            view.open_web_btn.connect_clicked(move |_| this.open_web());
        }
        {
            let this = view.clone();
            view.job_list.connect_row_selected(move |_, row| {
                if let Some(row) = row {
                    this.load_log(row.index() as usize);
                }
            });
        }

        view
    }

    pub fn set_target(self: &Rc<Self>, target: Option<Target>) {
        *self.target.borrow_mut() = target;
        clear(&self.run_list);
        clear(&self.job_list);
        self.log.buffer().set_text("");
        self.title.set_text("Select a run");
        self.selected_run.set(None);
        for b in [&self.rerun_btn, &self.cancel_btn, &self.open_web_btn] {
            b.set_sensitive(false);
        }
    }

    pub fn refresh(self: &Rc<Self>) {
        let Some(target) = self.target.borrow().clone() else {
            self.status.set_text("No GitHub remote for this repository");
            return;
        };

        self.spinner.start();
        self.status.set_text("Loading runs…");

        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .workflow_runs(&target.owner, &target.repo)
                .await
                .map(|r| r.data)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Runs(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Runs(result)) = rx.recv().await {
                this.spinner.stop();
                match result {
                    Ok(runs) => this.populate(runs),
                    Err(e) => this.status.set_text(&format!("Could not load: {e}")),
                }
            }
        });
    }

    fn populate(self: &Rc<Self>, runs: Vec<WorkflowRun>) {
        clear(&self.run_list);

        let failing = runs.iter().filter(|r| r.failed()).count();
        let running = runs.iter().filter(|r| r.is_running()).count();
        self.status.set_text(&match (runs.len(), failing, running) {
            (0, _, _) => "No workflow runs".to_string(),
            (n, 0, 0) => format!("{n} runs"),
            (n, f, 0) => format!("{n} runs · {f} failing"),
            (n, 0, r) => format!("{n} runs · {r} in progress"),
            (n, f, r) => format!("{n} runs · {f} failing · {r} in progress"),
        });

        for r in &runs {
            self.run_list.append(&run_row(r));
        }
        *self.runs.borrow_mut() = runs;

        // Open the first failing run, or the newest if all is well. The
        // question a CI page is opened to answer is almost always "what broke".
        let pick = self.runs.borrow().iter().position(|r| r.failed()).or(
            if self.runs.borrow().is_empty() {
                None
            } else {
                Some(0)
            },
        );
        if let Some(i) = pick {
            if let Some(row) = self.run_list.row_at_index(i as i32) {
                self.run_list.select_row(Some(&row));
            }
            self.select_run(i);
        }
    }

    fn select_run(self: &Rc<Self>, index: usize) {
        let Some(run) = self.runs.borrow().get(index).cloned() else {
            return;
        };
        let Some(target) = self.target.borrow().clone() else {
            return;
        };

        self.selected_run.set(Some(run.id));
        self.title.set_text(&format!(
            "{} #{} · {}",
            run.name.as_deref().unwrap_or("workflow"),
            run.run_number.unwrap_or(0),
            run.outcome()
        ));
        self.rerun_btn.set_sensitive(run.failed());
        self.cancel_btn.set_sensitive(run.is_running());
        self.open_web_btn.set_sensitive(run.html_url.is_some());

        self.spinner.start();
        let (tx, rx) = async_channel::bounded::<Msg>(4);
        let run_id = run.id;
        self.rt.spawn(async move {
            let result = target
                .client
                .run_jobs(&target.owner, &target.repo, run_id)
                .await
                .map(|r| r.data)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Jobs(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Jobs(result)) = rx.recv().await {
                this.spinner.stop();
                match result {
                    Ok(jobs) => this.populate_jobs(jobs),
                    Err(e) => this.status.set_text(&format!("Could not load jobs: {e}")),
                }
            }
        });
    }

    fn populate_jobs(self: &Rc<Self>, jobs: Vec<Job>) {
        clear(&self.job_list);
        self.log.buffer().set_text("");

        for j in &jobs {
            self.job_list.append(&job_row(j));
        }
        *self.jobs.borrow_mut() = jobs;

        // Same reasoning as the run list: jump to what failed.
        let pick = self.jobs.borrow().iter().position(|j| j.failed()).or(
            if self.jobs.borrow().is_empty() {
                None
            } else {
                Some(0)
            },
        );
        if let Some(i) = pick {
            if let Some(row) = self.job_list.row_at_index(i as i32) {
                self.job_list.select_row(Some(&row));
            }
        }
    }

    fn load_log(self: &Rc<Self>, index: usize) {
        let (Some(target), Some(job)) = (
            self.target.borrow().clone(),
            self.jobs.borrow().get(index).cloned(),
        ) else {
            return;
        };

        self.log.buffer().set_text("Loading log…");
        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = target
                .client
                .job_log(&target.owner, &target.repo, job.id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Log(result)).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Log(result)) = rx.recv().await {
                match result {
                    Ok(text) => this.show_log(&text),
                    Err(e) => this
                        .log
                        .buffer()
                        .set_text(&format!("Could not load log:\n{e}")),
                }
            }
        });
    }

    /// Show the tail of a log rather than all of it.
    ///
    /// A CI log runs to tens of thousands of lines, and the failure is at the
    /// end. Loading the whole thing into a TextView costs memory proportional
    /// to a file nobody scrolls to the top of.
    fn show_log(&self, text: &str) {
        const TAIL_LINES: usize = 2000;

        let lines: Vec<&str> = text.lines().collect();
        let shown = if lines.len() > TAIL_LINES {
            let skipped = lines.len() - TAIL_LINES;
            format!(
                "… {skipped} earlier lines not shown …\n\n{}",
                lines[skipped..].join("\n")
            )
        } else {
            text.to_string()
        };

        let buf = self.log.buffer();
        buf.set_text(&shown);
        // Scroll to the end: the failure is at the bottom.
        let mut end = buf.end_iter();
        self.log.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
    }

    fn rerun(self: &Rc<Self>) {
        self.run_action(true);
    }

    fn cancel(self: &Rc<Self>) {
        self.run_action(false);
    }

    fn run_action(self: &Rc<Self>, rerun: bool) {
        let (Some(target), Some(run_id)) = (self.target.borrow().clone(), self.selected_run.get())
        else {
            return;
        };

        self.rerun_btn.set_sensitive(false);
        self.cancel_btn.set_sensitive(false);

        let (tx, rx) = async_channel::bounded::<Msg>(4);
        self.rt.spawn(async move {
            let result = if rerun {
                target
                    .client
                    .rerun_failed_jobs(&target.owner, &target.repo, run_id)
                    .await
            } else {
                target
                    .client
                    .cancel_run(&target.owner, &target.repo, run_id)
                    .await
            };
            let _ = tx.send(Msg::Acted(result.map_err(|e| e.to_string()))).await;
        });

        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Msg::Acted(result)) = rx.recv().await {
                match result {
                    Ok(()) => {
                        this.status
                            .set_text(if rerun { "Re-run started" } else { "Cancelled" });
                        this.refresh();
                    }
                    Err(e) => this.status.set_text(&format!("Could not act: {e}")),
                }
            }
        });
    }

    fn open_web(&self) {
        let Some(id) = self.selected_run.get() else {
            return;
        };
        let url = self
            .runs
            .borrow()
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.html_url.clone());
        let Some(url) = url else { return };

        let launcher = gtk::UriLauncher::new(&url);
        launcher.launch(
            self.root.root().and_downcast::<gtk::Window>().as_ref(),
            None::<&gtk::gio::Cancellable>,
            |_| {},
        );
    }
}

/// A glyph for a run or job state.
///
/// Text, not colour alone: about one man in twelve cannot reliably tell the
/// red from the green, and "did CI pass" is exactly the question that must not
/// depend on that.
fn glyph(outcome: &str) -> &'static str {
    match outcome {
        "success" => "✓",
        "failure" | "timed_out" => "✗",
        "cancelled" => "⊘",
        "skipped" => "–",
        "in_progress" | "queued" => "●",
        _ => "?",
    }
}

fn run_row(r: &WorkflowRun) -> gtk::ListBoxRow {
    let mark = gtk::Label::new(Some(glyph(r.outcome())));
    mark.add_css_class("monospace");
    mark.set_width_chars(2);
    if r.failed() {
        mark.add_css_class("error");
    } else if r.outcome() == "success" {
        mark.add_css_class("success");
    }

    let title = gtk::Label::new(Some(&format!(
        "{} #{}",
        r.name.as_deref().unwrap_or("workflow"),
        r.run_number.unwrap_or(0)
    )));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let meta = gtk::Label::new(Some(&format!(
        "{} · {}",
        r.head_branch.as_deref().unwrap_or("?"),
        r.event.as_deref().unwrap_or("?")
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
    row_box.set_margin_start(8);
    row_box.set_margin_end(8);
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.append(&mark);
    row_box.append(&text);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row
}

fn job_row(j: &Job) -> gtk::ListBoxRow {
    let mark = gtk::Label::new(Some(glyph(j.outcome())));
    mark.add_css_class("monospace");
    mark.set_width_chars(2);
    if j.failed() {
        mark.add_css_class("error");
    }

    let name = gtk::Label::new(Some(&j.name));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);

    // Naming the failing step saves opening the log for the common case.
    let failing = j.steps.iter().find(|s| s.failed()).map(|s| s.name.clone());
    let detail = gtk::Label::new(failing.as_deref().or(Some(j.outcome())));
    detail.add_css_class("dim-label");
    detail.add_css_class("caption");
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row_box.set_margin_start(8);
    row_box.set_margin_end(8);
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.append(&mark);
    row_box.append(&name);
    row_box.append(&detail);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_layout(
    run_list: &gtk::ListBox,
    job_list: &gtk::ListBox,
    log: &gtk::TextView,
    title: &gtk::Label,
    status: &gtk::Label,
    spinner: &gtk::Spinner,
    rerun_btn: &gtk::Button,
    cancel_btn: &gtk::Button,
    open_web_btn: &gtk::Button,
) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_margin_start(12);
    header.set_margin_end(12);
    header.set_margin_top(8);
    header.set_margin_bottom(8);
    title.set_hexpand(true);
    header.append(title);
    header.append(spinner);
    header.append(open_web_btn);
    header.append(cancel_btn);
    header.append(rerun_btn);

    let jobs_scroll = gtk::ScrolledWindow::builder()
        .child(job_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .height_request(150)
        .build();

    let log_scroll = gtk::ScrolledWindow::builder()
        .child(log)
        .vexpand(true)
        .build();

    let detail = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&jobs_scroll)
        .end_child(&log_scroll)
        .resize_start_child(false)
        .shrink_start_child(true)
        .shrink_end_child(true)
        .position(170)
        .build();

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_margin_start(12);
    footer.set_margin_end(12);
    footer.set_margin_top(4);
    footer.set_margin_bottom(4);
    footer.append(status);

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&header);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&detail);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&footer);

    let runs_scroll = gtk::ScrolledWindow::builder()
        .child(run_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .width_request(240)
        .build();

    gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&runs_scroll)
        .end_child(&right)
        .resize_start_child(false)
        .shrink_start_child(true)
        .shrink_end_child(true)
        .position(260)
        .build()
        .upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_a_glyph_and_none_repeat_across_pass_and_fail() {
        // Colour alone is not enough — about one man in twelve cannot rely on
        // it, and "did CI pass" must not be one of those questions.
        assert_eq!(glyph("success"), "✓");
        assert_eq!(glyph("failure"), "✗");
        assert_eq!(glyph("timed_out"), "✗");
        assert_ne!(glyph("success"), glyph("failure"));
        assert_ne!(glyph("success"), glyph("cancelled"));
        assert_eq!(
            glyph("something_new"),
            "?",
            "an unknown state is not a tick"
        );
    }

    #[test]
    fn a_running_state_is_distinct_from_both_outcomes() {
        assert_eq!(glyph("in_progress"), "●");
        assert_eq!(glyph("queued"), "●");
        assert_ne!(glyph("in_progress"), glyph("success"));
        assert_ne!(glyph("in_progress"), glyph("failure"));
    }
}
