//! History: searchable list of dictations and transcribed files.

use std::cell::RefCell;
use std::process::Child;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use osw_core::history::{Entry, History, Status};

const PAGE: usize = 100;

struct State {
    history: History,
    query: String,
    limit: usize,
    seen_version: i64,
    /// Entry being played and its `pw-play` process.
    player: Option<(String, Child)>,
    list: gtk::ListBox,
    stack: gtk::Stack,
    toasts: adw::ToastOverlay,
}

type Shared = Rc<RefCell<State>>;

pub fn build(window: &adw::ApplicationWindow, toasts: &adw::ToastOverlay) -> gtk::Widget {
    let history = match History::open() {
        Ok(h) => h,
        Err(e) => {
            return adw::StatusPage::builder()
                .icon_name("dialog-error-symbolic")
                .title("History unavailable")
                .description(e.to_string())
                .build()
                .upcast();
        }
    };

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search transcriptions")
        .hexpand(true)
        .build();
    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Transcribe audio files…")
        .build();
    let top = gtk::Box::builder().spacing(8).margin_top(12).margin_bottom(6).build();
    top.append(&search);
    top.append(&add);

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .valign(gtk::Align::Start)
        .margin_top(6)
        .margin_bottom(18)
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("audio-input-microphone-symbolic")
        .title("No dictations yet")
        .description("Press Right Ctrl to dictate, or drop audio files here to transcribe them.")
        .vexpand(true)
        .build();
    let no_results = adw::StatusPage::builder()
        .icon_name("edit-find-symbolic")
        .title("No results")
        .vexpand(true)
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&list, Some("list"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&no_results, Some("no-results"));

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&top);
    column.append(&stack);
    let clamp = adw::Clamp::builder().maximum_size(860).margin_start(12).margin_end(12).child(&column).build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();

    let state: Shared = Rc::new(RefCell::new(State {
        history,
        query: String::new(),
        limit: PAGE,
        seen_version: -1,
        player: None,
        list,
        stack,
        toasts: toasts.clone(),
    }));
    reload(&state);

    {
        let state = state.clone();
        search.connect_search_changed(move |entry| {
            {
                let mut s = state.borrow_mut();
                s.query = entry.text().to_string();
                s.limit = PAGE;
            }
            reload(&state);
        });
    }
    {
        let window = window.clone();
        let toasts = toasts.clone();
        add.connect_clicked(move |_| {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Audio"));
            filter.add_mime_type("audio/*");
            filter.add_mime_type("video/mp4");
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let dialog = gtk::FileDialog::builder().title("Transcribe audio files").filters(&filters).build();
            let toasts = toasts.clone();
            dialog.open_multiple(Some(&window), None::<&gio::Cancellable>, move |result| {
                if let Ok(model) = result {
                    let files: Vec<gio::File> =
                        (0..model.n_items()).filter_map(|i| model.item(i)?.downcast::<gio::File>().ok()).collect();
                    crate::queue_files(&files, &toasts);
                }
            });
        });
    }

    // The daemon writes from another process; poll the cheap data_version
    // and refresh when something changed or playback finished.
    {
        let weak = Rc::downgrade(&state);
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(state) = weak.upgrade() else { return glib::ControlFlow::Break };
            let (changed, playback_ended) = {
                let mut s = state.borrow_mut();
                let version = s.history.data_version().unwrap_or(s.seen_version);
                let ended = match &mut s.player {
                    Some((_, child)) => child.try_wait().ok().flatten().is_some(),
                    None => false,
                };
                if ended {
                    s.player = None;
                }
                (version != s.seen_version, ended)
            };
            if changed || playback_ended {
                reload(&state);
            }
            glib::ControlFlow::Continue
        });
    }

    scroller.upcast()
}

fn reload(state: &Shared) {
    let mut s = state.borrow_mut();
    s.seen_version = s.history.data_version().unwrap_or(-1);
    let entries = match s.history.list(&s.query, s.limit, 0) {
        Ok(e) => e,
        Err(e) => {
            log::error!("loading history: {e}");
            Vec::new()
        }
    };
    s.list.remove_all();
    let playing = s.player.as_ref().map(|(id, _)| id.clone());
    for entry in &entries {
        let row = entry_row(state, entry, playing.as_deref() == Some(entry.id.as_str()));
        s.list.append(&row);
    }
    if entries.len() == s.limit {
        let more = gtk::Button::builder().label("Show more").css_classes(["flat"]).margin_top(6).margin_bottom(6).build();
        let state2 = state.clone();
        more.connect_clicked(move |_| {
            state2.borrow_mut().limit += PAGE;
            reload(&state2);
        });
        s.list.append(&more);
    }
    let page = match (entries.is_empty(), s.query.is_empty()) {
        (false, _) => "list",
        (true, true) => "empty",
        (true, false) => "no-results",
    };
    s.stack.set_visible_child_name(page);
}

fn entry_row(state: &Shared, entry: &Entry, playing: bool) -> gtk::ListBoxRow {
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(10)
        .margin_bottom(8)
        .margin_start(14)
        .margin_end(8)
        .build();

    match entry.status {
        Status::Done if entry.text.is_empty() => body.append(&dim_label("No speech detected")),
        Status::Done => {
            let text = gtk::Label::builder()
                .label(&entry.text)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .selectable(true)
                .xalign(0.0)
                .build();
            body.append(&text);
        }
        Status::Pending | Status::Transcribing => {
            let line = gtk::Box::builder().spacing(8).build();
            line.append(&gtk::Spinner::builder().spinning(true).build());
            let label = if entry.status == Status::Pending {
                "Waiting…".to_string()
            } else {
                format!("Transcribing… {:.0}%", entry.progress * 100.0)
            };
            line.append(&dim_label(&label));
            body.append(&line);
        }
        Status::Failed => {
            let label = gtk::Label::builder()
                .label(format!("Failed: {}", entry.error.as_deref().unwrap_or("unknown error")))
                .wrap(true)
                .xalign(0.0)
                .css_classes(["error"])
                .build();
            body.append(&label);
        }
    }

    let footer = gtk::Box::builder().spacing(2).build();
    let caption = gtk::Label::builder()
        .label(caption(entry))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label"])
        .build();
    footer.append(&caption);

    let busy = matches!(entry.status, Status::Pending | Status::Transcribing);
    let copy = icon_button("edit-copy-symbolic", "Copy");
    copy.set_sensitive(!entry.text.is_empty());
    {
        let text = entry.text.clone();
        let state = state.clone();
        copy.connect_clicked(move |b| {
            b.clipboard().set_text(&text);
            state.borrow().toasts.add_toast(adw::Toast::new("Copied"));
        });
    }
    let play = icon_button(
        if playing { "media-playback-stop-symbolic" } else { "media-playback-start-symbolic" },
        if playing { "Stop" } else { "Play" },
    );
    play.set_sensitive(!busy && entry.duration_secs > 0.0);
    {
        let id = entry.id.clone();
        let state = state.clone();
        play.connect_clicked(move |_| toggle_playback(&state, &id));
    }
    let redo = icon_button("view-refresh-symbolic", "Transcribe again with the current model");
    redo.set_sensitive(!busy && entry.duration_secs > 0.0);
    {
        let id = entry.id.clone();
        let state = state.clone();
        redo.connect_clicked(move |_| {
            if let Err(e) = crate::daemon::send(&format!("retranscribe {id}")) {
                state.borrow().toasts.add_toast(adw::Toast::new(&e));
            }
        });
    }
    let delete = icon_button("user-trash-symbolic", "Delete");
    delete.set_sensitive(!busy);
    {
        let id = entry.id.clone();
        let state = state.clone();
        delete.connect_clicked(move |_| {
            stop_playback(&state);
            if let Err(e) = state.borrow().history.delete(&id) {
                log::error!("delete failed: {e}");
            }
            reload(&state);
        });
    }
    for b in [&copy, &play, &redo, &delete] {
        footer.append(b);
    }
    body.append(&footer);

    gtk::ListBoxRow::builder().activatable(false).child(&body).build()
}

fn caption(entry: &Entry) -> String {
    let mut parts = vec![format_time(entry.created_at)];
    if entry.duration_secs > 0.0 {
        parts.push(format_duration(entry.duration_secs));
    }
    if let Some(name) = &entry.source_name {
        parts.push(name.clone());
    }
    if !entry.model.is_empty() {
        parts.push(entry.model.clone());
    }
    parts.join(" · ")
}

fn format_time(ms: i64) -> String {
    let Ok(t) = glib::DateTime::from_unix_local(ms / 1000) else { return String::new() };
    let Ok(now) = glib::DateTime::now_local() else { return String::new() };
    let fmt = if (t.year(), t.day_of_year()) == (now.year(), now.day_of_year()) {
        "Today %H:%M"
    } else if t.year() == now.year() {
        "%b %e, %H:%M"
    } else {
        "%b %e %Y, %H:%M"
    };
    t.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

fn format_duration(secs: f32) -> String {
    if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let s = secs.round() as u32;
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn dim_label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).css_classes(["dim-label"]).build()
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build()
}

fn stop_playback(state: &Shared) {
    if let Some((_, mut child)) = state.borrow_mut().player.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn toggle_playback(state: &Shared, id: &str) {
    let was_playing = state.borrow().player.as_ref().is_some_and(|(p, _)| p == id);
    stop_playback(state);
    if !was_playing {
        let path = state.borrow().history.audio_path(id);
        match std::process::Command::new("pw-play").arg(&path).spawn() {
            Ok(child) => state.borrow_mut().player = Some((id.to_string(), child)),
            Err(e) => state.borrow().toasts.add_toast(adw::Toast::new(&format!("Playback failed: {e}"))),
        }
    }
    reload(state);
}
