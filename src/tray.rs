//! X11 side: XEmbed system tray icons drawn directly with XRender.
//!
//! We own every X window ourselves instead of going through GtkStatusIcon.
//! That matters for trays like dwm's systray patch, which relayout and redraw
//! the whole bar on every unmap/reparent/destroy of an icon (and re-map icons
//! that unmap themselves). GTK tears a status icon down in several such steps;
//! we remove an icon with exactly one request: XDestroyWindow. Icon updates
//! only repaint our own window and never resize/remap it.

use std::os::fd::{AsRawFd, RawFd};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::render::{self, ConnectionExt as _, PictOp, PictType, Pictformat};
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

pub type Error = Box<dyn std::error::Error>;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        _NET_SYSTEM_TRAY_OPCODE,
        _NET_SYSTEM_TRAY_VISUAL,
        _XEMBED_INFO,
        MANAGER,
        _NET_WM_NAME,
        UTF8_STRING,
    }
}

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const XEMBED_MAPPED: u32 = 1;
pub const DEFAULT_SIZE: u16 = 22;

pub struct Tray {
    pub conn: RustConnection,
    root: Window,
    root_depth: u8,
    root_visual: Visualid,
    atoms: Atoms,
    selection: Atom,
    pub manager: Option<Window>,
    argb_format: Pictformat,
    lsb_first: bool,
}

pub struct TrayIcon {
    pub win: Window,
    pub width: u16,
    pub height: u16,
    argb: bool,
    /// Tray manager window we've sent a dock request to.
    docked_to: Option<Window>,
    win_picture: render::Picture,
    src: Option<(Pixmap, render::Picture)>,
}

#[derive(Debug)]
pub enum TrayEvent {
    /// A tray manager appeared (or was replaced): (re)dock every icon.
    ManagerAppeared,
    /// The tray manager went away.
    ManagerGone,
    Expose(Window),
    Resized(Window, u16, u16),
    Button { win: Window, button: u8, x: i16, y: i16, time: u32, release: bool },
    Enter { win: Window },
    Leave,
}

impl Tray {
    pub fn new() -> Result<Tray, Error> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let (root, root_depth, root_visual) = (screen.root, screen.root_depth, screen.root_visual);
        let lsb_first = setup.image_byte_order == ImageOrder::LSB_FIRST;
        let atoms = Atoms::new(&conn)?.reply()?;
        let selection = conn
            .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())?
            .reply()?
            .atom;

        conn.prefetch_extension_information(render::X11_EXTENSION_NAME)?;
        let formats = conn.render_query_pict_formats()?.reply()?;
        let argb_format = formats
            .formats
            .iter()
            .find(|f| {
                f.type_ == PictType::DIRECT
                    && f.depth == 32
                    && f.direct.alpha_mask == 0xff
                    && f.direct.alpha_shift == 24
                    && f.direct.red_shift == 16
                    && f.direct.green_shift == 8
                    && f.direct.blue_shift == 0
            })
            .ok_or("X server has no ARGB32 picture format")?
            .id;

        // Learn about new tray managers (MANAGER client messages on root).
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;

        let mut tray = Tray {
            conn,
            root,
            root_depth,
            root_visual,
            atoms,
            selection,
            manager: None,
            argb_format,
            lsb_first,
        };
        tray.find_manager()?;
        tray.conn.flush()?;
        Ok(tray)
    }

    pub fn fd(&self) -> RawFd {
        self.conn.stream().as_raw_fd()
    }

    fn find_manager(&mut self) -> Result<(), Error> {
        let owner = self.conn.get_selection_owner(self.selection)?.reply()?.owner;
        self.manager = (owner != NONE).then_some(owner);
        if let Some(m) = self.manager {
            // So we notice when the tray goes away.
            self.conn.change_window_attributes(
                m,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
            )?;
        }
        Ok(())
    }

    /// 32-bit visual the tray asks for (compositing trays), if any.
    fn tray_visual(&self) -> Option<(Visualid, u8)> {
        let m = self.manager?;
        let reply = self
            .conn
            .get_property(false, m, self.atoms._NET_SYSTEM_TRAY_VISUAL, AtomEnum::VISUALID, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let vid = reply.value32()?.next()?;
        let screen_depth = self.conn.setup().roots.iter().flat_map(|s| &s.allowed_depths);
        for d in screen_depth {
            if d.visuals.iter().any(|v| v.visual_id == vid) {
                return (d.depth == 32).then_some((vid, 32));
            }
        }
        None
    }

    fn format_for_visual(&self, visual: Visualid) -> Result<Pictformat, Error> {
        let formats = self.conn.render_query_pict_formats()?.reply()?;
        for s in &formats.screens {
            for d in &s.depths {
                if let Some(v) = d.visuals.iter().find(|v| v.visual == visual) {
                    return Ok(v.format);
                }
            }
        }
        Err("no picture format for visual".into())
    }

    pub fn create_icon(&self, name: &str) -> Result<TrayIcon, Error> {
        let c = &self.conn;
        let win = c.generate_id()?;
        let events = EventMask::EXPOSURE
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::ENTER_WINDOW
            | EventMask::LEAVE_WINDOW;
        let size = DEFAULT_SIZE;
        let (visual, argb) = match self.tray_visual() {
            Some((vid, 32)) => {
                let cmap = c.generate_id()?;
                c.create_colormap(ColormapAlloc::NONE, cmap, self.root, vid)?;
                let aux = CreateWindowAux::new()
                    .event_mask(events)
                    .background_pixel(0)
                    .border_pixel(0)
                    .colormap(cmap);
                c.create_window(32, win, self.root, 0, 0, size, size, 0, WindowClass::INPUT_OUTPUT, vid, &aux)?;
                c.free_colormap(cmap)?;
                (vid, true)
            }
            _ => {
                // Inherit the tray's background so alpha blends against it.
                let aux = CreateWindowAux::new()
                    .event_mask(events)
                    .background_pixmap(BackPixmap::PARENT_RELATIVE);
                c.create_window(
                    self.root_depth, win, self.root, 0, 0, size, size, 0,
                    WindowClass::INPUT_OUTPUT, self.root_visual, &aux,
                )?;
                (self.root_visual, false)
            }
        };
        c.change_property32(PropMode::REPLACE, win, self.atoms._XEMBED_INFO, self.atoms._XEMBED_INFO, &[0, XEMBED_MAPPED])?;
        let mut class = name.as_bytes().to_vec();
        class.push(0);
        class.extend_from_slice(b"trayproxy\0");
        c.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, &class)?;
        c.change_property8(PropMode::REPLACE, win, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING, name.as_bytes())?;

        let win_picture = c.generate_id()?;
        c.render_create_picture(win_picture, win, self.format_for_visual(visual)?, &Default::default())?;
        Ok(TrayIcon { win, width: size, height: size, argb, docked_to: None, win_picture, src: None })
    }

    /// Ask the current tray manager to embed the icon (once per manager;
    /// a duplicate request makes some trays show the icon twice).
    pub fn dock(&self, icon: &mut TrayIcon) -> Result<(), Error> {
        let Some(m) = self.manager else { return Ok(()) };
        if icon.docked_to == Some(m) {
            return Ok(());
        }
        icon.docked_to = Some(m);
        let ev = ClientMessageEvent::new(
            32,
            m,
            self.atoms._NET_SYSTEM_TRAY_OPCODE,
            [CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, icon.win, 0, 0],
        );
        self.conn.send_event(false, m, EventMask::NO_EVENT, ev)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Remove an icon with a single DestroyNotify for the tray to handle.
    pub fn destroy(&self, icon: TrayIcon) {
        let c = &self.conn;
        if let Some((pm, pic)) = icon.src {
            let _ = c.render_free_picture(pic);
            let _ = c.free_pixmap(pm);
        }
        let _ = c.render_free_picture(icon.win_picture);
        let _ = c.destroy_window(icon.win);
        let _ = c.flush();
    }

    /// Hide all icons while no tray is around (they'd otherwise be reparented
    /// to the root window and show up in a screen corner).
    pub fn unmap(&self, icon: &mut TrayIcon) {
        icon.docked_to = None;
        let _ = self.conn.unmap_window(icon.win);
    }

    /// Upload a straight-alpha RGBA image (already scaled to fit) and paint it
    /// centred in the icon window.
    pub fn set_image(&self, icon: &mut TrayIcon, rgba: &[u8], iw: u16, ih: u16) -> Result<(), Error> {
        let c = &self.conn;
        let (w, h) = (icon.width.max(1), icon.height.max(1));
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let ox = (w as i32 - iw as i32) / 2;
        let oy = (h as i32 - ih as i32) / 2;
        for y in 0..ih as i32 {
            let ty = y + oy;
            if ty < 0 || ty >= h as i32 {
                continue;
            }
            for x in 0..iw as i32 {
                let tx = x + ox;
                if tx < 0 || tx >= w as i32 {
                    continue;
                }
                let s = (y as usize * iw as usize + x as usize) * 4;
                let a = rgba[s + 3] as u32;
                let pm = |v: u8| ((v as u32 * a + 127) / 255) as u8;
                let (r, g, b) = (pm(rgba[s]), pm(rgba[s + 1]), pm(rgba[s + 2]));
                let d = (ty as usize * w as usize + tx as usize) * 4;
                let px = if self.lsb_first { [b, g, r, a as u8] } else { [a as u8, r, g, b] };
                buf[d..d + 4].copy_from_slice(&px);
            }
        }

        let pm = c.generate_id()?;
        c.create_pixmap(32, pm, icon.win, w, h)?;
        let gc = c.generate_id()?;
        c.create_gc(gc, pm, &CreateGCAux::new())?;
        c.put_image(ImageFormat::Z_PIXMAP, pm, gc, w, h, 0, 0, 0, 32, &buf)?;
        c.free_gc(gc)?;
        let pic = c.generate_id()?;
        c.render_create_picture(pic, pm, self.argb_format, &Default::default())?;
        if let Some((old_pm, old_pic)) = icon.src.replace((pm, pic)) {
            c.render_free_picture(old_pic)?;
            c.free_pixmap(old_pm)?;
        }
        self.draw(icon)
    }

    pub fn draw(&self, icon: &TrayIcon) -> Result<(), Error> {
        let c = &self.conn;
        let Some((_, pic)) = icon.src else { return Ok(()) };
        let op = if icon.argb {
            PictOp::SRC
        } else {
            c.clear_area(false, icon.win, 0, 0, 0, 0)?;
            PictOp::OVER
        };
        c.render_composite(op, pic, NONE, icon.win_picture, 0, 0, 0, 0, 0, 0, icon.width, icon.height)?;
        c.flush()?;
        Ok(())
    }

    pub fn root_position(&self, win: Window) -> (i16, i16) {
        self.conn
            .translate_coordinates(win, self.root, 0, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| (r.dst_x, r.dst_y))
            .unwrap_or_default()
    }

    /// Release the implicit grab from a button press so GTK can grab the
    /// pointer for a popup menu.
    pub fn ungrab(&self, time: u32) {
        let _ = self.conn.ungrab_pointer(time);
        let _ = self.conn.flush();
    }

    /// Return the next pending event, translated; None when the queue is empty.
    pub fn next_event(&mut self) -> Option<TrayEvent> {
        loop {
            let ev = match self.conn.poll_for_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => return None,
                Err(e) => {
                    eprintln!("X connection error: {e}");
                    std::process::exit(1);
                }
            };
            let out = match ev {
                Event::ClientMessage(e)
                    if e.window == self.root
                        && e.type_ == self.atoms.MANAGER
                        && e.data.as_data32()[1] == self.selection =>
                {
                    let _ = self.find_manager();
                    Some(TrayEvent::ManagerAppeared)
                }
                Event::DestroyNotify(e) if Some(e.window) == self.manager => {
                    self.manager = None;
                    let _ = self.find_manager();
                    Some(if self.manager.is_some() { TrayEvent::ManagerAppeared } else { TrayEvent::ManagerGone })
                }
                Event::Expose(e) if e.count == 0 => Some(TrayEvent::Expose(e.window)),
                Event::ConfigureNotify(e) if e.window == e.event => {
                    Some(TrayEvent::Resized(e.window, e.width, e.height))
                }
                Event::ButtonPress(e) => Some(TrayEvent::Button {
                    win: e.event,
                    button: e.detail,
                    x: e.root_x,
                    y: e.root_y,
                    time: e.time,
                    release: false,
                }),
                Event::ButtonRelease(e) => Some(TrayEvent::Button {
                    win: e.event,
                    button: e.detail,
                    x: e.root_x,
                    y: e.root_y,
                    time: e.time,
                    release: true,
                }),
                Event::EnterNotify(e) => Some(TrayEvent::Enter { win: e.event }),
                Event::LeaveNotify(_) => Some(TrayEvent::Leave),
                Event::Error(e) => {
                    // Usually a BadWindow race with an icon we just destroyed.
                    eprintln!("X error: {:?} (request {})", e.error_kind, e.major_opcode);
                    None
                }
                _ => None,
            };
            if out.is_some() {
                return out;
            }
        }
    }
}
