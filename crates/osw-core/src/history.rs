//! Transcription history: SQLite (WAL, so the daemon writes while the UI
//! reads) with an FTS5 trigram index for substring search in any language,
//! plus the 16 kHz audio of every entry as a WAV file for playback and
//! re-transcription.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{paths, SAMPLE_RATE};

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("history database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("history audio: {0}")]
    Audio(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = HistoryError> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Transcribing,
    Done,
    Failed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Transcribing => "transcribing",
            Status::Done => "done",
            Status::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "pending" => Status::Pending,
            "transcribing" => Status::Transcribing,
            "done" => Status::Done,
            _ => Status::Failed,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    /// Unix time in milliseconds.
    pub created_at: i64,
    pub duration_secs: f32,
    pub text: String,
    pub status: Status,
    /// 0..1 while transcribing.
    pub progress: f32,
    pub model: String,
    /// Original file name for imported files; `None` for dictations.
    pub source_name: Option<String>,
    pub error: Option<String>,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct History {
    conn: Connection,
    audio_dir: PathBuf,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS recordings (
    id          TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    duration    REAL NOT NULL,
    text        TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL,
    progress    REAL NOT NULL DEFAULT 0,
    model       TEXT NOT NULL DEFAULT '',
    source_name TEXT,
    error       TEXT
);
CREATE INDEX IF NOT EXISTS recordings_created ON recordings(created_at);
CREATE VIRTUAL TABLE IF NOT EXISTS recordings_fts USING fts5(
    text, content='recordings', content_rowid='rowid', tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS recordings_ai AFTER INSERT ON recordings BEGIN
    INSERT INTO recordings_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER IF NOT EXISTS recordings_ad AFTER DELETE ON recordings BEGIN
    INSERT INTO recordings_fts(recordings_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
END;
CREATE TRIGGER IF NOT EXISTS recordings_au AFTER UPDATE OF text ON recordings BEGIN
    INSERT INTO recordings_fts(recordings_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
    INSERT INTO recordings_fts(rowid, text) VALUES (new.rowid, new.text);
END;
";

const COLUMNS: &str =
    "id, created_at, duration, text, status, progress, model, source_name, error";

fn entry_from_row(row: &Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get(0)?,
        created_at: row.get(1)?,
        duration_secs: row.get::<_, f64>(2)? as f32,
        text: row.get(3)?,
        status: Status::parse(&row.get::<_, String>(4)?),
        progress: row.get::<_, f64>(5)? as f32,
        model: row.get(6)?,
        source_name: row.get(7)?,
        error: row.get(8)?,
    })
}

impl History {
    /// Opens `~/.local/share/opensuperwhisper/history.db`.
    pub fn open() -> Result<Self> {
        Self::open_in(&paths::data_dir())
    }

    pub fn open_in(dir: &Path) -> Result<Self> {
        let audio_dir = dir.join("recordings");
        std::fs::create_dir_all(&audio_dir)?;
        let conn = Connection::open(dir.join("history.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, audio_dir })
    }

    pub fn audio_path(&self, id: &str) -> PathBuf {
        self.audio_dir.join(format!("{id}.wav"))
    }

    /// Writes 16 kHz mono audio as 16-bit WAV.
    pub fn save_audio(&self, id: &str, samples: &[f32]) -> Result<PathBuf> {
        let path = self.audio_path(id);
        let tmp = path.with_extension("wav.tmp");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let audio_err = |e: hound::Error| HistoryError::Audio(e.to_string());
        let mut w = hound::WavWriter::create(&tmp, spec).map_err(audio_err)?;
        for &s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).map_err(audio_err)?;
        }
        w.finalize().map_err(audio_err)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn load_audio(&self, id: &str) -> Result<Vec<f32>> {
        let reader = hound::WavReader::open(self.audio_path(id))
            .map_err(|e| HistoryError::Audio(e.to_string()))?;
        reader
            .into_samples::<i16>()
            .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
            .collect::<Result<_, _>>()
            .map_err(|e| HistoryError::Audio(e.to_string()))
    }

    pub fn insert(&self, e: &Entry) -> Result<()> {
        self.conn.execute(
            &format!("INSERT INTO recordings ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
            params![
                e.id,
                e.created_at,
                e.duration_secs as f64,
                e.text,
                e.status.as_str(),
                e.progress as f64,
                e.model,
                e.source_name,
                e.error
            ],
        )?;
        Ok(())
    }

    pub fn set_status(&self, id: &str, status: Status, progress: f32) -> Result<()> {
        self.conn.execute(
            "UPDATE recordings SET status = ?2, progress = ?3 WHERE id = ?1",
            params![id, status.as_str(), progress as f64],
        )?;
        Ok(())
    }

    pub fn set_duration(&self, id: &str, secs: f32) -> Result<()> {
        self.conn.execute("UPDATE recordings SET duration = ?2 WHERE id = ?1", params![id, secs as f64])?;
        Ok(())
    }

    pub fn set_result(&self, id: &str, result: std::result::Result<(&str, &str), &str>) -> Result<()> {
        match result {
            Ok((text, model)) => self.conn.execute(
                "UPDATE recordings SET status = 'done', progress = 1, text = ?2, model = ?3, error = NULL
                 WHERE id = ?1",
                params![id, text, model],
            )?,
            Err(error) => self.conn.execute(
                "UPDATE recordings SET status = 'failed', progress = 0, error = ?2 WHERE id = ?1",
                params![id, error],
            )?,
        };
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<Entry>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {COLUMNS} FROM recordings WHERE id = ?1"), [id], entry_from_row)
            .optional()?)
    }

    /// Newest first. A query of 3+ characters uses the trigram index
    /// (substring match); shorter queries fall back to LIKE.
    pub fn list(&self, query: &str, limit: usize, offset: usize) -> Result<Vec<Entry>> {
        let query = query.trim();
        let (sql, arg) = if query.is_empty() {
            (format!("SELECT {COLUMNS} FROM recordings ORDER BY created_at DESC LIMIT ?1 OFFSET ?2"), None)
        } else if query.chars().count() >= 3 {
            let phrase = format!("\"{}\"", query.replace('"', "\"\""));
            (
                format!(
                    "SELECT {} FROM recordings r JOIN recordings_fts f ON f.rowid = r.rowid
                     WHERE recordings_fts MATCH ?3 ORDER BY r.created_at DESC LIMIT ?1 OFFSET ?2",
                    COLUMNS.split(", ").map(|c| format!("r.{c}")).collect::<Vec<_>>().join(", ")
                ),
                Some(phrase),
            )
        } else {
            let pattern = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            (
                format!(
                    "SELECT {COLUMNS} FROM recordings WHERE text LIKE ?3 ESCAPE '\\'
                     ORDER BY created_at DESC LIMIT ?1 OFFSET ?2"
                ),
                Some(pattern),
            )
        };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = match arg {
            None => stmt.query_map(params![limit as i64, offset as i64], entry_from_row)?,
            Some(a) => stmt.query_map(params![limit as i64, offset as i64, a], entry_from_row)?,
        };
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM recordings WHERE id = ?1", [id])?;
        let _ = std::fs::remove_file(self.audio_path(id));
        Ok(())
    }

    pub fn delete_all(&self) -> Result<()> {
        let ids: Vec<String> = self
            .conn
            .prepare("SELECT id FROM recordings")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        self.conn.execute("DELETE FROM recordings", [])?;
        for id in ids {
            let _ = std::fs::remove_file(self.audio_path(&id));
        }
        Ok(())
    }

    /// Deletes entries older than `days`; returns how many were removed.
    pub fn prune_older_than(&self, days: u32) -> Result<usize> {
        let cutoff = now_ms() - days as i64 * 86_400_000;
        let ids: Vec<String> = self
            .conn
            .prepare("SELECT id FROM recordings WHERE created_at < ?1")?
            .query_map([cutoff], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in &ids {
            self.delete(id)?;
        }
        Ok(ids.len())
    }

    /// Entries a crashed or killed daemon left unfinished.
    pub fn mark_interrupted(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE recordings SET status = 'failed', progress = 0, error = 'interrupted'
             WHERE status IN ('pending', 'transcribing')",
            [],
        )?)
    }

    /// Changes whenever another connection commits; cheap to poll.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    pub fn audio_disk_usage(&self) -> u64 {
        std::fs::read_dir(&self.audio_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.metadata().ok())
            .map(|m| m.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_history() -> (History, PathBuf) {
        let dir = std::env::temp_dir().join(format!("osw-history-{}", new_id()));
        (History::open_in(&dir).unwrap(), dir)
    }

    fn entry(text: &str, created_at: i64) -> Entry {
        Entry {
            id: new_id(),
            created_at,
            duration_secs: 1.5,
            text: text.into(),
            status: Status::Done,
            progress: 1.0,
            model: "m".into(),
            source_name: None,
            error: None,
        }
    }

    #[test]
    fn insert_list_search_delete() {
        let (h, dir) = temp_history();
        let a = entry("Hello world from Linux", 1);
        let b = entry("你好世界，今天天气很好", 2);
        let c = entry("ask not what your country", 3);
        for e in [&a, &b, &c] {
            h.insert(e).unwrap();
        }
        let all = h.list("", 10, 0).unwrap();
        assert_eq!(all.iter().map(|e| &e.id).collect::<Vec<_>>(), [&c.id, &b.id, &a.id]);
        assert_eq!(h.list("world", 10, 0).unwrap(), vec![a.clone()]);
        assert_eq!(h.list("WORLD", 10, 0).unwrap().len(), 1, "case-insensitive");
        assert_eq!(h.list("天气很", 10, 0).unwrap(), vec![b.clone()], "CJK substring");
        assert_eq!(h.list("天气", 10, 0).unwrap(), vec![b.clone()], "short query via LIKE");
        assert_eq!(h.list("countr", 10, 0).unwrap(), vec![c.clone()], "prefix");
        assert!(h.list("\"quoted", 10, 0).unwrap().is_empty());

        h.delete(&a.id).unwrap();
        assert!(h.list("world", 10, 0).unwrap().is_empty(), "FTS follows deletes");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn results_update_text_and_index() {
        let (h, dir) = temp_history();
        let mut e = entry("", now_ms());
        e.status = Status::Pending;
        h.insert(&e).unwrap();
        h.set_status(&e.id, Status::Transcribing, 0.5).unwrap();
        assert_eq!(h.get(&e.id).unwrap().unwrap().progress, 0.5);
        h.set_result(&e.id, Ok(("fresh text", "turbo"))).unwrap();
        let got = h.get(&e.id).unwrap().unwrap();
        assert_eq!((got.status, got.text.as_str(), got.model.as_str()), (Status::Done, "fresh text", "turbo"));
        assert_eq!(h.list("fresh", 10, 0).unwrap().len(), 1);
        h.set_result(&e.id, Err("boom")).unwrap();
        assert_eq!(h.get(&e.id).unwrap().unwrap().error.as_deref(), Some("boom"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn audio_round_trip_and_prune() {
        let (h, dir) = temp_history();
        let old = entry("old", now_ms() - 40 * 86_400_000);
        let new = entry("new", now_ms());
        h.insert(&old).unwrap();
        h.insert(&new).unwrap();
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 1600.0) - 0.5).collect();
        h.save_audio(&old.id, &samples).unwrap();
        let back = h.load_audio(&old.id).unwrap();
        assert_eq!(back.len(), samples.len());
        assert!(back.iter().zip(&samples).all(|(a, b)| (a - b).abs() < 1e-3));

        assert_eq!(h.prune_older_than(30).unwrap(), 1);
        assert!(!h.audio_path(&old.id).exists());
        assert_eq!(h.list("", 10, 0).unwrap(), vec![new]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn data_version_changes_on_foreign_commit() {
        let (writer, dir) = temp_history();
        let reader = History::open_in(&dir).unwrap();
        let v = reader.data_version().unwrap();
        writer.insert(&entry("x", 1)).unwrap();
        assert_ne!(reader.data_version().unwrap(), v);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
