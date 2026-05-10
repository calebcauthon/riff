use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "riff",
    version = crate::RIFF_VERSION,
    long_version = crate::RIFF_LONG_VERSION,
    about = "riff CLI: local dictation + screenshot session tool"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub verbose: bool,

    #[arg(long, global = true)]
    pub quiet: bool,

    #[arg(long, global = true)]
    pub json: bool,

    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Disable start/stop beep sounds for this invocation
    #[arg(long = "no-beeps", global = true)]
    pub no_beeps: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start dictation session
    Start(StartArgs),
    /// Capture screenshot into active session
    Shot,
    /// Stop dictation and transcribe
    Stop(StopArgs),
    /// Toggle dictation session (start if idle, stop if active)
    Toggle(ToggleArgs),
    /// Split session: stop current recording and immediately start a new one
    Fork,
    /// Show running live session status
    Live(LiveArgs),
    /// Transcribe audio captured so far and keep recording
    Chunk,
    /// Pause transcription capture while continuing to record audio
    Pause,
    /// Resume transcription capture after pause
    Unpause,
    /// Toggle transcription pause state (pause if listening, unpause if paused)
    TogglePause,

    /// List recent sessions
    List(ListArgs),
    /// Show note markdown for a session id
    Show(ShowArgs),
    /// Print transcript for a recent session index
    Copy(CopyArgs),
    /// Copy transcript and paste into focused app
    Send(SendArgs),
    /// Open HTML report for a session id
    Html(HtmlArgs),
    /// Set which derived image is used at the transcript screenshot path
    ScreenshotUse(ScreenshotUseArgs),

    /// Pick start/stop sounds and beep timing
    Sounds,
    /// Disable beeps globally (writes RIFF_BEEP=0 to rc file)
    Silence,
    /// Enable beeps globally (writes RIFF_BEEP=1 to rc file)
    Loud,
    /// Show active session status
    Status,
    /// Show startup/shutdown timing summary from perf log
    Perf(PerfArgs),
    #[command(hide = true, name = "watch-clipboard")]
    WatchClipboard(WatchClipboardArgs),
    /// Kill background helper servers (web + parakeet)
    KillServer,
}

#[derive(Args, Debug)]
pub struct StartArgs {
    #[arg(long)]
    pub screenshot_dir: Option<PathBuf>,

    #[arg(long, default_value = "auto")]
    pub audio_device: String,
}

#[derive(Args, Debug)]
pub struct StopArgs {
    #[arg(long)]
    pub no_stop_hooks: bool,

    #[arg(long)]
    pub transcribe_cmd: Option<String>,

    #[arg(long)]
    pub post_transcribe_cmd: Option<String>,

    #[arg(long)]
    pub python_bin: Option<String>,

    #[arg(long)]
    pub parakeet_script: Option<PathBuf>,

    #[arg(long)]
    pub parakeet_model: Option<String>,
}

#[derive(Args, Debug)]
pub struct ToggleArgs {
    /// Used when idle (start path): override screenshot source dir
    #[arg(long)]
    pub screenshot_dir: Option<PathBuf>,

    /// Used when idle (start path): ffmpeg avfoundation selector
    #[arg(long, default_value = "auto")]
    pub audio_device: String,

    /// Used when active (stop path): custom transcription command template
    #[arg(long)]
    pub no_stop_hooks: bool,

    /// Used when active (stop path): custom transcription command template
    #[arg(long)]
    pub transcribe_cmd: Option<String>,

    /// Used when active (stop path): post-process transcript command template
    #[arg(long)]
    pub post_transcribe_cmd: Option<String>,

    /// Used when active (stop path): override python interpreter
    #[arg(long)]
    pub python_bin: Option<String>,

    /// Used when active (stop path): override parakeet script path
    #[arg(long)]
    pub parakeet_script: Option<PathBuf>,

    /// Used when active (stop path): override parakeet model name
    #[arg(long)]
    pub parakeet_model: Option<String>,
}

#[derive(Args, Debug)]
pub struct LiveArgs {
    /// Refresh interval in milliseconds
    #[arg(long, default_value_t = 1000)]
    pub poll_ms: u64,

    /// Print one snapshot and exit
    #[arg(long, default_value_t = false)]
    pub once: bool,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Number of recent sessions to show
    pub n: Option<usize>,
}

#[derive(Args, Debug)]
pub struct CopyArgs {
    /// Which recent session to output (1 = most recent)
    pub n: Option<usize>,
}

#[derive(Args, Debug)]
pub struct SendArgs {
    /// Which recent session to send (1 = most recent)
    pub n: Option<usize>,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// Session id (for example: 20260413-013011)
    pub session_id: String,
}

#[derive(Args, Debug)]
pub struct HtmlArgs {
    /// Session id (for example: 20260413-013011); defaults to most recent when omitted
    pub session_id: Option<String>,
}

#[derive(Args, Debug)]
pub struct PerfArgs {
    /// Number of recent perf records to inspect
    pub n: Option<usize>,
}

#[derive(Args, Debug)]
pub struct ScreenshotUseArgs {
    /// Session id (for example: 20260413-013011)
    #[arg(long)]
    pub session_id: String,

    /// Screenshot id (for example: 1)
    #[arg(long)]
    pub shot_id: usize,

    /// Module id (for example: polaroid, framed, enhanced, original)
    #[arg(long)]
    pub module: String,
}

#[derive(Args, Debug)]
pub struct WatchClipboardArgs {
    #[arg(long)]
    pub session_id: String,

    #[arg(long)]
    pub events_path: PathBuf,

    #[arg(long)]
    pub started_at_epoch: f64,

    #[arg(long, default_value_t = 0)]
    pub start_id: usize,

    #[arg(long, default_value_t = 450)]
    pub poll_ms: u64,
}
