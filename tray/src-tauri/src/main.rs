#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, PhysicalPosition, WebviewWindow, WindowEvent,
};

const POPUP_W: f64 = 400.0;

// Best-effort: park popup near cursor, clamped into the visible screen.
fn position_near_cursor(win: &WebviewWindow, x: f64, y: f64) {
    let _ = win.set_position(PhysicalPosition::new(
        (x - POPUP_W / 2.0).max(8.0),
        y + 8.0,
    ));
    let _ = win.set_always_on_top(true);
}

fn toggle_popup(app: &tauri::AppHandle, cursor: Option<(f64, f64)>) {
    let Some(win) = app.get_webview_window("popup") else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    } else {
        if let Some((x, y)) = cursor {
            // best-effort: must not panic on multi-monitor oddities
            position_near_cursor(&win, x, y);
        }
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_http::init())
        .setup(|app| {
            let win = app.get_webview_window("popup").expect("popup window");

            // X button hides instead of exiting
            let w2 = win.clone();
            win.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = w2.hide();
                }
            });

            let open = MenuItem::with_id(app, "open", "Open tunna popup", true, None::<&str>)?;
            let manager =
                MenuItem::with_id(app, "manager", "Open manager UI (localhost:8080)", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &manager, &sep, &quit])?;

            let icon = app.default_window_icon().expect("window icon").clone();

            TrayIconBuilder::with_id("tunna-tray")
                .icon(icon)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => toggle_popup(app, None),
                    "manager" => {
                        let _ = std::process::Command::new("xdg-open")
                            .arg("http://127.0.0.1:8080")
                            .spawn();
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        position,
                        ..
                    } = event
                    {
                        let (x, y) = (position.x, position.y);
                        toggle_popup(tray.app_handle(), Some((x, y)));
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tunna-tray");
}
