//! The menu bar. Each item's id goes to the UI as a "menu" event, which runs it
//! on the tab in front; the shortcuts follow Transmission where it has one.

use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem, Submenu, SubmenuBuilder};
use tauri::{AppHandle, Runtime};

fn item<R: Runtime>(app: &AppHandle<R>, id: &str, text: &str, keys: Option<&str>) -> tauri::Result<tauri::menu::MenuItem<R>> {
    let mut builder = MenuItemBuilder::with_id(id, text);
    if let Some(keys) = keys {
        builder = builder.accelerator(keys);
    }
    builder.build(app)
}

fn submenu<R: Runtime>(
    app: &AppHandle<R>,
    title: &str,
    entries: &[Option<(&str, &str, Option<&str>)>],
) -> tauri::Result<Submenu<R>> {
    let mut builder = SubmenuBuilder::new(app, title);
    for entry in entries {
        builder = match entry {
            Some((id, text, keys)) => builder.item(&item(app, id, text, *keys)?),
            None => builder.separator(),
        };
    }
    builder.build()
}

pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let app_menu = SubmenuBuilder::new(app, "KDownloader")
        .item(&item(app, "about", "About KDownloader", None)?)
        .item(&item(app, "check-updates", "Check for Updates…", None)?)
        .separator()
        .item(&item(app, "settings", "Settings…", Some("CmdOrCtrl+Comma"))?)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;
    let file = SubmenuBuilder::new(app, "File")
        .item(&item(app, "open-torrent", "Open Torrent File…", Some("CmdOrCtrl+O"))?)
        .item(&item(app, "add-links", "Add Magnet or Links…", Some("CmdOrCtrl+U"))?)
        .separator()
        .item(&PredefinedMenuItem::close_window(app, None)?)
        .build()?;
    // Standard editing, so text fields keep copy, paste and select all.
    let edit = SubmenuBuilder::new(app, "Edit").undo().redo().separator().cut().copy().paste().select_all().build()?;
    // Remove has no key here: ⌘⌫ must keep deleting text in fields. The list
    // handles Delete and ⌘⌫ itself.
    let transfers = submenu(
        app,
        "Transfers",
        &[
            Some(("resume", "Resume Selected", Some("CmdOrCtrl+Slash"))),
            Some(("pause", "Pause Selected", Some("CmdOrCtrl+Period"))),
            Some(("resume-all", "Resume All", Some("Alt+CmdOrCtrl+Slash"))),
            Some(("pause-all", "Pause All", Some("Alt+CmdOrCtrl+Period"))),
            None,
            Some(("reveal", "Show in Finder", Some("CmdOrCtrl+R"))),
            Some(("limits", "Speed Limits…", Some("Shift+CmdOrCtrl+L"))),
            Some(("reannounce", "Force Re-announce", None)),
            Some(("move-top", "Move to Top of Queue", Some("Alt+CmdOrCtrl+Up"))),
            Some(("move-bottom", "Move to Bottom of Queue", Some("Alt+CmdOrCtrl+Down"))),
            None,
            Some(("remove", "Remove…", None)),
            Some(("remove-trash", "Remove and Move Files to Trash", None)),
        ],
    )?;
    let view = submenu(
        app,
        "View",
        &[
            Some(("tab-torrent", "Torrents", Some("CmdOrCtrl+1"))),
            Some(("tab-direct", "Direct Download", Some("CmdOrCtrl+2"))),
            Some(("link-grabber", "Link Grabber", Some("CmdOrCtrl+3"))),
            Some(("tab-bandwidth", "Bandwidth", Some("CmdOrCtrl+4"))),
            None,
            Some(("details", "Show or Hide Details", Some("CmdOrCtrl+I"))),
            Some(("options", "Show or Hide Add Options", Some("Alt+CmdOrCtrl+O"))),
        ],
    )?;
    let window = SubmenuBuilder::new(app, "Window").minimize().maximize().separator().fullscreen().build()?;
    let help = submenu(app, "Help", &[Some(("shortcuts", "Keyboard Shortcuts", None))])?;
    Menu::with_items(app, &[&app_menu, &file, &edit, &transfers, &view, &window, &help])
}
