//! The windowed commit list — a `GListModel` that never materializes the whole
//! history.
//!
//! This is the reason forqen is a GTK app and not a webview. `GtkColumnView`
//! asks a `GListModel` only for the items it is about to draw, so the model can
//! report a million rows while holding a few hundred. A DOM-based list has to
//! either render everything or reimplement this virtualization in JavaScript.
//!
//! The item objects are intentionally hollow: a `CommitItem` carries a row
//! index and nothing else. The commit data is fetched during bind, straight
//! from [`crate::state::AppState`], so GTK's item cache cannot become a second
//! uncapped copy of the history.

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::state::AppState;

mod item {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    pub struct CommitItem {
        pub index: Cell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CommitItem {
        const NAME: &'static str = "ForqenCommitItem";
        type Type = super::CommitItem;
    }

    impl ObjectImpl for CommitItem {}
}

glib::wrapper! {
    /// A row handle. Holds an index, not a commit.
    pub struct CommitItem(ObjectSubclass<item::CommitItem>);
}

impl CommitItem {
    pub fn new(index: u32) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().index.set(index);
        obj
    }

    pub fn index(&self) -> u32 {
        self.imp().index.get()
    }
}

mod model {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct CommitListModel {
        pub state: RefCell<Option<AppState>>,
        /// Rows GTK has been told about. Tracked separately from the spine so
        /// growth can be announced with `items_changed`, which is what lets the
        /// scrollbar settle as the walk proceeds instead of jumping.
        pub announced: RefCell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CommitListModel {
        const NAME: &'static str = "ForqenCommitListModel";
        type Type = super::CommitListModel;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for CommitListModel {}

    impl ListModelImpl for CommitListModel {
        fn item_type(&self) -> glib::Type {
            super::CommitItem::static_type()
        }

        fn n_items(&self) -> u32 {
            *self.announced.borrow()
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            (position < self.n_items()).then(|| super::CommitItem::new(position).upcast())
        }
    }
}

glib::wrapper! {
    pub struct CommitListModel(ObjectSubclass<model::CommitListModel>)
        @implements gio::ListModel;
}

impl CommitListModel {
    pub fn new(state: AppState) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().state.borrow_mut() = Some(state);
        obj
    }

    /// Tell GTK the spine has grown.
    ///
    /// `items_changed` with `added` and no removals is the cheap path: GTK
    /// appends without invalidating the rows already on screen, so a walk step
    /// does not cause a visible re-layout.
    pub fn sync_length(&self) {
        let Some(state) = self.imp().state.borrow().clone() else {
            return;
        };
        let actual = state.rows();
        let mut announced = self.imp().announced.borrow_mut();
        if actual > *announced {
            let from = *announced;
            let added = actual - from;
            *announced = actual;
            drop(announced);
            self.items_changed(from, 0, added);
        }
    }

    /// Reset to empty — on closing a repository or opening another.
    pub fn clear(&self) {
        let had = *self.imp().announced.borrow();
        *self.imp().announced.borrow_mut() = 0;
        if had > 0 {
            self.items_changed(0, had, 0);
        }
    }
}

/// Build the factory that renders one row.
///
/// Hydration happens here, synchronously, reading from packfiles gix has
/// already mmapped. That is a deliberate exception to "no git on the main
/// loop": a single commit read is a decompression of a few hundred bytes, and
/// routing it through a worker would mean every row flashing a placeholder
/// before settling — worse UX to fix a cost that is not measurable.
///
/// ponytail: sync hydrate on the main loop, bounded by one commit object per
/// row. If a cold page cache ever makes scrolling stutter, move `ensure` to a
/// worker and render placeholders for rows not yet realized — `HistoryWindow`
/// already returns `None` for those.
pub fn row_factory(state: AppState) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("factory items are ListItems");

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_margin_start(6);
        row.set_margin_end(6);

        let summary = gtk::Label::new(None);
        summary.set_xalign(0.0);
        summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
        summary.set_hexpand(true);

        let author = gtk::Label::new(None);
        author.set_xalign(1.0);
        author.add_css_class("dim-label");
        author.set_width_chars(18);
        author.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let sha = gtk::Label::new(None);
        sha.add_css_class("dim-label");
        sha.add_css_class("monospace");

        row.append(&summary);
        row.append(&author);
        row.append(&sha);
        list_item.set_child(Some(&row));
    });

    factory.connect_bind(move |_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("factory items are ListItems");

        let Some(item) = list_item.item().and_downcast::<CommitItem>() else {
            return;
        };
        let Some(row_box) = list_item.child().and_downcast::<gtk::Box>() else {
            return;
        };

        let index = item.index() as usize;

        // Realize a band around this row. GTK binds in visible order, so this
        // is where the window learns what the viewport is.
        let row = state.with(|s| {
            let end = (index + 1).min(s.window.len());
            if let Err(e) = s.window.ensure(&s.repo, index..end) {
                tracing::warn!(index, error = %e, "failed to hydrate commit row");
            }
            s.window.row(index).cloned()
        });

        let Some(Some(row)) = row else { return };

        let mut child = row_box.first_child();
        if let Some(l) = child.and_downcast_ref::<gtk::Label>() {
            l.set_text(&row.summary);
        }
        child = row_box.first_child().and_then(|c| c.next_sibling());
        if let Some(l) = child.and_downcast_ref::<gtk::Label>() {
            l.set_text(&row.author_name);
        }
        child = row_box.last_child();
        if let Some(l) = child.and_downcast_ref::<gtk::Label>() {
            l.set_text(&row.id.short());
        }
    });

    factory
}
