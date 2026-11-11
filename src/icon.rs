//! Turning SNI icon data (theme names, file paths, ARGB pixmaps) into RGBA
//! images of the right size. Theme lookup goes through GTK; SVGs are
//! rendered with resvg so they work without gdk-pixbuf's SVG loader.

use crate::dbus::Pixmaps;
use gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};
use gtk::prelude::*;
use std::ffi::{c_char, c_int, CStr, CString};
use std::path::{Path, PathBuf};

pub struct Image {
    pub rgba: Vec<u8>,
    pub width: u16,
    pub height: u16,
}

/// Render the best available icon so it fits in a `size`×`size` box.
pub fn render(name: &str, theme_path: &str, pixmaps: &Pixmaps, size: i32) -> Option<Image> {
    let size = size.max(1);
    (!name.is_empty())
        .then(|| from_name(name, theme_path, size))
        .flatten()
        .or_else(|| from_pixmaps(pixmaps, size).map(|p| to_rgba(&fit(p, size))))
}

fn from_name(name: &str, theme_path: &str, size: i32) -> Option<Image> {
    // Some apps put a file path in IconName.
    if name.starts_with('/') && Path::new(name).is_file() {
        return load_file(Path::new(name), size);
    }
    let theme = gtk::IconTheme::default()?;
    if !theme_path.is_empty() {
        let paths = search_path(&theme);
        if !paths.iter().any(|p| p == Path::new(theme_path)) {
            theme.append_search_path(theme_path);
        }
    }
    if !theme.has_icon(name) && !theme.rescan_if_needed() {
        // Files added to a subdirectory after the scan don't bump the
        // directory mtime; resetting the search path forces a rescan.
        set_search_path(&theme, &search_path(&theme));
    }
    if let Some(info) = theme.lookup_icon(name, size, gtk::IconLookupFlags::FORCE_SIZE) {
        if let Some(img) = info.filename().and_then(|f| load_file(&f, size)) {
            return Some(img);
        }
        if let Ok(pb) = info.load_icon() {
            return Some(to_rgba(&fit(pb, size)));
        }
    }
    // GTK ignores SVG icons when gdk-pixbuf has no SVG loader, so look for
    // them ourselves.
    let theme_name = gtk::Settings::default()
        .and_then(|s| s.gtk_icon_theme_name())
        .map(|s| s.to_string())
        .unwrap_or_default();
    find_svg(name, theme_path, &theme_name).and_then(|f| load_file(&f, size))
}

/// Load an image file, rendering SVG (and gzipped SVGZ) with resvg.
fn load_file(path: &Path, size: i32) -> Option<Image> {
    let data = std::fs::read(path).ok()?;
    if is_svg(&data) {
        return render_svg(&data, path.parent(), size);
    }
    let pb = Pixbuf::from_file_at_scale(path, size, size, true).ok()?;
    Some(to_rgba(&fit(pb, size)))
}

pub fn is_svg(data: &[u8]) -> bool {
    if data.starts_with(&[0x1f, 0x8b]) {
        return true; // gzip: svgz (other gzipped images aren't icon formats)
    }
    let head = &data[..data.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    let t = text.trim_start_matches('\u{feff}').trim_start();
    t.starts_with('<') && (text.contains("<svg") || text.contains("<!DOCTYPE svg"))
}

/// Render SVG data so it fits in a `size`×`size` box, keeping aspect ratio.
pub fn render_svg(data: &[u8], resources_dir: Option<&Path>, size: i32) -> Option<Image> {
    use resvg::{tiny_skia, usvg};
    let opt = usvg::Options { resources_dir: resources_dir.map(Path::to_path_buf), ..Default::default() };
    let tree = match usvg::Tree::from_data(data, &opt) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bad SVG icon: {e}");
            return None;
        }
    };
    let (w, h) = (tree.size().width(), tree.size().height());
    let scale = size as f32 / w.max(h);
    let (pw, ph) = (((w * scale).round() as u32).max(1), ((h * scale).round() as u32).max(1));
    let mut pixmap = tiny_skia::Pixmap::new(pw, ph)?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let mut rgba = Vec::with_capacity(pw as usize * ph as usize * 4);
    for p in pixmap.pixels() {
        let c = p.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(Image { rgba, width: pw as u16, height: ph as u16 })
}

/// Search icon directories for `<name>.svg`/`.svgz`: the app's own theme
/// path first, then the user's icon theme, then hicolor, then pixmaps.
fn find_svg(name: &str, theme_path: &str, theme_name: &str) -> Option<PathBuf> {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, PathBuf>> = Default::default();
    }
    let key = format!("{theme_path}\0{theme_name}\0{name}");
    if let Some(p) = CACHE.with(|c| c.borrow().get(&key).cloned()).filter(|p| p.is_file()) {
        return Some(p);
    }
    let mut bases: Vec<PathBuf> = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    bases.push(home.join(".icons"));
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    bases.push(data_home.join("icons"));
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    bases.extend(dirs.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("icons")));

    let mut roots: Vec<PathBuf> = Vec::new();
    if !theme_path.is_empty() {
        roots.push(PathBuf::from(theme_path));
    }
    for t in [theme_name, "hicolor"] {
        if !t.is_empty() {
            roots.extend(bases.iter().map(|b| b.join(t)));
        }
    }
    roots.push(PathBuf::from("/usr/share/pixmaps"));

    let files = [format!("{name}.svg"), format!("{name}.svgz")];
    let found = roots.iter().find_map(|r| search_dir(r, &files, 4));
    if let Some(p) = &found {
        CACHE.with(|c| c.borrow_mut().insert(key, p.clone()));
    }
    found
}

fn search_dir(dir: &Path, files: &[String], depth: u32) -> Option<PathBuf> {
    for f in files {
        let p = dir.join(f);
        if p.is_file() {
            return Some(p);
        }
    }
    if depth == 0 {
        return None;
    }
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() || t.is_symlink()))
        .map(|e| e.path())
        .collect();
    // Prefer scalable/ and larger sizes.
    subdirs.sort_by_key(|p| {
        let s = p.to_string_lossy();
        if s.contains("scalable") { 0 } else { 1 }
    });
    subdirs.iter().find_map(|d| search_dir(d, files, depth - 1))
}

fn from_pixmaps(pixmaps: &Pixmaps, size: i32) -> Option<Pixbuf> {
    // Smallest pixmap at least `size` tall, else the largest one.
    let valid = pixmaps
        .iter()
        .filter(|(w, h, d)| *w > 0 && *h > 0 && d.len() >= (*w as usize * *h as usize * 4));
    let best = valid
        .clone()
        .filter(|(_, h, _)| *h >= size)
        .min_by_key(|(_, h, _)| *h)
        .or_else(|| valid.max_by_key(|(_, h, _)| *h))?;
    let (w, h, data) = best;
    // SNI pixmaps are ARGB32 in network byte order: A, R, G, B.
    let mut rgba = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4).take(*w as usize * *h as usize) {
        rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
    }
    Some(Pixbuf::from_bytes(
        &gtk::glib::Bytes::from_owned(rgba),
        Colorspace::Rgb,
        true,
        8,
        *w,
        *h,
        *w * 4,
    ))
}

fn fit(p: Pixbuf, size: i32) -> Pixbuf {
    let (w, h) = (p.width(), p.height());
    if w <= size && h <= size && (w == size || h == size) {
        return p;
    }
    let scale = size as f64 / w.max(h) as f64;
    let (nw, nh) = (((w as f64 * scale).round() as i32).max(1), ((h as f64 * scale).round() as i32).max(1));
    p.scale_simple(nw, nh, InterpType::Bilinear).unwrap_or(p)
}

fn to_rgba(p: &Pixbuf) -> Image {
    let (w, h) = (p.width() as usize, p.height() as usize);
    let stride = p.rowstride() as usize;
    let n = p.n_channels() as usize;
    let alpha = p.has_alpha();
    let bytes = p.read_pixel_bytes();
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let i = y * stride + x * n;
            rgba.extend_from_slice(&[bytes[i], bytes[i + 1], bytes[i + 2], if alpha { bytes[i + 3] } else { 255 }]);
        }
    }
    Image { rgba, width: w as u16, height: h as u16 }
}

/// Decode dbusmenu "icon-data" (PNG per spec; SVG accepted too).
pub fn pixbuf_from_data(data: &[u8], size: i32) -> Option<Pixbuf> {
    if is_svg(data) {
        return render_svg(data, None, size).map(|i| image_to_pixbuf(&i));
    }
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    loader.write(data).ok()?;
    loader.close().ok()?;
    Some(fit(loader.pixbuf()?, size))
}

fn search_path(theme: &gtk::IconTheme) -> Vec<PathBuf> {
    use gtk::glib::translate::ToGlibPtr;
    unsafe {
        let mut p: *mut *mut c_char = std::ptr::null_mut();
        let mut n: c_int = 0;
        gtk::ffi::gtk_icon_theme_get_search_path(theme.to_glib_none().0, &mut p, &mut n);
        if p.is_null() {
            return Vec::new();
        }
        let v = (0..n as usize)
            .map(|i| PathBuf::from(CStr::from_ptr(*p.add(i)).to_string_lossy().into_owned()))
            .collect();
        gtk::glib::ffi::g_strfreev(p);
        v
    }
}

fn set_search_path(theme: &gtk::IconTheme, paths: &[PathBuf]) {
    use gtk::glib::translate::ToGlibPtr;
    let owned: Vec<CString> = paths
        .iter()
        .filter_map(|p| CString::new(p.to_string_lossy().into_owned()).ok())
        .collect();
    let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    unsafe {
        gtk::ffi::gtk_icon_theme_set_search_path(theme.to_glib_none().0, ptrs.as_mut_ptr(), ptrs.len() as c_int);
    }
}

pub fn image_to_pixbuf(img: &Image) -> Pixbuf {
    Pixbuf::from_bytes(
        &gtk::glib::Bytes::from(&img.rgba[..]),
        Colorspace::Rgb,
        true,
        8,
        img.width as i32,
        img.height as i32,
        img.width as i32 * 4,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_detection_and_render() {
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="red"/></svg>"#;
        assert!(is_svg(svg));
        assert!(!is_svg(b"\x89PNG\r\n\x1a\n...."));
        let img = render_svg(svg, None, 22).unwrap();
        assert_eq!((img.width, img.height), (11, 22)); // aspect kept, fits the box
        assert_eq!(&img.rgba[..4], &[255, 0, 0, 255]);
    }
}
