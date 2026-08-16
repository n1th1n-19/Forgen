//! Interactive diff view: select lines or hunks and stage exactly those.
//!
//! Replaces the read-only source view. The engine has supported partial
//! staging since `stage::stage_lines`, but with no way to express a selection
//! the interface could only stage whole files — which is the one thing that
//! makes a git GUI worth using over `git add`.
//!
//! Syntax highlighting is traded away for interactivity. GtkSourceView gives
//! per-token colour but a `GtkTextView` selection is a character range, and
//! mapping that back to "which diff lines" is guesswork the moment a line wraps.
//! A `ColumnView` row *is* a diff line, so the selection is unambiguous by
//! construction. Additions and removals are still coloured, per line rather
//! than per token.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use git::diff::{FileDiff, LineKind};

/// What a row represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A `@@ ... @@` header. Not selectable; carries the hunk's stage button.
    HunkHeader,
    Context,
    Added,
    Removed,
    NoNewline,
}

impl RowKind {
    fn from_line(k: LineKind) -> Self {
        match k {
            LineKind::Context => Self::Context,
            LineKind::Added => Self::Added,
            LineKind::Removed => Self::Removed,
            LineKind::NoNewline => Self::NoNewline,
        }
    }

    /// Whether staging this row means anything. Context and headers are
    /// carried by the patch regardless, so selecting them is meaningless.
    pub fn is_stageable(self) -> bool {
        matches!(self, Self::Added | Self::Removed)
    }

    fn css_class(self) -> Option<&'static str> {
        match self {
            Self::Added => Some("diff-added"),
            Self::Removed => Some("diff-removed"),
            Self::HunkHeader => Some("diff-hunk-header"),
            _ => None,
        }
    }
}

mod row {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    pub struct DiffRow {
        pub hunk: Cell<usize>,
        /// Index within the hunk's line list. `usize::MAX` for a hunk header,
        /// which has no line of its own.
        pub line: Cell<usize>,
        pub kind: Cell<Option<RowKind>>,
        pub text: RefCell<String>,
        pub old_no: Cell<i64>,
        pub new_no: Cell<i64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DiffRow {
        const NAME: &'static str = "ForqenDiffRow";
        type Type = super::DiffRow;
    }

    impl ObjectImpl for DiffRow {}
}

glib::wrapper! {
    pub struct DiffRow(ObjectSubclass<row::DiffRow>);
}

impl DiffRow {
    #[allow(clippy::too_many_arguments)]
    fn new(
        hunk: usize,
        line: usize,
        kind: RowKind,
        text: String,
        old_no: Option<u32>,
        new_no: Option<u32>,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.hunk.set(hunk);
        imp.line.set(line);
        imp.kind.set(Some(kind));
        *imp.text.borrow_mut() = text;
        imp.old_no.set(old_no.map(i64::from).unwrap_or(-1));
        imp.new_no.set(new_no.map(i64::from).unwrap_or(-1));
        obj
    }

    pub fn hunk(&self) -> usize {
        self.imp().hunk.get()
    }

    pub fn line(&self) -> usize {
        self.imp().line.get()
    }

    pub fn kind(&self) -> RowKind {
        self.imp().kind.get().unwrap_or(RowKind::Context)
    }

    fn text(&self) -> String {
        self.imp().text.borrow().clone()
    }

    fn lineno(&self, old: bool) -> String {
        let v = if old {
            self.imp().old_no.get()
        } else {
            self.imp().new_no.get()
        };
        if v < 0 {
            String::new()
        } else {
            v.to_string()
        }
    }
}

/// The diff pane.
pub struct DiffView {
    pub root: gtk::Widget,
    store: gio::ListStore,
    selection: gtk::MultiSelection,
    /// The diff currently displayed, so a selection can be turned back into
    /// the `[hunk][line]` mask the staging engine expects.
    current: Rc<RefCell<Option<FileDiff>>>,
}

impl DiffView {
    pub fn new() -> Rc<Self> {
        let store = gio::ListStore::new::<DiffRow>();
        let selection = gtk::MultiSelection::new(Some(store.clone()));

        let view = gtk::ColumnView::builder()
            .model(&selection)
            .show_row_separators(false)
            .show_column_separators(false)
            .build();
        view.add_css_class("diff-view");
        view.add_css_class("monospace");

        view.append_column(&lineno_column("−", true));
        view.append_column(&lineno_column("+", false));
        view.append_column(&text_column());

        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .build();

        Rc::new(Self {
            root: scroll.upcast(),
            store,
            selection,
            current: Rc::new(RefCell::new(None)),
        })
    }

    /// Show a diff, replacing whatever was there.
    pub fn show(&self, file: &FileDiff) {
        self.store.remove_all();

        if file.is_binary {
            self.store.append(&DiffRow::new(
                0,
                usize::MAX,
                RowKind::HunkHeader,
                format!("Binary file {} differs", file.new_path),
                None,
                None,
            ));
            *self.current.borrow_mut() = Some(file.clone());
            return;
        }

        for (h, hunk) in file.hunks.iter().enumerate() {
            self.store.append(&DiffRow::new(
                h,
                usize::MAX,
                RowKind::HunkHeader,
                hunk.header(),
                None,
                None,
            ));
            for (l, line) in hunk.lines.iter().enumerate() {
                self.store.append(&DiffRow::new(
                    h,
                    l,
                    RowKind::from_line(line.kind),
                    line.to_patch_line(),
                    line.old_lineno,
                    line.new_lineno,
                ));
            }
        }

        *self.current.borrow_mut() = Some(file.clone());
    }

    pub fn clear(&self) {
        self.store.remove_all();
        *self.current.borrow_mut() = None;
    }

    pub fn current(&self) -> Option<FileDiff> {
        self.current.borrow().clone()
    }

    /// Build the `[hunk][line]` selection mask for the staging engine.
    ///
    /// Returns `None` when nothing stageable is selected, which callers treat
    /// as "do nothing" rather than as an error.
    pub fn selection_mask(&self) -> Option<Vec<Vec<bool>>> {
        let file = self.current.borrow();
        let file = file.as_ref()?;

        let mut mask: Vec<Vec<bool>> = file
            .hunks
            .iter()
            .map(|h| vec![false; h.lines.len()])
            .collect();

        let bitset = self.selection.selection();
        let mut any = false;

        for i in 0..self.store.n_items() {
            if !bitset.contains(i) {
                continue;
            }
            let Some(row) = self.store.item(i).and_downcast::<DiffRow>() else {
                continue;
            };
            // Selecting a hunk header means the whole hunk — it is the natural
            // reading of clicking the `@@` line, and it makes header rows
            // useful rather than inert.
            if row.kind() == RowKind::HunkHeader {
                if let Some(h) = mask.get_mut(row.hunk()) {
                    h.iter_mut().for_each(|v| *v = true);
                    any |= !h.is_empty();
                }
                continue;
            }
            if !row.kind().is_stageable() {
                continue;
            }
            if let Some(v) = mask.get_mut(row.hunk()).and_then(|h| h.get_mut(row.line())) {
                *v = true;
                any = true;
            }
        }

        any.then_some(mask)
    }

    /// Mask covering every change in the file, for "stage all".
    pub fn full_mask(&self) -> Option<Vec<Vec<bool>>> {
        let file = self.current.borrow();
        let file = file.as_ref()?;
        Some(
            file.hunks
                .iter()
                .map(|h| vec![true; h.lines.len()])
                .collect(),
        )
    }

    /// True when at least one stageable row is selected.
    pub fn has_selection(&self) -> bool {
        self.selection_mask().is_some()
    }

    /// Run `f` whenever the selection changes, so buttons can follow it.
    pub fn connect_selection_changed(&self, f: impl Fn() + 'static) {
        self.selection.connect_selection_changed(move |_, _, _| f());
    }
}

fn lineno_column(title: &str, old: bool) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let label = gtk::Label::new(None);
        label.set_xalign(1.0);
        label.set_width_chars(5);
        label.add_css_class("dim-label");
        label.add_css_class("monospace");
        item.set_child(Some(&label));
    });

    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let (Some(row), Some(label)) = (
            item.item().and_downcast::<DiffRow>(),
            item.child().and_downcast::<gtk::Label>(),
        ) else {
            return;
        };
        label.set_text(&row.lineno(old));
    });

    gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .build()
}

fn text_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.add_css_class("monospace");
        // No ellipsis and no wrap: a diff line is a unit, and truncating one
        // mid-token hides exactly the character that changed.
        label.set_single_line_mode(true);
        item.set_child(Some(&label));
    });

    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let (Some(row), Some(label)) = (
            item.item().and_downcast::<DiffRow>(),
            item.child().and_downcast::<gtk::Label>(),
        ) else {
            return;
        };
        label.set_text(&row.text());

        // Classes are cleared before being applied: list rows are recycled, so
        // a row that was an addition can be rebound as a removal and would
        // otherwise keep both colours.
        for c in ["diff-added", "diff-removed", "diff-hunk-header"] {
            label.remove_css_class(c);
        }
        if let Some(c) = row.kind().css_class() {
            label.add_css_class(c);
        }

        // A context line cannot be staged on its own, so it is not selectable.
        // Hunk headers stay selectable — selecting one means the whole hunk.
        item.set_selectable(row.kind().is_stageable() || row.kind() == RowKind::HunkHeader);
    });

    gtk::ColumnViewColumn::builder()
        .title("Diff")
        .factory(&factory)
        .expand(true)
        .build()
}

/// Colours for the diff rows.
///
/// Defined against libadwaita's named palette rather than as literal hex, so
/// they track the user's light/dark preference and accent instead of being two
/// fixed colours that look wrong in one of the two themes.
pub const CSS: &str = "
.diff-view .diff-added {
    background-color: alpha(@success_color, 0.18);
}
.diff-view .diff-removed {
    background-color: alpha(@error_color, 0.18);
}
.diff-view .diff-hunk-header {
    color: alpha(currentColor, 0.6);
    font-weight: bold;
}
";

#[cfg(test)]
mod tests {
    use super::*;
    use git::diff;

    #[test]
    fn only_changed_lines_are_stageable() {
        assert!(RowKind::Added.is_stageable());
        assert!(RowKind::Removed.is_stageable());
        assert!(!RowKind::Context.is_stageable());
        assert!(!RowKind::HunkHeader.is_stageable());
        assert!(
            !RowKind::NoNewline.is_stageable(),
            "the no-newline marker follows whatever line it annotates"
        );
    }

    #[test]
    fn row_kinds_map_from_diff_line_kinds() {
        assert_eq!(RowKind::from_line(LineKind::Added), RowKind::Added);
        assert_eq!(RowKind::from_line(LineKind::Removed), RowKind::Removed);
        assert_eq!(RowKind::from_line(LineKind::Context), RowKind::Context);
        assert_eq!(RowKind::from_line(LineKind::NoNewline), RowKind::NoNewline);
    }

    /// The row list must line up with `[hunk][line]` indices, because the
    /// selection mask is built from them and a drift of one stages the wrong
    /// change.
    #[test]
    fn row_indices_track_the_parsed_diff() {
        let f = diff::parse(
            "\
diff --git a/a b/a
index 1..2 100644
--- a/a
+++ b/a
@@ -1,3 +1,3 @@
 one
-two
+TWO
@@ -10,2 +10,2 @@
-ten
+TEN
",
        )
        .remove(0);

        assert_eq!(f.hunks.len(), 2);
        assert_eq!(f.hunks[0].lines.len(), 3);
        assert_eq!(f.hunks[1].lines.len(), 2);

        // Hunk 0 line 1 is the removal, line 2 the addition.
        assert_eq!(f.hunks[0].lines[1].kind, LineKind::Removed);
        assert_eq!(f.hunks[0].lines[2].kind, LineKind::Added);
        // Hunk 1 restarts line numbering at 0.
        assert_eq!(f.hunks[1].lines[0].kind, LineKind::Removed);
    }
}
