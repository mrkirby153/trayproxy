//! D-Bus side: the StatusNotifierWatcher service we own, the
//! StatusNotifierItem client proxy, and dbusmenu layout fetching.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::{interface, proxy};

pub const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
pub const WATCHER_PATH: &str = "/StatusNotifierWatcher";
pub const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
pub const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";
pub const MENU_IFACE: &str = "com.canonical.dbusmenu";

/// Sent from the watcher service (which zbus may run on its own thread) to
/// the GTK main loop.
pub struct Registration {
    /// Unique bus name (":1.42") of the owner of the item.
    pub owner: String,
    /// Name the item should be called on (well-known name or unique name).
    pub bus: String,
    pub path: String,
    /// The string we publish in RegisteredStatusNotifierItems.
    pub key: String,
}

pub struct Watcher {
    pub items: Arc<Mutex<Vec<String>>>,
    pub tx: async_channel::Sender<Registration>,
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] hdr: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let Some(sender) = hdr.sender().map(|s| s.to_string()) else {
            return Err(zbus::fdo::Error::Failed("no sender".into()));
        };
        // Spec says `service` is a bus name; libappindicator/ayatana pass an
        // object path instead and expect us to use the sender.
        let (bus, path) = if service.starts_with('/') {
            (sender.clone(), service.to_string())
        } else if service.is_empty() {
            (sender.clone(), DEFAULT_ITEM_PATH.to_string())
        } else {
            (service.to_string(), DEFAULT_ITEM_PATH.to_string())
        };
        let key = format!("{bus}{path}");
        if self.items.lock().unwrap().contains(&key) {
            return Ok(());
        }
        let _ = self
            .tx
            .send(Registration { owner: sender, bus, path, key })
            .await;
        Ok(())
    }

    async fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap().clone()
    }

    #[zbus(property)]
    async fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    pub async fn status_notifier_item_registered(
        e: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn status_notifier_item_unregistered(
        e: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn status_notifier_host_registered(e: &SignalEmitter<'_>) -> zbus::Result<()>;
}

pub type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

#[proxy(interface = "org.kde.StatusNotifierItem", gen_blocking = false)]
pub trait StatusNotifierItem {
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn scroll(&self, delta: i32, orientation: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn title(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_pixmap(&self) -> zbus::Result<Pixmaps>;
    #[zbus(property)]
    fn attention_icon_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> zbus::Result<Pixmaps>;
    #[zbus(property)]
    fn icon_theme_path(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn tool_tip(&self) -> zbus::Result<(String, Pixmaps, String, String)>;
    #[zbus(property)]
    fn menu(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn item_is_menu(&self) -> zbus::Result<bool>;
}

/// Snapshot of an item's properties. Missing properties are left empty.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Props {
    pub id: String,
    pub title: String,
    pub status: String,
    pub icon_name: String,
    pub icon_pixmap: Pixmaps,
    pub attention_icon_name: String,
    pub attention_icon_pixmap: Pixmaps,
    pub icon_theme_path: String,
    pub tooltip_title: String,
    pub tooltip_text: String,
    pub menu: Option<String>,
    pub item_is_menu: bool,
}

impl Props {
    pub async fn fetch(p: &StatusNotifierItemProxy<'_>) -> Props {
        let (id, title, status, icon_name, icon_pixmap, att_name, att_pix, theme, tip, menu, is_menu) = futures_util::join!(
            p.id(),
            p.title(),
            p.status(),
            p.icon_name(),
            p.icon_pixmap(),
            p.attention_icon_name(),
            p.attention_icon_pixmap(),
            p.icon_theme_path(),
            p.tool_tip(),
            p.menu(),
            p.item_is_menu(),
        );
        let (tooltip_title, tooltip_text) = tip.map(|t| (t.2, t.3)).unwrap_or_default();
        Props {
            id: id.unwrap_or_default(),
            title: title.unwrap_or_default(),
            status: status.unwrap_or_default(),
            icon_name: icon_name.unwrap_or_default(),
            icon_pixmap: icon_pixmap.unwrap_or_default(),
            attention_icon_name: att_name.unwrap_or_default(),
            attention_icon_pixmap: att_pix.unwrap_or_default(),
            icon_theme_path: theme.unwrap_or_default(),
            tooltip_title,
            tooltip_text,
            menu: menu
                .ok()
                .map(|p| p.to_string())
                .filter(|p| !p.is_empty() && p != "/"),
            item_is_menu: is_menu.unwrap_or(false),
        }
    }

    /// Which icon to show right now: (name, pixmaps).
    pub fn current_icon(&self) -> (&str, &Pixmaps) {
        if self.status.eq_ignore_ascii_case("NeedsAttention")
            && (!self.attention_icon_name.is_empty() || !self.attention_icon_pixmap.is_empty())
        {
            (&self.attention_icon_name, &self.attention_icon_pixmap)
        } else {
            (&self.icon_name, &self.icon_pixmap)
        }
    }

    pub fn tooltip(&self) -> String {
        let title = if self.tooltip_title.is_empty() { &self.title } else { &self.tooltip_title };
        match (title.is_empty(), self.tooltip_text.is_empty()) {
            (_, true) => title.clone(),
            (true, false) => self.tooltip_text.clone(),
            (false, false) => format!("{title}\n{}", self.tooltip_text),
        }
    }
}

/// One node of a dbusmenu layout.
#[derive(Debug, Default)]
pub struct MenuNode {
    pub id: i32,
    pub props: HashMap<String, MenuProp>,
    pub children: Vec<MenuNode>,
}

#[derive(Debug, Clone)]
pub enum MenuProp {
    Str(String),
    Bool(bool),
    Int(i32),
    Bytes(Vec<u8>),
}

impl MenuNode {
    pub fn str(&self, k: &str) -> Option<&str> {
        match self.props.get(k) {
            Some(MenuProp::Str(s)) => Some(s),
            _ => None,
        }
    }
    pub fn bool(&self, k: &str, default: bool) -> bool {
        match self.props.get(k) {
            Some(MenuProp::Bool(b)) => *b,
            _ => default,
        }
    }
    pub fn int(&self, k: &str) -> Option<i32> {
        match self.props.get(k) {
            Some(MenuProp::Int(i)) => Some(*i),
            _ => None,
        }
    }
    pub fn bytes(&self, k: &str) -> Option<&[u8]> {
        match self.props.get(k) {
            Some(MenuProp::Bytes(b)) => Some(b),
            _ => None,
        }
    }

    fn parse(v: &Value<'_>) -> Option<MenuNode> {
        let s = match v {
            Value::Structure(s) => s,
            Value::Value(inner) => return Self::parse(inner),
            _ => return None,
        };
        let f = s.fields();
        let id = match f.first()? {
            Value::I32(i) => *i,
            _ => return None,
        };
        let mut props = HashMap::new();
        if let Some(Value::Dict(d)) = f.get(1) {
            for (k, v) in d.iter() {
                let Value::Str(k) = k else { continue };
                let v = match v {
                    Value::Value(b) => b.as_ref(),
                    other => other,
                };
                let p = match v {
                    Value::Str(s) => MenuProp::Str(s.to_string()),
                    Value::Bool(b) => MenuProp::Bool(*b),
                    Value::I32(i) => MenuProp::Int(*i),
                    Value::Array(a) => MenuProp::Bytes(
                        a.iter()
                            .filter_map(|x| if let Value::U8(b) = x { Some(*b) } else { None })
                            .collect(),
                    ),
                    _ => continue,
                };
                props.insert(k.to_string(), p);
            }
        }
        let mut children = Vec::new();
        if let Some(Value::Array(a)) = f.get(2) {
            children.extend(a.iter().filter_map(Self::parse));
        }
        Some(MenuNode { id, props, children })
    }
}

pub async fn fetch_menu(conn: &zbus::Connection, bus: &str, path: &str) -> zbus::Result<MenuNode> {
    // Let the app refresh the menu first; ignore failures (optional method).
    let _ = conn
        .call_method(Some(bus), path, Some(MENU_IFACE), "AboutToShow", &(0i32,))
        .await;
    let reply = conn
        .call_method(
            Some(bus),
            path,
            Some(MENU_IFACE),
            "GetLayout",
            &(0i32, -1i32, Vec::<String>::new()),
        )
        .await?;
    let body = reply.body();
    let s: zbus::zvariant::Structure<'_> = body.deserialize()?;
    s.fields()
        .get(1)
        .and_then(MenuNode::parse)
        .ok_or_else(|| zbus::Error::Failure("malformed dbusmenu layout".into()))
}

pub async fn menu_event(conn: &zbus::Connection, bus: &str, path: &str, id: i32, event: &str, time: u32) {
    let r = conn
        .call_method(Some(bus), path, Some(MENU_IFACE), "Event", &(id, event, Value::I32(0), time))
        .await;
    if let Err(e) = r {
        eprintln!("dbusmenu Event({id}, {event}) on {bus}: {e}");
    }
}
