//! OpenSuperWhisper window: dictation history and settings. A separate
//! process from the daemon; talks to it over the control socket and reads
//! the shared history database and config file directly.

mod daemon;
mod history_page;
mod settings_page;

use std::cell::RefCell;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

const APP_ID: &str = "io.github.OpenSuperWhisper";

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // Command lines are handled by the first instance, so `osw-ui --settings`
    // from the tray switches an already open window to that page.
    let mut flags = gio::ApplicationFlags::HANDLES_COMMAND_LINE;
    if std::env::var_os("OSW_UI_SCREENSHOT").is_some() {
        // Never hand the command line to (and thereby disturb) an open window.
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let app = adw::Application::builder().application_id(APP_ID).flags(flags).build();
    app.connect_command_line(|app, cmdline| {
        let page = if cmdline.arguments().iter().any(|a| a == "--settings") { "settings" } else { "history" };
        show(app, page);
        glib::ExitCode::SUCCESS
    });
    app.run()
}

thread_local! {
    static STACK: RefCell<Option<adw::ViewStack>> = const { RefCell::new(None) };
}

fn show(app: &adw::Application, page: &str) {
    if app.active_window().is_none() {
        build_window(app);
    }
    STACK.with_borrow(|stack| {
        if let Some(stack) = stack {
            stack.set_visible_child_name(page);
        }
    });
    if let Some(window) = app.active_window() {
        window.present();
    }
}

fn build_window(app: &adw::Application) {
    if !daemon::is_running() {
        if let Err(e) = daemon::start() {
            log::error!("could not start osw-daemon: {e}");
        }
    }

    let toasts = adw::ToastOverlay::new();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("OpenSuperWhisper")
        .default_width(760)
        .default_height(680)
        .build();

    let stack = adw::ViewStack::new();
    STACK.set(Some(stack.clone()));
    let history = history_page::build(&window, &toasts);
    stack.add_titled_with_icon(&history, Some("history"), "History", "document-open-recent-symbolic");
    let settings = settings_page::build(&window, &toasts);
    stack.add_titled_with_icon(&settings, Some("settings"), "Settings", "preferences-system-symbolic");

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(
        &adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build(),
    ));

    let banner = adw::Banner::builder()
        .title("The OpenSuperWhisper service is not running")
        .button_label("Start")
        .revealed(false)
        .build();
    banner.connect_button_clicked(|banner| {
        if daemon::start().is_ok() {
            banner.set_revealed(false);
        }
    });
    {
        let banner = banner.clone();
        glib::timeout_add_seconds_local(2, move || {
            banner.set_revealed(!daemon::is_running());
            glib::ControlFlow::Continue
        });
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&stack);
    stack.set_vexpand(true);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(&toasts));

    // Dropping audio files anywhere on the window queues them.
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    {
        let toasts = toasts.clone();
        let stack = stack.clone();
        drop.connect_drop(move |_, value, _, _| {
            let Ok(list) = value.get::<gdk::FileList>() else { return false };
            let files: Vec<gio::File> = list.files();
            stack.set_visible_child_name("history");
            queue_files(&files, &toasts);
            true
        });
    }
    window.add_controller(drop);
    window.present();

    // Developer aid for checking layouts: OSW_UI_SCREENSHOT=out.png [OSW_UI_PAGE=settings]
    if let Ok(path) = std::env::var("OSW_UI_SCREENSHOT") {
        if let Ok(page) = std::env::var("OSW_UI_PAGE") {
            stack.set_visible_child_name(&page);
        }
        if let Some(h) = std::env::var("OSW_UI_HEIGHT").ok().and_then(|h| h.parse().ok()) {
            window.set_default_size(760, h);
        }
        let window = window.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
            screenshot(&window, &path);
            window.close();
        });
    }
}

fn screenshot(window: &adw::ApplicationWindow, path: &str) {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let (w, h) = (window.width(), window.height());
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, w as f64, h as f64);
    let Some(node) = snapshot.to_node() else { return };
    let Some(renderer) = window.native().and_then(|n| n.renderer()) else { return };
    let texture = renderer.render_texture(&node, None);
    if let Err(e) = texture.save_to_png(path) {
        log::error!("screenshot failed: {e}");
    }
}

/// Sends audio files to the daemon for transcription into the history.
pub fn queue_files(files: &[gio::File], toasts: &adw::ToastOverlay) {
    let mut queued = 0;
    for path in files.iter().filter_map(|f| f.path()) {
        match daemon::send(&format!("transcribe-file {}", path.display())) {
            Ok(_) => queued += 1,
            Err(e) => {
                toasts.add_toast(adw::Toast::new(&e));
                return;
            }
        }
    }
    if queued > 0 {
        let msg = if queued == 1 { "Transcribing 1 file".to_string() } else { format!("Transcribing {queued} files") };
        toasts.add_toast(adw::Toast::new(&msg));
    }
}
