//! trayproxy: proxy StatusNotifierItems as XEmbed system tray icons.
//!
//! A Rust reimplementation of snixembed. It owns org.kde.StatusNotifierWatcher
//! on the session bus and shows each registered item as an XEmbed tray icon.

mod config;
mod dbus;
mod icon;
mod tray;
mod ui;

use dbus::{Props, Registration, StatusNotifierItemProxy, Watcher, ITEM_IFACE, WATCHER_NAME, WATCHER_PATH};
use futures_util::{FutureExt, StreamExt};
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tray::{Tray, TrayEvent, TrayIcon};
use zbus::proxy::CacheProperties;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Item {
    key: String,
    owner: String,
    bus: String,
    proxy: StatusNotifierItemProxy<'static>,
    props: Props,
    icon: Option<TrayIcon>,
    task: Option<glib::JoinHandle<()>>,
    /// Keeps the open popup menu alive.
    menu: Option<gtk::Menu>,
}

struct App {
    conn: zbus::Connection,
    tray: Tray,
    items: Vec<Item>,
    config: config::Config,
    config_file: config::ConfigFile,
    tooltip: Rc<RefCell<ui::Tooltip>>,
    registered: Arc<Mutex<Vec<String>>>,
}

type AppRc = Rc<RefCell<App>>;

impl App {
    fn item_by_win(&self, win: u32) -> Option<usize> {
        self.items.iter().position(|i| i.icon.as_ref().is_some_and(|ic| ic.win == win))
    }

    fn item_by_key(&self, key: &str) -> Option<usize> {
        self.items.iter().position(|i| i.key == key)
    }

    /// Bring an item's tray icon in line with its props and the config:
    /// create, destroy, or repaint as needed.
    fn sync(&mut self, idx: usize, repaint: bool) {
        let item = &mut self.items[idx];
        let p = &item.props;
        let hidden = self.config.is_hidden(&p.id, &p.title, &item.bus, &p.status);
        match (&mut item.icon, hidden) {
            (Some(_), true) => {
                println!("hiding {} ({})", p.id, item.bus);
                self.tray.destroy(item.icon.take().unwrap());
            }
            (None, false) => match self.tray.create_icon(if p.id.is_empty() { &item.bus } else { &p.id }) {
                Ok(mut icon) => {
                    paint(&self.tray, &mut icon, p);
                    if let Err(e) = self.tray.dock(&mut icon) {
                        eprintln!("dock failed: {e}");
                    }
                    item.icon = Some(icon);
                }
                Err(e) => eprintln!("could not create tray icon for {}: {e}", item.bus),
            },
            (Some(icon), false) if repaint => paint(&self.tray, icon, p),
            _ => {}
        }
    }
}

fn paint(tray: &Tray, icon: &mut TrayIcon, p: &Props) {
    let size = icon.width.min(icon.height) as i32;
    let (name, pixmaps) = p.current_icon();
    let img = icon::render(name, &p.icon_theme_path, pixmaps, size)
        .or_else(|| icon::render("image-missing", "", &Vec::new(), size));
    if let Some(img) = img {
        if let Err(e) = tray.set_image(icon, &img.rgba, img.width, img.height) {
            eprintln!("drawing icon failed: {e}");
        }
    }
}

fn icon_changed(a: &Props, b: &Props) -> bool {
    a.current_icon() != b.current_icon() || a.icon_theme_path != b.icon_theme_path
}

/// Drain X events. Also called after X round-trips made outside the fd
/// callback, since those can leave events buffered inside x11rb.
fn pump(app: &AppRc) {
    loop {
        let ev = app.borrow_mut().tray.next_event();
        match ev {
            Some(ev) => handle_x(app, ev),
            None => break,
        }
    }
}

fn schedule_pump(app: &AppRc) {
    let app = app.clone();
    glib::idle_add_local_once(move || pump(&app));
}

fn handle_x(app: &AppRc, ev: TrayEvent) {
    let mut a = app.borrow_mut();
    match ev {
        TrayEvent::ManagerAppeared => {
            println!("tray manager appeared; docking icons");
            let a = &mut *a;
            for item in &mut a.items {
                if let Some(icon) = &mut item.icon {
                    let _ = a.tray.dock(icon);
                }
            }
        }
        TrayEvent::ManagerGone => {
            println!("tray manager went away");
            let a = &mut *a;
            for item in &mut a.items {
                if let Some(icon) = &mut item.icon {
                    a.tray.unmap(icon);
                }
            }
        }
        TrayEvent::Expose(win) => {
            if let Some(i) = a.item_by_win(win) {
                let _ = a.tray.draw(a.items[i].icon.as_ref().unwrap());
            }
        }
        TrayEvent::Resized(win, w, h) => {
            if let Some(i) = a.item_by_win(win) {
                let a = &mut *a;
                let item = &mut a.items[i];
                let icon = item.icon.as_mut().unwrap();
                if (icon.width, icon.height) != (w, h) {
                    icon.width = w;
                    icon.height = h;
                    paint(&a.tray, icon, &item.props);
                }
            }
        }
        TrayEvent::Enter { win, .. } => {
            if let Some(i) = a.item_by_win(win) {
                let icon = a.items[i].icon.as_ref().unwrap();
                let (x, y) = a.tray.root_position(win);
                let (w, h) = (icon.width as i32, icon.height as i32);
                let text = a.items[i].props.tooltip();
                ui::Tooltip::schedule(&a.tooltip, text, x as i32, y as i32, w, h);
            }
        }
        TrayEvent::Leave => a.tooltip.borrow_mut().cancel(),
        TrayEvent::Button { win, button, x, y, time } => {
            a.tooltip.borrow_mut().cancel();
            let Some(i) = a.item_by_win(win) else { return };
            let item = &a.items[i];
            let proxy = item.proxy.clone();
            let (x, y) = (x as i32, y as i32);
            let wants_menu = button == 3 || (button == 1 && item.props.item_is_menu);
            if wants_menu {
                if item.props.menu.is_some() {
                    a.tray.ungrab(time);
                    let key = item.key.clone();
                    drop(a);
                    glib::MainContext::default().spawn_local(show_menu(app.clone(), key, button as u32, x, y));
                } else {
                    spawn_call(async move { proxy.context_menu(x, y).await });
                }
                return;
            }
            match button {
                1 => spawn_call(async move { proxy.activate(x, y).await }),
                2 => spawn_call(async move { proxy.secondary_activate(x, y).await }),
                4 => spawn_call(async move { proxy.scroll(120, "vertical").await }),
                5 => spawn_call(async move { proxy.scroll(-120, "vertical").await }),
                6 => spawn_call(async move { proxy.scroll(-120, "horizontal").await }),
                7 => spawn_call(async move { proxy.scroll(120, "horizontal").await }),
                _ => {}
            }
        }
    }
}

fn spawn_call<F: std::future::Future<Output = zbus::Result<()>> + 'static>(f: F) {
    glib::MainContext::default().spawn_local(async move {
        if let Err(e) = f.await {
            eprintln!("call to item failed: {e}");
        }
    });
}

async fn show_menu(app: AppRc, key: String, button: u32, x: i32, y: i32) {
    let (conn, bus, path, proxy) = {
        let a = app.borrow();
        let Some(i) = a.item_by_key(&key) else { return };
        let it = &a.items[i];
        (a.conn.clone(), it.bus.clone(), it.props.menu.clone().unwrap(), it.proxy.clone())
    };
    match dbus::fetch_menu(&conn, &bus, &path).await {
        Ok(layout) => {
            let target = Rc::new(ui::MenuTarget { conn, bus, path });
            let menu = ui::build_menu(&layout, &target);
            menu.popup_easy(button, 0);
            let mut a = app.borrow_mut();
            if let Some(i) = a.item_by_key(&key) {
                a.items[i].menu = Some(menu);
            }
        }
        Err(e) => {
            eprintln!("could not load menu of {bus}: {e}; falling back to ContextMenu");
            spawn_call(async move { proxy.context_menu(x, y).await });
        }
    }
}

async fn emit_changes(conn: &zbus::Connection, registered: Option<&str>, unregistered: Option<&str>) {
    let Ok(iface) = conn.object_server().interface::<_, Watcher>(WATCHER_PATH).await else { return };
    let em = iface.signal_emitter();
    if let Some(k) = registered {
        let _ = Watcher::status_notifier_item_registered(em, k).await;
    }
    if let Some(k) = unregistered {
        let _ = Watcher::status_notifier_item_unregistered(em, k).await;
    }
    let _ = iface.get().await.registered_status_notifier_items_changed(em).await;
}

async fn add_item(app: AppRc, reg: Registration) {
    let conn = app.borrow().conn.clone();
    // Resolve the unique owner so signal matching and vanish detection work
    // for well-known names too.
    let owner = if reg.bus.starts_with(':') {
        reg.bus.clone()
    } else {
        match zbus::fdo::DBusProxy::new(&conn).await {
            Ok(d) => match d.get_name_owner(reg.bus.as_str().try_into().unwrap()).await {
                Ok(o) => o.to_string(),
                Err(_) => reg.owner.clone(),
            },
            Err(_) => reg.owner.clone(),
        }
    };
    let proxy = match StatusNotifierItemProxy::builder(&conn)
        .destination(reg.bus.clone())
        .and_then(|b| b.path(reg.path.clone()))
        .map(|b| b.cache_properties(CacheProperties::No))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => return eprintln!("cannot talk to {}: {e}", reg.key),
        },
        Err(e) => return eprintln!("bad item {}: {e}", reg.key),
    };
    let props = Props::fetch(&proxy).await;
    println!(
        "item registered: id={:?} title={:?} bus={} path={}",
        props.id, props.title, reg.bus, reg.path
    );

    {
        let mut a = app.borrow_mut();
        if a.item_by_key(&reg.key).is_some() {
            return;
        }
        a.registered.lock().unwrap().push(reg.key.clone());
        let task = glib::MainContext::default().spawn_local(watch_item(
            app.clone(),
            reg.key.clone(),
            owner.clone(),
            reg.path.clone(),
        ));
        a.items.push(Item {
            key: reg.key.clone(),
            owner,
            bus: reg.bus,

            proxy,
            props,
            icon: None,
            task: Some(task),
            menu: None,
        });
        let idx = a.items.len() - 1;
        a.sync(idx, true);
    }
    schedule_pump(&app);
    emit_changes(&conn, Some(&reg.key), None).await;
}

/// Listen for an item's New* signals and refresh its props.
async fn watch_item(app: AppRc, key: String, owner: String, path: String) {
    let conn = app.borrow().conn.clone();
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(owner.as_str())
        .and_then(|b| b.path(path.as_str()))
        .and_then(|b| b.interface(ITEM_IFACE))
        .map(|b| b.build());
    let mut stream = match rule {
        Ok(rule) => match zbus::MessageStream::for_match_rule(rule, &conn, None).await {
            Ok(s) => s,
            Err(e) => return eprintln!("cannot watch {key}: {e}"),
        },
        Err(e) => return eprintln!("cannot watch {key}: {e}"),
    };
    while stream.next().await.is_some() {
        // Coalesce bursts (apps often send NewIcon+NewToolTip+... together).
        glib::timeout_future(Duration::from_millis(30)).await;
        while let Some(Some(_)) = stream.next().now_or_never() {}
        refresh(&app, &key).await;
    }
}

async fn refresh(app: &AppRc, key: &str) {
    let proxy = {
        let a = app.borrow();
        let Some(i) = a.item_by_key(key) else { return };
        a.items[i].proxy.clone()
    };
    let props = Props::fetch(&proxy).await;
    {
        let mut a = app.borrow_mut();
        let Some(i) = a.item_by_key(key) else { return };
        if a.items[i].props == props {
            return;
        }
        let repaint = icon_changed(&a.items[i].props, &props);
        a.items[i].props = props;
        a.sync(i, repaint);
    }
    schedule_pump(app);
}

fn remove_items_for_name(app: &AppRc, name: &str) {
    let removed: Vec<String> = {
        let mut a = app.borrow_mut();
        let a = &mut *a;
        let mut removed = Vec::new();
        let mut i = 0;
        while i < a.items.len() {
            if a.items[i].owner == name || a.items[i].bus == name {
                let mut item = a.items.remove(i);
                println!("item vanished: id={:?} bus={}", item.props.id, item.bus);
                if let Some(t) = item.task.take() {
                    t.abort();
                }
                if let Some(icon) = item.icon.take() {
                    a.tray.destroy(icon);
                }
                a.registered.lock().unwrap().retain(|k| *k != item.key);
                removed.push(item.key);
            } else {
                i += 1;
            }
        }
        if !removed.is_empty() {
            a.tooltip.borrow_mut().cancel();
        }
        removed
    };
    if removed.is_empty() {
        return;
    }
    let conn = app.borrow().conn.clone();
    glib::MainContext::default().spawn_local(async move {
        for k in removed {
            emit_changes(&conn, None, Some(&k)).await;
        }
    });
}

fn reload_config(app: &AppRc, force: bool) {
    let mut a = app.borrow_mut();
    if !force && !a.config_file.changed() {
        return;
    }
    if let Some(c) = a.config_file.load() {
        a.config = c;
        for i in 0..a.items.len() {
            a.sync(i, false);
        }
    }
    drop(a);
    schedule_pump(app);
}

async fn run(tray: Tray, config_path: std::path::PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = async_channel::unbounded();
    let registered = Arc::new(Mutex::new(Vec::new()));
    let watcher = Watcher { items: registered.clone(), tx };
    let conn = zbus::connection::Builder::session()?
        .serve_at(WATCHER_PATH, watcher)?
        .build()
        .await?;
    let flags = zbus::fdo::RequestNameFlags::DoNotQueue.into();
    match conn.request_name_with_flags(WATCHER_NAME, flags).await {
        Ok(zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner) => {}
        _ => {
            eprintln!("error: could not acquire {WATCHER_NAME} (is snixembed or another tray host running?)");
            std::process::exit(1);
        }
    }
    println!("trayproxy {VERSION}: watching for StatusNotifierItems");

    let mut config_file = config::ConfigFile::new(config_path);
    let config = config_file.load().unwrap_or_default();
    let app: AppRc = Rc::new(RefCell::new(App {
        conn: conn.clone(),
        tray,
        items: Vec::new(),
        config,
        config_file,
        tooltip: Rc::new(RefCell::new(ui::Tooltip::new())),
        registered,
    }));

    // X events.
    let fd = app.borrow().tray.fd();
    let a = app.clone();
    glib::source::unix_fd_add_local(fd, glib::IOCondition::IN | glib::IOCondition::HUP, move |_, cond| {
        if cond.contains(glib::IOCondition::HUP) {
            eprintln!("X connection closed");
            std::process::exit(1);
        }
        pump(&a);
        glib::ControlFlow::Continue
    });
    schedule_pump(&app);

    // Config hot-reload: on change (polled) and on SIGHUP.
    let a = app.clone();
    glib::timeout_add_seconds_local(2, move || {
        reload_config(&a, false);
        glib::ControlFlow::Continue
    });
    let a = app.clone();
    glib::source::unix_signal_add_local(1 /* SIGHUP */, move || {
        reload_config(&a, true);
        glib::ControlFlow::Continue
    });

    // Items disappearing from the bus.
    let a = app.clone();
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owners = dbus.receive_name_owner_changed().await?;
    glib::MainContext::default().spawn_local(async move {
        while let Some(sig) = owners.next().await {
            if let Ok(args) = sig.args() {
                if args.new_owner().is_none() {
                    remove_items_for_name(&a, args.name().as_str());
                }
            }
        }
    });

    if let Ok(iface) = conn.object_server().interface::<_, Watcher>(WATCHER_PATH).await {
        let _ = Watcher::status_notifier_host_registered(iface.signal_emitter()).await;
    }

    while let Ok(reg) = rx.recv().await {
        glib::MainContext::default().spawn_local(add_item(app.clone(), reg));
    }
    Ok(())
}

/// `--fork`: start a child copy and exit once it owns the watcher name, so
/// startup scripts can launch tray apps right after us without races.
fn fork_self(args: &[String]) -> ! {
    let exe = std::env::current_exe().expect("cannot find own executable");
    let mut child = std::process::Command::new(exe)
        .args(args.iter().filter(|a| *a != "--fork"))
        .spawn()
        .expect("failed to spawn");
    glib::MainContext::default().block_on(async move {
        let conn = zbus::Connection::session().await.expect("no session bus");
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.expect("no DBus proxy");
        loop {
            if let Ok(Some(st)) = child.try_wait() {
                std::process::exit(st.code().unwrap_or(1));
            }
            if dbus.name_has_owner(WATCHER_NAME.try_into().unwrap()).await == Ok(true) {
                std::process::exit(0);
            }
            glib::timeout_future(Duration::from_millis(50)).await;
        }
    })
}

/// `--list`: print the items registered with the running watcher, with the
/// values you can put in the config's `hide` list.
fn list_items() -> ! {
    glib::MainContext::default().block_on(async {
        let conn = zbus::Connection::session().await.expect("no session bus");
        let props = zbus::fdo::PropertiesProxy::builder(&conn)
            .destination(WATCHER_NAME)
            .unwrap()
            .path(WATCHER_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        let v = match props
            .get(WATCHER_NAME.try_into().unwrap(), "RegisteredStatusNotifierItems")
            .await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("no StatusNotifierWatcher running: {e}");
                std::process::exit(1);
            }
        };
        let keys: Vec<String> = v.try_into().unwrap_or_default();
        println!("{:<32} {:<32} {:<12} BUS", "ID", "TITLE", "STATUS");
        for key in keys {
            let (bus, path) = match key.find('/') {
                Some(i) => (key[..i].to_string(), key[i..].to_string()),
                None => (key.clone(), dbus::DEFAULT_ITEM_PATH.to_string()),
            };
            let Ok(b) = StatusNotifierItemProxy::builder(&conn).destination(bus.clone()) else { continue };
            let Ok(b) = b.path(path) else { continue };
            let Ok(p) = b.cache_properties(CacheProperties::No).build().await else { continue };
            let pr = Props::fetch(&p).await;
            println!("{:<32} {:<32} {:<12} {}", pr.id, pr.title, pr.status, bus);
        }
        std::process::exit(0)
    })
}

fn usage() {
    println!(
        "trayproxy {VERSION} - show StatusNotifierItems as XEmbed tray icons\n\n\
         usage: trayproxy [--fork] [--config PATH] | --list | --version\n\n\
         \x20 --fork         return once the watcher is running (for startup scripts)\n\
         \x20 --config PATH  config file (default: {})\n\
         \x20 --list         list items known to the running instance\n\
         \x20 --version      print version",
        config::default_path().display()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path = config::default_path();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-v" | "--version" => return println!("trayproxy {VERSION}"),
            "-h" | "--help" => return usage(),
            "--list" => list_items(),
            "--fork" => fork_self(&args),
            "--config" | "-c" => match it.next() {
                Some(p) => config_path = p.into(),
                None => {
                    usage();
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("unknown argument: {other}");
                usage();
                std::process::exit(2);
            }
        }
    }

    gtk::gdk::set_allowed_backends("x11");
    if let Err(e) = gtk::init() {
        eprintln!("error: cannot initialise GTK: {e}");
        std::process::exit(1);
    }
    let tray = match Tray::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot connect to X: {e}");
            std::process::exit(1);
        }
    };
    glib::MainContext::default().spawn_local(async move {
        if let Err(e) = run(tray, config_path).await {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    });
    gtk::main();
}
