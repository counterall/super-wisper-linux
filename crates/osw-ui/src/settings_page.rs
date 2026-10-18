//! Settings: edits `config.toml` and asks the daemon to reload it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use osw_core::config::{Config, ModifierKey, Trigger};
use osw_core::history::History;
use osw_core::models::{self, CatalogModel};
use osw_core::settings::LANGUAGES;

type Shared = Rc<RefCell<Config>>;

/// Writes the config and applies it in the running daemon.
fn save(config: &Shared, toasts: &adw::ToastOverlay) {
    if let Err(e) = config.borrow().save_to(&Config::path()) {
        toasts.add_toast(adw::Toast::new(&format!("Could not save settings: {e}")));
        return;
    }
    let _ = crate::daemon::send("reload");
}

/// Updates one config field from a widget and saves.
fn on_change<W: 'static>(
    config: &Shared,
    toasts: &adw::ToastOverlay,
    update: impl Fn(&mut Config, &W) + 'static,
) -> impl Fn(&W) + 'static {
    let config = config.clone();
    let toasts = toasts.clone();
    move |w| {
        update(&mut config.borrow_mut(), w);
        save(&config, &toasts);
    }
}

pub fn build(window: &adw::ApplicationWindow, toasts: &adw::ToastOverlay) -> gtk::Widget {
    let config: Shared = Rc::new(RefCell::new(Config::load_or_create().unwrap_or_else(|e| {
        log::error!("{e}; showing defaults");
        Config::default()
    })));
    let page = adw::PreferencesPage::new();
    page.add(&model_group(&config, toasts));
    page.add(&recording_group(&config, toasts));
    page.add(&text_group(&config, toasts));
    page.add(&history_group(window, &config, toasts));
    page.add(&advanced_group(&config, toasts));
    page.upcast()
}

// --- model ------------------------------------------------------------------

fn model_group(config: &Shared, toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Whisper model")
        .description("Larger models are more accurate; Turbo V3 small is fast on a GPU.")
        .build();
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    populate_models(&group, &rows, config, toasts);

    let gpu = adw::SwitchRow::builder()
        .title("GPU acceleration")
        .subtitle("Much faster; turn off if the GPU misbehaves")
        .active(config.borrow().use_gpu)
        .build();
    gpu.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| c.use_gpu = w.is_active()));
    group.add(&gpu);
    group
}

/// (Re)builds one row per catalog model plus any other installed model.
fn populate_models(
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    config: &Shared,
    toasts: &adw::ToastOverlay,
) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    let selected = config
        .borrow()
        .model_path()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()));
    let installed: Vec<String> = models::installed()
        .iter()
        .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
        .collect();

    let mut entries: Vec<(String, String, Option<&'static CatalogModel>)> = models::CATALOG
        .iter()
        .map(|m| (m.filename.to_string(), m.name.to_string(), Some(m)))
        .collect();
    entries.extend(
        installed
            .iter()
            .filter(|f| models::find(f).is_none())
            .map(|f| (f.clone(), f.clone(), None)),
    );

    let group_anchor = gtk::CheckButton::new(); // radio group leader, never shown
    let mut new_rows = Vec::new();
    for (filename, name, catalog) in entries {
        let row = adw::ActionRow::builder().title(&name).build();
        if let Some(m) = catalog {
            row.set_subtitle(&format!("{} · {}", m.description, size_label(m.size_mb)));
        }
        if installed.contains(&filename) {
            let radio = gtk::CheckButton::builder().valign(gtk::Align::Center).build();
            radio.set_group(Some(&group_anchor));
            radio.set_active(selected.as_deref() == Some(filename.as_str()));
            let config2 = config.clone();
            let toasts2 = toasts.clone();
            radio.connect_toggled(move |r| {
                if r.is_active() {
                    config2.borrow_mut().model = Some(filename.clone());
                    save(&config2, &toasts2);
                    toasts2.add_toast(adw::Toast::new("Loading model…"));
                }
            });
            row.add_prefix(&radio);
            row.set_activatable_widget(Some(&radio));
        } else if let Some(m) = catalog {
            // Same indent as the radio buttons of installed models.
            let icon = gtk::Image::builder()
                .icon_name("folder-download-symbolic")
                .width_request(30)
                .css_classes(["dim-label"])
                .build();
            row.add_prefix(&icon);
            row.add_suffix(&download_widget(m, group, rows, config, toasts));
        }
        group.add(&row);
        new_rows.push(row);
    }
    *rows.borrow_mut() = new_rows;
}

fn size_label(mb: u32) -> String {
    if mb >= 1000 {
        format!("{:.1} GB", mb as f32 / 1000.0)
    } else {
        format!("{mb} MB")
    }
}

/// "Download" button that turns into a progress bar with a cancel button.
fn download_widget(
    model: &'static CatalogModel,
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    config: &Shared,
    toasts: &adw::ToastOverlay,
) -> gtk::Widget {
    let stack = gtk::Stack::builder().valign(gtk::Align::Center).build();
    let button = gtk::Button::builder().label("Download").css_classes(["flat"]).build();
    let progress = gtk::ProgressBar::builder().width_request(120).valign(gtk::Align::Center).build();
    let cancel = gtk::Button::builder()
        .icon_name("process-stop-symbolic")
        .tooltip_text("Cancel download")
        .css_classes(["flat", "circular"])
        .build();
    let busy = gtk::Box::builder().spacing(6).build();
    busy.append(&progress);
    busy.append(&cancel);
    stack.add_named(&button, Some("idle"));
    stack.add_named(&busy, Some("busy"));

    let flag = Arc::new(AtomicBool::new(false));
    {
        let flag = flag.clone();
        cancel.connect_clicked(move |_| flag.store(true, Ordering::Relaxed));
    }
    let (group, rows, config, toasts) = (group.clone(), rows.clone(), config.clone(), toasts.clone());
    let widget = stack.clone();
    button.connect_clicked(move |_| {
        stack.set_visible_child_name("busy");
        flag.store(false, Ordering::Relaxed);
        let (tx, rx) = async_channel::unbounded::<Result<f64, Option<String>>>();
        let flag = flag.clone();
        std::thread::spawn(move || {
            let mut last = 0.0;
            let result = models::download(model, &flag, |done, total| {
                if let Some(t) = total {
                    let f = done as f64 / t as f64;
                    if f - last >= 0.005 {
                        last = f;
                        let _ = tx.send_blocking(Ok(f));
                    }
                }
            });
            let _ = tx.send_blocking(match result {
                Ok(_) => Err(None),
                Err(osw_core::Error::Cancelled) => Err(Some(String::new())),
                Err(e) => Err(Some(e.to_string())),
            });
        });
        let (stack, progress) = (stack.clone(), progress.clone());
        let (group, rows, config, toasts) = (group.clone(), rows.clone(), config.clone(), toasts.clone());
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    Ok(fraction) => progress.set_fraction(fraction),
                    Err(None) => {
                        toasts.add_toast(adw::Toast::new(&format!("{} downloaded", model.name)));
                        populate_models(&group, &rows, &config, &toasts);
                        return;
                    }
                    Err(Some(e)) => {
                        if !e.is_empty() {
                            toasts.add_toast(adw::Toast::new(&format!("Download failed: {e}")));
                        }
                        stack.set_visible_child_name("idle");
                        return;
                    }
                }
            }
        });
    });
    widget.upcast()
}

// --- recording ----------------------------------------------------------------

const MODIFIERS: [(ModifierKey, &str); 8] = [
    (ModifierKey::RightCtrl, "Right Ctrl"),
    (ModifierKey::LeftCtrl, "Left Ctrl"),
    (ModifierKey::RightAlt, "Right Alt"),
    (ModifierKey::LeftAlt, "Left Alt"),
    (ModifierKey::RightShift, "Right Shift"),
    (ModifierKey::LeftShift, "Left Shift"),
    (ModifierKey::RightSuper, "Right Super"),
    (ModifierKey::LeftSuper, "Left Super"),
];
const MOUSE: [(u8, &str); 3] = [(8, "Mouse back button"), (9, "Mouse forward button"), (2, "Middle mouse button")];

fn trigger_options() -> Vec<(Trigger, String)> {
    let mut v: Vec<(Trigger, String)> = MODIFIERS
        .iter()
        .map(|(k, n)| (Trigger::ModifierKey { key: *k }, format!("{n} alone")))
        .collect();
    v.push((Trigger::Shortcut { keys: String::new() }, "Key combination".into()));
    v.extend(MOUSE.iter().map(|(b, n)| (Trigger::MouseButton { button: *b }, n.to_string())));
    v.push((Trigger::None, "None (bind `osw toggle` in system settings)".into()));
    v
}

fn trigger_index(options: &[(Trigger, String)], t: &Trigger) -> usize {
    options
        .iter()
        .position(|(o, _)| match (o, t) {
            (Trigger::Shortcut { .. }, Trigger::Shortcut { .. }) => true,
            _ => o == t,
        })
        .unwrap_or(0)
}

fn recording_group(config: &Shared, toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Recording").build();
    let options = Rc::new(trigger_options());
    let labels: Vec<&str> = options.iter().map(|(_, l)| l.as_str()).collect();
    let current = config.borrow().trigger.clone();

    let trigger = adw::ComboRow::builder()
        .title("Start/stop with")
        .model(&gtk::StringList::new(&labels))
        .selected(trigger_index(&options, &current) as u32)
        .build();
    let keys = adw::EntryRow::builder()
        .title("Key combination, e.g. Ctrl+Alt+d")
        .show_apply_button(true)
        .visible(matches!(current, Trigger::Shortcut { .. }))
        .build();
    if let Trigger::Shortcut { keys: k } = &current {
        keys.set_text(k);
    }
    {
        let options = options.clone();
        let keys = keys.clone();
        trigger.connect_selected_notify(on_change(config, toasts, move |c, w: &adw::ComboRow| {
            let (t, _) = &options[w.selected() as usize];
            let is_shortcut = matches!(t, Trigger::Shortcut { .. });
            keys.set_visible(is_shortcut);
            c.trigger = match t {
                Trigger::Shortcut { .. } => Trigger::Shortcut { keys: keys.text().to_string() },
                other => other.clone(),
            };
            // An empty combination is not usable yet: wait for "apply".
            if is_shortcut && keys.text().is_empty() {
                c.trigger = Trigger::None;
            }
        }));
    }
    keys.connect_apply(on_change(config, toasts, |c, w: &adw::EntryRow| {
        c.trigger = Trigger::Shortcut { keys: w.text().trim().to_string() };
    }));
    group.add(&trigger);
    group.add(&keys);

    let hold = adw::SwitchRow::builder()
        .title("Hold to record")
        .subtitle("Holding the trigger records until release; a quick tap toggles")
        .active(config.borrow().hold_to_record)
        .build();
    hold.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| c.hold_to_record = w.is_active()));
    group.add(&hold);

    // Microphones come from the daemon (it watches PipeWire).
    let mics = crate::daemon::microphones();
    let mut mic_labels = vec!["System default".to_string()];
    mic_labels.extend(mics.iter().map(|(_, d)| d.clone()));
    let mut mic_names: Vec<Option<String>> = vec![None];
    mic_names.extend(mics.iter().map(|(n, _)| Some(n.clone())));
    let configured = config.borrow().microphone.clone();
    if let Some(name) = &configured {
        if !mic_names.contains(&Some(name.clone())) {
            mic_labels.push(format!("{name} (not connected)"));
            mic_names.push(Some(name.clone()));
        }
    }
    let selected = mic_names.iter().position(|n| *n == configured).unwrap_or(0);
    let mic = adw::ComboRow::builder()
        .title("Microphone")
        .model(&gtk::StringList::new(&mic_labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(selected as u32)
        .build();
    mic.connect_selected_notify(on_change(config, toasts, move |c, w: &adw::ComboRow| {
        c.microphone = mic_names.get(w.selected() as usize).cloned().flatten();
    }));
    group.add(&mic);

    let esc = adw::SwitchRow::builder()
        .title("Cancel with a single Esc")
        .subtitle("Otherwise press Esc twice")
        .active(config.borrow().esc_cancel_without_confirmation)
        .build();
    esc.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| {
        c.esc_cancel_without_confirmation = w.is_active()
    }));
    group.add(&esc);

    let paste = adw::SwitchRow::builder()
        .title("Paste into the active app")
        .active(config.borrow().auto_paste)
        .build();
    paste.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| c.auto_paste = w.is_active()));
    group.add(&paste);

    let keep = adw::SwitchRow::builder()
        .title("Keep text in the clipboard")
        .subtitle("Otherwise the previous clipboard contents are restored")
        .active(config.borrow().keep_in_clipboard)
        .build();
    keep.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| c.keep_in_clipboard = w.is_active()));
    group.add(&keep);
    group
}

// --- text -----------------------------------------------------------------------

fn text_group(config: &Shared, toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Transcription").build();

    let mut labels = vec!["Detect automatically"];
    labels.extend(LANGUAGES.iter().map(|(_, n)| *n));
    let current = config.borrow().transcription.language.clone();
    let selected = LANGUAGES.iter().position(|(c, _)| *c == current).map_or(0, |i| i + 1);
    let lang = adw::ComboRow::builder()
        .title("Language")
        .model(&gtk::StringList::new(&labels))
        .selected(selected as u32)
        .enable_search(true)
        .build();
    lang.connect_selected_notify(on_change(config, toasts, |c, w: &adw::ComboRow| {
        c.transcription.language = match w.selected() as usize {
            0 => "auto".into(),
            i => LANGUAGES[i - 1].0.into(),
        };
    }));
    group.add(&lang);

    let prompt = adw::EntryRow::builder()
        .title("Vocabulary hint (names, terms, style)")
        .text(&config.borrow().transcription.initial_prompt)
        .show_apply_button(true)
        .build();
    prompt.connect_apply(on_change(config, toasts, |c, w: &adw::EntryRow| {
        c.transcription.initial_prompt = w.text().to_string()
    }));
    group.add(&prompt);

    let space = adw::SwitchRow::builder()
        .title("Add a space after sentences")
        .subtitle("So consecutive dictations do not run together")
        .active(config.borrow().add_space_after_sentence)
        .build();
    space.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| {
        c.add_space_after_sentence = w.is_active()
    }));
    group.add(&space);

    let cjk = adw::SwitchRow::builder()
        .title("Chinese/Japanese/Korean spacing fixes")
        .subtitle("Adds spaces between CJK and Latin text, fixes punctuation")
        .active(config.borrow().transcription.asian_autocorrect)
        .build();
    cjk.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| {
        c.transcription.asian_autocorrect = w.is_active()
    }));
    group.add(&cjk);
    group
}

// --- history --------------------------------------------------------------------

const RETENTION: [(Option<u32>, &str); 5] =
    [(None, "Forever"), (Some(7), "1 week"), (Some(30), "1 month"), (Some(90), "3 months"), (Some(365), "1 year")];

fn history_group(window: &adw::ApplicationWindow, config: &Shared, toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("History").build();

    let save_history = adw::SwitchRow::builder()
        .title("Save dictations")
        .subtitle("Text and audio, for search, playback and re-transcription")
        .active(config.borrow().save_history)
        .build();
    save_history.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| c.save_history = w.is_active()));
    group.add(&save_history);

    let current = config.borrow().history_retention_days;
    let retention = adw::ComboRow::builder()
        .title("Keep history for")
        .model(&gtk::StringList::new(&RETENTION.map(|(_, l)| l)))
        .selected(RETENTION.iter().position(|(d, _)| *d == current).unwrap_or(0) as u32)
        .build();
    retention.connect_selected_notify(on_change(config, toasts, |c, w: &adw::ComboRow| {
        c.history_retention_days = RETENTION[w.selected() as usize].0;
    }));
    group.add(&retention);

    let usage = gtk::Label::builder().css_classes(["dim-label"]).build();
    let refresh_usage = {
        let usage = usage.clone();
        move || {
            let bytes = History::open().map(|h| h.audio_disk_usage()).unwrap_or(0);
            usage.set_label(&format!("{:.1} MB", bytes as f64 / 1e6));
        }
    };
    refresh_usage();
    let storage = adw::ActionRow::builder().title("Audio storage used").build();
    storage.add_suffix(&usage);
    group.add(&storage);

    let delete = gtk::Button::builder()
        .label("Delete all…")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .build();
    let clear = adw::ActionRow::builder().title("Delete all history").build();
    clear.add_suffix(&delete);
    group.add(&clear);
    {
        let window = window.clone();
        let toasts = toasts.clone();
        delete.connect_clicked(move |_| {
            let dialog = adw::AlertDialog::new(
                Some("Delete all history?"),
                Some("All transcriptions and their audio will be removed permanently."),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("delete", "Delete");
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            let toasts = toasts.clone();
            let refresh_usage = refresh_usage.clone();
            dialog.connect_response(None, move |_, response| {
                if response != "delete" {
                    return;
                }
                match History::open().and_then(|h| h.delete_all()) {
                    Ok(()) => toasts.add_toast(adw::Toast::new("History deleted")),
                    Err(e) => toasts.add_toast(adw::Toast::new(&format!("Delete failed: {e}"))),
                }
                refresh_usage();
            });
            dialog.present(Some(&window));
        });
    }
    group
}

// --- advanced -------------------------------------------------------------------

fn advanced_group(config: &Shared, toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Advanced").build();
    let t = config.borrow().transcription.clone();

    let temperature = adw::SpinRow::with_range(0.0, 1.0, 0.1);
    temperature.set_title("Temperature");
    temperature.set_subtitle("0 is the most deterministic");
    temperature.set_digits(1);
    temperature.set_value(t.temperature as f64);
    temperature.connect_value_notify(on_change(config, toasts, |c, w: &adw::SpinRow| {
        c.transcription.temperature = w.value() as f32
    }));
    group.add(&temperature);

    let no_speech = adw::SpinRow::with_range(0.0, 1.0, 0.05);
    no_speech.set_title("No-speech threshold");
    no_speech.set_digits(2);
    no_speech.set_value(t.no_speech_threshold as f64);
    no_speech.connect_value_notify(on_change(config, toasts, |c, w: &adw::SpinRow| {
        c.transcription.no_speech_threshold = w.value() as f32
    }));
    group.add(&no_speech);

    let beam = adw::SwitchRow::builder()
        .title("Beam search")
        .subtitle("Slightly more accurate, noticeably slower")
        .active(t.beam_search)
        .build();
    beam.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| c.transcription.beam_search = w.is_active()));
    group.add(&beam);

    let beam_size = adw::SpinRow::with_range(1.0, 10.0, 1.0);
    beam_size.set_title("Beam size");
    beam_size.set_value(t.beam_size as f64);
    beam_size.connect_value_notify(on_change(config, toasts, |c, w: &adw::SpinRow| {
        c.transcription.beam_size = w.value() as u32
    }));
    group.add(&beam_size);

    let stamps = adw::SwitchRow::builder()
        .title("Timestamps")
        .subtitle("Prefix each segment with [start->end]")
        .active(t.show_timestamps)
        .build();
    stamps.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| {
        c.transcription.show_timestamps = w.is_active()
    }));
    group.add(&stamps);

    let warm = adw::SwitchRow::builder()
        .title("Keep microphone ready")
        .subtitle("Recording starts instantly but the microphone stays in use (restart needed)")
        .active(config.borrow().keep_mic_warm)
        .build();
    warm.connect_active_notify(on_change(config, toasts, |c, w: &adw::SwitchRow| c.keep_mic_warm = w.is_active()));
    group.add(&warm);
    group
}
