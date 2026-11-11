//! GTK pieces: dbusmenu popup menus and hover tooltips.

use crate::dbus::{self, MenuNode};
use gtk::prelude::*;

pub struct MenuTarget {
    pub conn: zbus::Connection,
    pub bus: String,
    pub path: String,
}

pub fn build_menu(node: &MenuNode, target: &std::rc::Rc<MenuTarget>) -> gtk::Menu {
    let menu = gtk::Menu::new();
    for child in &node.children {
        if !child.bool("visible", true) {
            continue;
        }
        if child.str("type") == Some("separator") {
            menu.append(&gtk::SeparatorMenuItem::new());
            continue;
        }
        let label = child.str("label").unwrap_or("");
        let toggle = child.str("toggle-type").unwrap_or("");
        let item: gtk::MenuItem = if toggle == "checkmark" || toggle == "radio" {
            let c = gtk::CheckMenuItem::with_mnemonic(label);
            c.set_draw_as_radio(toggle == "radio");
            c.set_active(child.int("toggle-state") == Some(1));
            c.upcast()
        } else if let Some(image) = menu_icon(child) {
            let item = gtk::MenuItem::new();
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            hbox.pack_start(&image, false, false, 0);
            let l = gtk::Label::with_mnemonic(label);
            l.set_xalign(0.0);
            hbox.pack_start(&l, true, true, 0);
            item.add(&hbox);
            item
        } else {
            gtk::MenuItem::with_mnemonic(label)
        };
        item.set_sensitive(child.bool("enabled", true));

        if !child.children.is_empty() || child.str("children-display") == Some("submenu") {
            item.set_submenu(Some(&build_menu(child, target)));
        } else {
            let id = child.id;
            let t = target.clone();
            item.connect_activate(move |_| {
                let t = t.clone();
                let time = gtk::current_event_time();
                gtk::glib::MainContext::default().spawn_local(async move {
                    dbus::menu_event(&t.conn, &t.bus, &t.path, id, "clicked", time).await;
                });
            });
        }
        menu.append(&item);
    }
    menu.show_all();
    menu
}

fn menu_icon(node: &MenuNode) -> Option<gtk::Image> {
    if let Some(name) = node.str("icon-name").filter(|n| !n.is_empty()) {
        if let Some(img) = crate::icon::render(name, "", &Vec::new(), 16) {
            return Some(gtk::Image::from_pixbuf(Some(&crate::icon::image_to_pixbuf(&img))));
        }
    }
    let data = node.bytes("icon-data").filter(|d| !d.is_empty())?;
    let pb = crate::icon::pixbuf_from_data(data, 16)?;
    Some(gtk::Image::from_pixbuf(Some(&pb)))
}

/// A single tooltip popup shared by all icons.
pub struct Tooltip {
    window: gtk::Window,
    label: gtk::Label,
    pending: Option<gtk::glib::SourceId>,
}

impl Tooltip {
    pub fn new() -> Self {
        let window = gtk::Window::new(gtk::WindowType::Popup);
        window.set_type_hint(gtk::gdk::WindowTypeHint::Tooltip);
        window.style_context().add_class("tooltip");
        let label = gtk::Label::new(None);
        label.set_margin_top(4);
        label.set_margin_bottom(4);
        label.set_margin_start(6);
        label.set_margin_end(6);
        window.add(&label);
        Tooltip { window, label, pending: None }
    }

    /// Prepare to show `text` near the icon at (x, y)-(x+w, y+h) after a delay.
    pub fn schedule(this: &std::rc::Rc<std::cell::RefCell<Self>>, text: String, x: i32, y: i32, w: i32, h: i32) {
        let mut me = this.borrow_mut();
        me.cancel();
        if text.trim().is_empty() {
            return;
        }
        let weak = std::rc::Rc::downgrade(this);
        me.pending = Some(gtk::glib::timeout_add_local_once(
            std::time::Duration::from_millis(500),
            move || {
                if let Some(t) = weak.upgrade() {
                    let mut t = t.borrow_mut();
                    t.pending = None;
                    t.show(&text, x, y, w, h);
                }
            },
        ));
    }

    fn show(&self, text: &str, x: i32, y: i32, w: i32, h: i32) {
        let text = text.replace("<br>", "\n").replace("<br/>", "\n").replace("<br />", "\n");
        if gtk::pango::parse_markup(&text, '\0').is_ok() {
            self.label.set_markup(&text);
        } else {
            self.label.set_text(&text);
        }
        self.window.resize(1, 1);
        let (_, nat) = self.window.preferred_size();
        let (tw, th) = (nat.width, nat.height);
        let (sw, sh) = gtk::gdk::Screen::default()
            .and_then(|s| s.root_window())
            .map(|r| (r.width(), r.height()))
            .unwrap_or((i32::MAX, i32::MAX));
        // Below the icon if the tray is at the top of the screen, else above.
        let ty = if y + h + th + 4 <= sh { y + h + 4 } else { (y - th - 4).max(0) };
        let tx = (x + w / 2 - tw / 2).clamp(0, (sw - tw).max(0));
        self.window.move_(tx, ty);
        self.window.show_all();
    }

    pub fn cancel(&mut self) {
        if let Some(id) = self.pending.take() {
            id.remove();
        }
        self.window.hide();
    }
}
