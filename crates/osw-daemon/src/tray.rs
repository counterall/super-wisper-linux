//! System tray icon (StatusNotifierItem; Cinnamon shows it via xapp).

use std::sync::{Arc, Mutex};

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{RadioGroup, RadioItem, StandardItem};
use ksni::MenuItem;

use crate::capture::Microphone;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayStatus {
    Idle,
    Recording,
    Transcribing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayCmd {
    Toggle,
    SelectMicrophone(Option<String>),
    OpenConfig,
    Reload,
    Quit,
}

pub struct Tray {
    pub status: TrayStatus,
    pub model: String,
    pub microphone: Option<String>,
    microphones: Arc<Mutex<Vec<Microphone>>>,
    on_cmd: Box<dyn Fn(TrayCmd) + Send>,
}

impl Tray {
    pub fn spawn(
        microphone: Option<String>,
        microphones: Arc<Mutex<Vec<Microphone>>>,
        on_cmd: impl Fn(TrayCmd) + Send + 'static,
    ) -> Option<Handle<Tray>> {
        let tray = Tray {
            status: TrayStatus::Idle,
            model: String::new(),
            microphone,
            microphones,
            on_cmd: Box::new(on_cmd),
        };
        match tray.spawn() {
            Ok(handle) => Some(handle),
            Err(e) => {
                log::warn!("tray icon unavailable: {e}");
                None
            }
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "opensuperwhisper".into()
    }

    fn title(&self) -> String {
        "OpenSuperWhisper".into()
    }

    fn icon_name(&self) -> String {
        match self.status {
            TrayStatus::Idle => "audio-input-microphone-symbolic",
            TrayStatus::Recording => "media-record-symbolic",
            TrayStatus::Transcribing => "content-loading-symbolic",
        }
        .into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "OpenSuperWhisper".into(),
            description: match self.status {
                TrayStatus::Idle => format!("Ready - {}", self.model),
                TrayStatus::Recording => "Recording".into(),
                TrayStatus::Transcribing => "Transcribing".into(),
            },
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.on_cmd)(TrayCmd::Toggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut mics = self.microphones.lock().unwrap().clone();
        mics.sort_by(|a, b| a.description.cmp(&b.description));
        let selected = match &self.microphone {
            None => 0,
            Some(name) => mics.iter().position(|m| &m.name == name).map_or(0, |i| i + 1),
        };
        let mut options = vec![RadioItem { label: "System default".into(), ..Default::default() }];
        options.extend(mics.iter().map(|m| RadioItem { label: m.description.clone(), ..Default::default() }));
        let names: Vec<Option<String>> =
            std::iter::once(None).chain(mics.iter().map(|m| Some(m.name.clone()))).collect();

        let recording = self.status == TrayStatus::Recording;
        vec![
            StandardItem {
                label: if recording { "Stop recording" } else { "Start recording" }.into(),
                icon_name: if recording { "media-playback-stop" } else { "media-record" }.into(),
                activate: Box::new(|t: &mut Self| (t.on_cmd)(TrayCmd::Toggle)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem { label: "Microphone".into(), enabled: false, ..Default::default() }.into(),
            RadioGroup {
                selected,
                select: Box::new(move |t: &mut Self, i| {
                    let name = names.get(i).cloned().flatten();
                    t.microphone = name.clone();
                    (t.on_cmd)(TrayCmd::SelectMicrophone(name));
                }),
                options,
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: format!("Model: {}", self.model),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Edit settings…".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|t: &mut Self| (t.on_cmd)(TrayCmd::OpenConfig)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Reload settings".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|t: &mut Self| (t.on_cmd)(TrayCmd::Reload)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| (t.on_cmd)(TrayCmd::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}
