# trayproxy

A Rust reimplementation of [snixembed](https://git.sr.ht/~steef/snixembed):
it shows StatusNotifierItem (SNI / AppIndicator) tray icons in XEmbed system
trays like dwm's systray patch, stalonetray or trayer.

It owns `org.kde.StatusNotifierWatcher` on the session bus, so **stop
snixembed before starting it**.

## Features

- Icons from theme names, file paths and ARGB pixmaps, plus `NeedsAttention` icons
- SVG/SVGZ icons (theme icons, file paths, menu icons) rendered with resvg,
  so they work even without gdk-pixbuf's SVG loader
- Left click → `Activate`, middle click → `SecondaryActivate`, wheel → `Scroll`
- Right click → dbusmenu popup (checkmarks, radio items, submenus, icons),
  falling back to `ContextMenu` when there is no menu
- Hover tooltips (Pango markup, falls back to plain text)
- AppIndicator-style registrations (object path instead of bus name)
- Hide apps through a config file that reloads while it runs
- Re-docks icons if the tray restarts
- `--fork` for startup scripts, like snixembed

Unlike snixembed it doesn't use `GtkStatusIcon`. It creates and paints the
tray windows itself with x11rb and XRender. GTK is used only for menus and
tooltips.

## Hiding apps

`~/.config/trayproxy/config.toml` (or `$XDG_CONFIG_HOME`, or `--config PATH`):

```toml
# Matched case-insensitively against each item's Id, Title and D-Bus name.
# `*` and `?` wildcards work.
hide = [
  "nm-applet",
  "chrome_status_icon_*",
  "Discord",
]

# Also hide items whose status is "Passive" (the app's "nothing to see here").
hide_passive = false
```

The file is checked every 2 seconds (or on `kill -HUP`), and icons appear or
disappear right away. A file with errors is ignored and the last good config
stays in place.

To find the values to put in `hide`, run this while trayproxy is running:

```
trayproxy --list
```

The log also prints `item registered: id=... title=... bus=...` for every item.

## Build / run

Needs GTK 3 and an X11 session.

```
cargo build --release
./target/release/trayproxy            # or: trayproxy --fork  (in .xinitrc)
```
