//! Single writer for the history database, off the controller thread.

use crossbeam_channel as cb;
use osw_core::history::{Entry, History, Status};

pub enum HistoryCmd {
    Insert(Entry),
    SaveAudio { id: String, samples: Vec<f32> },
    SetStatus { id: String, status: Status, progress: f32 },
    SetDuration { id: String, secs: f32 },
    SetResult { id: String, result: Result<(String, String), String> },
    Prune { days: u32 },
}

pub fn spawn() -> anyhow::Result<cb::Sender<HistoryCmd>> {
    let history = History::open()?;
    match history.mark_interrupted() {
        Ok(0) => {}
        Ok(n) => log::info!("marked {n} unfinished history entries as interrupted"),
        Err(e) => log::warn!("history cleanup failed: {e}"),
    }
    let (tx, rx) = cb::unbounded();
    std::thread::Builder::new().name("history".into()).spawn(move || {
        for cmd in rx {
            let result = match cmd {
                HistoryCmd::Insert(e) => history.insert(&e),
                HistoryCmd::SaveAudio { id, samples } => history.save_audio(&id, &samples).map(drop),
                HistoryCmd::SetStatus { id, status, progress } => history.set_status(&id, status, progress),
                HistoryCmd::SetDuration { id, secs } => history.set_duration(&id, secs),
                HistoryCmd::SetResult { id, result } => match &result {
                    Ok((text, model)) => history.set_result(&id, Ok((text, model))),
                    Err(e) => history.set_result(&id, Err(e)),
                },
                HistoryCmd::Prune { days } => history.prune_older_than(days).map(|n| {
                    if n > 0 {
                        log::info!("deleted {n} history entries older than {days} days");
                    }
                }),
            };
            if let Err(e) = result {
                log::error!("history: {e}");
            }
        }
    })?;
    Ok(tx)
}
