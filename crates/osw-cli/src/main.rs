use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use osw_core::engine::CancelToken;
use osw_core::{audio, models, paths, EngineOptions, TranscribeSettings, WhisperEngine};

#[derive(Parser)]
#[command(name = "osw", about = "OpenSuperWhisper command line tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Transcribe an audio file and print the text.
    Transcribe {
        file: PathBuf,
        #[command(flatten)]
        engine: EngineArgs,
        /// Prefix each segment with [start->end] timestamps.
        #[arg(long)]
        timestamps: bool,
    },
    /// Measure load, warm-up and transcription latency for one or more models.
    Bench {
        file: PathBuf,
        #[command(flatten)]
        engine: EngineArgs,
        /// Timed runs per model (after warm-up).
        #[arg(long, default_value_t = 5)]
        runs: usize,
    },
    /// List the model catalog and installed models.
    Models,
}

#[derive(Args)]
struct EngineArgs {
    /// Model path or filename in the models directory; repeatable for bench.
    #[arg(short, long)]
    model: Vec<String>,
    /// Language code, or "auto".
    #[arg(short, long, default_value = "en")]
    lang: String,
    #[arg(long, default_value = "")]
    prompt: String,
    /// Run on CPU only.
    #[arg(long)]
    cpu: bool,
    #[arg(long)]
    no_flash_attn: bool,
    #[arg(short = 't', long)]
    threads: Option<usize>,
}

impl EngineArgs {
    fn options(&self) -> EngineOptions {
        let mut opts = EngineOptions::default();
        opts.use_gpu &= !self.cpu;
        opts.flash_attn = opts.use_gpu && !self.no_flash_attn;
        if let Some(t) = self.threads {
            opts.n_threads = t;
        }
        opts
    }

    fn settings(&self, model: &Path) -> TranscribeSettings {
        let file = model.file_name().and_then(|f| f.to_str()).unwrap_or("");
        let language = models::preferred_language(file).unwrap_or(&self.lang).to_string();
        TranscribeSettings { language, initial_prompt: self.prompt.clone(), ..Default::default() }
    }

    fn model_paths(&self) -> Result<Vec<PathBuf>> {
        if self.model.is_empty() {
            let Some(model) = models::default_model() else {
                bail!("no model given and none installed in {}", paths::models_dir().display());
            };
            return Ok(vec![model]);
        }
        self.model
            .iter()
            .map(|m| {
                let direct = PathBuf::from(m);
                let in_dir = paths::models_dir().join(m);
                [direct, in_dir]
                    .into_iter()
                    .find(|p| p.is_file())
                    .with_context(|| format!("model not found: {m}"))
            })
            .collect()
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    match Cli::parse().command {
        Command::Transcribe { file, engine, timestamps } => transcribe(&file, &engine, timestamps),
        Command::Bench { file, engine, runs } => bench(&file, &engine, runs),
        Command::Models => list_models(),
    }
}

fn load_audio(file: &Path) -> Result<(Vec<f32>, Duration)> {
    let t = Instant::now();
    let samples = audio::load_file(file).with_context(|| format!("decoding {}", file.display()))?;
    Ok((samples, t.elapsed()))
}

fn transcribe(file: &Path, args: &EngineArgs, timestamps: bool) -> Result<()> {
    let model = args.model_paths()?.remove(0);
    let engine = WhisperEngine::load(&model, &args.options())?;
    let (samples, _) = load_audio(file)?;
    let mut settings = args.settings(&model);
    settings.show_timestamps = timestamps;
    let result = engine.transcribe(&samples, &settings, &CancelToken::default(), None)?;
    println!("{}", result.text);
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn bench(file: &Path, args: &EngineArgs, runs: usize) -> Result<()> {
    let runs = runs.max(1);
    let opts = args.options();
    let (samples, decode_time) = load_audio(file)?;
    let audio_secs = samples.len() as f64 / osw_core::SAMPLE_RATE as f64;
    println!(
        "audio: {} ({audio_secs:.2}s, decoded+resampled in {:.1} ms)",
        file.display(),
        ms(decode_time)
    );
    println!(
        "options: gpu={} flash_attn={} threads={}\n",
        opts.use_gpu, opts.flash_attn, opts.n_threads
    );

    for model in args.model_paths()? {
        let name = model.file_name().unwrap_or_default().to_string_lossy();
        let settings = args.settings(&model);

        let t = Instant::now();
        let engine = WhisperEngine::load(&model, &opts)?;
        let load = t.elapsed();
        let t = Instant::now();
        engine.warm_up()?;
        let warm = t.elapsed();

        let mut totals = Vec::with_capacity(runs);
        let mut decodes = Vec::with_capacity(runs);
        let mut vads = Vec::with_capacity(runs);
        let mut states = Vec::with_capacity(runs);
        let mut text = String::new();
        for _ in 0..runs {
            // A real dictation prepares the state when recording starts.
            engine.prepare()?;
            let t = Instant::now();
            let r = engine.transcribe(&samples, &settings, &CancelToken::default(), None)?;
            totals.push(ms(t.elapsed()));
            decodes.push(ms(r.stats.decode));
            vads.push(ms(r.stats.vad));
            states.push(ms(r.stats.state_init));
            text = r.text;
        }
        let total = median(&mut totals);
        println!("== {name}");
        println!("  load {:.0} ms, warm-up {:.0} ms", ms(load), ms(warm));
        println!(
            "  median of {runs}: total {total:.0} ms (vad {:.1}, state {:.1}, decode {:.0}) -> {:.1}x realtime",
            median(&mut vads),
            median(&mut states),
            median(&mut decodes),
            audio_secs * 1000.0 / total
        );
        println!("  text: {text}\n");
    }
    Ok(())
}

fn list_models() -> Result<()> {
    let dir = paths::models_dir();
    println!("models directory: {}\n", dir.display());
    for m in models::CATALOG {
        let mark = if dir.join(m.filename).is_file() { "*" } else { " " };
        println!("{mark} {:<18} {:>5} MB  {:<32} {}", m.name, m.size_mb, m.filename, m.description);
    }
    let extra: Vec<_> = models::installed()
        .into_iter()
        .filter(|p| {
            let f = p.file_name().and_then(|f| f.to_str()).unwrap_or("");
            models::find(f).is_none()
        })
        .collect();
    for p in extra {
        println!("* {}", p.display());
    }
    Ok(())
}
