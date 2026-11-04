//! The native menu. Its labels come from the UI's translations, so changing the language
//! changes the menu without a restart.

use std::collections::HashMap;

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter};

const SETTINGS_ID: &str = "settings";
const SETTINGS_EVENT: &str = "keel://open-settings";

pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    if event.id().as_ref() == SETTINGS_ID {
        let _ = app.emit(SETTINGS_EVENT, ());
    }
}

#[tauri::command]
pub fn menu_set(app: AppHandle, labels: HashMap<String, String>) -> Result<(), String> {
    build(&app, &labels).map_err(|e| e.to_string())
}

fn build(app: &AppHandle, labels: &HashMap<String, String>) -> tauri::Result<()> {
    let text = |key: &str| labels.get(key).cloned().unwrap_or_else(|| key.to_owned());
    let label = |key: &str| Some(labels.get(key).cloned().unwrap_or_else(|| key.to_owned()));

    let keel = Submenu::with_items(
        app,
        "Keel",
        true,
        &[
            &PredefinedMenuItem::about(app, label("about").as_deref(), None)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(
                app,
                SETTINGS_ID,
                text("settings"),
                true,
                Some("CmdOrCtrl+,"),
            )?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, label("hide").as_deref())?,
            &PredefinedMenuItem::hide_others(app, label("hideOthers").as_deref())?,
            &PredefinedMenuItem::show_all(app, label("showAll").as_deref())?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::quit(app, label("quit").as_deref())?,
        ],
    )?;
    let edit = Submenu::with_items(
        app,
        text("edit"),
        true,
        &[
            &PredefinedMenuItem::undo(app, label("undo").as_deref())?,
            &PredefinedMenuItem::redo(app, label("redo").as_deref())?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, label("cut").as_deref())?,
            &PredefinedMenuItem::copy(app, label("copy").as_deref())?,
            &PredefinedMenuItem::paste(app, label("paste").as_deref())?,
            &PredefinedMenuItem::select_all(app, label("selectAll").as_deref())?,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        text("window"),
        true,
        &[
            &PredefinedMenuItem::minimize(app, label("minimize").as_deref())?,
            &PredefinedMenuItem::maximize(app, label("zoom").as_deref())?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, label("close").as_deref())?,
        ],
    )?;
    app.set_menu(Menu::with_items(app, &[&keel, &edit, &window])?)?;
    Ok(())
}
