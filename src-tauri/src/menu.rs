//! The native menu. What's in it comes from the UI, built from the command registry and in the
//! person's language (see src/menu/menuModel.ts). This only makes it real and sends clicks back, so
//! an item runs the same command as its key, and nothing here can do anything else.

use serde::Deserialize;
use tauri::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Wry};

const COMMAND_PREFIX: &str = "cmd:";
const COMMAND_EVENT: &str = "keel://menu-command";

#[derive(Deserialize)]
pub struct MenuSpec {
    title: String,
    items: Vec<MenuEntry>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MenuEntry {
    #[serde(rename_all = "camelCase")]
    Command {
        command_id: String,
        title: String,
        accelerator: Option<String>,
    },
    Predefined {
        name: Predefined,
        title: String,
    },
    Separator,
}

/// Quit and Close Window end in `RunEvent::ExitRequested` and `WindowEvent::CloseRequested`, where
/// the app asks about active runs.
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub enum Predefined {
    About,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Minimize,
    Zoom,
    Close,
}

pub fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    if let Some(command) = event.id().as_ref().strip_prefix(COMMAND_PREFIX) {
        let _ = app.emit(COMMAND_EVENT, command);
    }
}

/// Replaces the menu bar. Called again when the commands or the language change.
#[tauri::command]
pub fn menu_set(app: AppHandle, menus: Vec<MenuSpec>) -> Result<(), String> {
    build(&app, &menus)
        .and_then(|menu| app.set_menu(menu))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn build(app: &AppHandle, menus: &[MenuSpec]) -> tauri::Result<Menu<Wry>> {
    let bar = Menu::new(app)?;
    for spec in menus {
        let submenu = Submenu::new(app, &spec.title, true)?;
        for entry in &spec.items {
            submenu.append(item(app, entry)?.as_ref())?;
        }
        bar.append(&submenu)?;
    }
    Ok(bar)
}

fn item(app: &AppHandle, entry: &MenuEntry) -> tauri::Result<Box<dyn IsMenuItem<Wry>>> {
    Ok(match entry {
        MenuEntry::Command {
            command_id,
            title,
            accelerator,
        } => Box::new(MenuItem::with_id(
            app,
            format!("{COMMAND_PREFIX}{command_id}"),
            title,
            true,
            accelerator.as_deref(),
        )?),
        MenuEntry::Predefined { name, title } => Box::new(predefined(app, *name, title)?),
        MenuEntry::Separator => Box::new(PredefinedMenuItem::separator(app)?),
    })
}

fn predefined(
    app: &AppHandle,
    name: Predefined,
    title: &str,
) -> tauri::Result<PredefinedMenuItem<Wry>> {
    let text = Some(title);
    match name {
        Predefined::About => PredefinedMenuItem::about(app, text, None),
        Predefined::Hide => PredefinedMenuItem::hide(app, text),
        Predefined::HideOthers => PredefinedMenuItem::hide_others(app, text),
        Predefined::ShowAll => PredefinedMenuItem::show_all(app, text),
        Predefined::Quit => PredefinedMenuItem::quit(app, text),
        Predefined::Undo => PredefinedMenuItem::undo(app, text),
        Predefined::Redo => PredefinedMenuItem::redo(app, text),
        Predefined::Cut => PredefinedMenuItem::cut(app, text),
        Predefined::Copy => PredefinedMenuItem::copy(app, text),
        Predefined::Paste => PredefinedMenuItem::paste(app, text),
        Predefined::SelectAll => PredefinedMenuItem::select_all(app, text),
        Predefined::Minimize => PredefinedMenuItem::minimize(app, text),
        Predefined::Zoom => PredefinedMenuItem::maximize(app, text),
        Predefined::Close => PredefinedMenuItem::close_window(app, text),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_the_menu_the_ui_sends() {
        let menus: Vec<MenuSpec> = serde_json::from_value(json!([{
            "title": "Keel",
            "items": [
                { "kind": "predefined", "name": "hideOthers", "title": "Hide Others" },
                { "kind": "separator" },
                { "kind": "command", "commandId": "navigate.settings", "title": "Settings…", "accelerator": "CmdOrCtrl+Comma" },
                { "kind": "command", "commandId": "run.new", "title": "New run", "accelerator": null },
            ],
        }]))
        .unwrap();
        assert_eq!(menus[0].title, "Keel");
        let items = &menus[0].items;
        assert!(matches!(
            &items[0],
            MenuEntry::Predefined { name: Predefined::HideOthers, title } if title == "Hide Others"
        ));
        assert!(matches!(items[1], MenuEntry::Separator));
        assert!(matches!(
            &items[2],
            MenuEntry::Command { command_id, accelerator: Some(key), .. }
                if command_id == "navigate.settings" && key == "CmdOrCtrl+Comma"
        ));
        assert!(matches!(
            &items[3],
            MenuEntry::Command {
                accelerator: None,
                ..
            }
        ));
    }

    #[test]
    fn takes_nothing_but_commands_and_the_items_it_knows() {
        let unknown_item = json!({ "kind": "predefined", "name": "services", "title": "Services" });
        assert!(serde_json::from_value::<MenuEntry>(unknown_item).is_err());
        let unknown_kind = json!({ "kind": "link", "title": "Site", "url": "https://example.com" });
        assert!(serde_json::from_value::<MenuEntry>(unknown_kind).is_err());
    }
}
