mod app;
mod assets;
mod bench;
mod column_menu;
mod data;
mod grid;
mod sheet;
#[cfg(feature = "snapshot")]
mod snapshot;
mod text_input;
mod theme;
mod ui;

use std::path::PathBuf;

use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};

use app::*;
use sheet as s;

pub fn bind_keys(cx: &mut App) {
    text_input::bind_keys(cx);
    let sheet = Some("Sheet");
    cx.bind_keys([
        KeyBinding::new("up", s::MoveUp, sheet),
        KeyBinding::new("down", s::MoveDown, sheet),
        KeyBinding::new("left", s::MoveLeft, sheet),
        KeyBinding::new("right", s::MoveRight, sheet),
        KeyBinding::new("shift-up", s::SelectUp, sheet),
        KeyBinding::new("shift-down", s::SelectDown, sheet),
        KeyBinding::new("shift-left", s::SelectLeft, sheet),
        KeyBinding::new("shift-right", s::SelectRight, sheet),
        KeyBinding::new("ctrl-up", s::JumpUp, sheet),
        KeyBinding::new("ctrl-down", s::JumpDown, sheet),
        KeyBinding::new("ctrl-left", s::JumpLeft, sheet),
        KeyBinding::new("ctrl-right", s::JumpRight, sheet),
        KeyBinding::new("ctrl-shift-up", s::JumpSelectUp, sheet),
        KeyBinding::new("ctrl-shift-down", s::JumpSelectDown, sheet),
        KeyBinding::new("ctrl-shift-left", s::JumpSelectLeft, sheet),
        KeyBinding::new("ctrl-shift-right", s::JumpSelectRight, sheet),
        KeyBinding::new("pageup", s::PageUp, sheet),
        KeyBinding::new("pagedown", s::PageDown, sheet),
        KeyBinding::new("shift-pageup", s::SelectPageUp, sheet),
        KeyBinding::new("shift-pagedown", s::SelectPageDown, sheet),
        KeyBinding::new("home", s::RowStart, sheet),
        KeyBinding::new("end", s::RowEnd, sheet),
        KeyBinding::new("ctrl-home", s::SheetStart, sheet),
        KeyBinding::new("ctrl-end", s::SheetEnd, sheet),
        KeyBinding::new("ctrl-a", s::SelectAll, sheet),
        KeyBinding::new("ctrl-space", s::SelectColumn, sheet),
        KeyBinding::new("shift-space", s::SelectRow, sheet),
        KeyBinding::new("tab", s::TabNext, sheet),
        KeyBinding::new("shift-tab", s::TabPrev, sheet),
        KeyBinding::new("enter", s::Enter, sheet),
        KeyBinding::new("f2", s::EditCell, sheet),
        KeyBinding::new("delete", s::ClearCells, sheet),
        KeyBinding::new("backspace", s::ClearCells, sheet),
        KeyBinding::new("ctrl-c", s::Copy, sheet),
        KeyBinding::new("ctrl-x", s::Cut, sheet),
        KeyBinding::new("ctrl-v", s::Paste, sheet),
        KeyBinding::new("ctrl-z", s::Undo, sheet),
        KeyBinding::new("ctrl-y", s::Redo, sheet),
        KeyBinding::new("ctrl-shift-z", s::Redo, sheet),
        KeyBinding::new("ctrl-shift-l", s::ClearFilters, sheet),
        KeyBinding::new("escape", s::Dismiss, sheet),
    ]);
    let editor = Some("CellEditor");
    cx.bind_keys([
        KeyBinding::new("enter", s::CommitEdit, editor),
        KeyBinding::new("tab", s::CommitEditRight, editor),
        KeyBinding::new("escape", s::CancelEdit, editor),
    ]);
    let menu = Some("ColumnMenu");
    cx.bind_keys([
        KeyBinding::new("enter", column_menu::MenuApply, menu),
        KeyBinding::new("escape", column_menu::MenuClose, menu),
    ]);
    cx.bind_keys([
        KeyBinding::new("enter", SearchConfirm, Some("SearchBox")),
        KeyBinding::new("down", SearchConfirm, Some("SearchBox")),
        KeyBinding::new("escape", SearchCancel, Some("SearchBox")),
        KeyBinding::new("enter", NameBoxConfirm, Some("NameBox")),
        KeyBinding::new("escape", NameBoxCancel, Some("NameBox")),
    ]);
    let ws = Some("Workspace");
    cx.bind_keys([
        KeyBinding::new("ctrl-o", Open, ws),
        KeyBinding::new("ctrl-w", CloseTab, ws),
        KeyBinding::new("ctrl-tab", NextTab, ws),
        KeyBinding::new("ctrl-shift-tab", PrevTab, ws),
        KeyBinding::new("ctrl-pagedown", NextTab, ws),
        KeyBinding::new("ctrl-pageup", PrevTab, ws),
        KeyBinding::new("ctrl-f", FocusSearch, ws),
        KeyBinding::new("ctrl-g", GoTo, ws),
        KeyBinding::new("ctrl-s", Export, ws),
        KeyBinding::new("ctrl-shift-h", ToggleHeader, ws),
        KeyBinding::new("ctrl-=", ZoomIn, ws),
        KeyBinding::new("ctrl-+", ZoomIn, ws),
        KeyBinding::new("ctrl--", ZoomOut, ws),
        KeyBinding::new("ctrl-0", ZoomReset, ws),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
}

fn open_window(paths: Vec<PathBuf>, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1360.0), px(860.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Rowscope".into()),
                ..Default::default()
            }),
            app_id: Some("rowscope".into()),
            window_min_size: Some(size(px(720.0), px(420.0))),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| Workspace::new(paths, window, cx)),
    )
    .expect("failed to open window");
}

fn main() {
    // A few malloc arenas keep the footprint low; the worker threads barely
    // allocate.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 4);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--bench") {
        bench::run(&args[1..]);
        return;
    }
    #[cfg(feature = "snapshot")]
    if args.first().map(String::as_str) == Some("--snapshot") {
        snapshot::run(&args[1..]);
        return;
    }
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            bind_keys(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            open_window(paths.clone(), cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
