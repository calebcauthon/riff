use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "dictate",
    about = "ispy CLI: local dictation + screenshot session tool"
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

    /// List recent sessions
    List(ListArgs),
    /// Show note markdown for a session id
    Show(ShowArgs),
    /// Print transcript for a recent session index
    Copy(CopyArgs),
    /// Open HTML report for a session id
    Html(HtmlArgs),

    /// Pick start/stop sounds and beep timing
    Sounds,
    /// Show active session status
    Status,
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
    pub transcribe_cmd: Option<String>,

    #[arg(long)]
    pub python_bin: Option<String>,

    #[arg(long)]
    pub parakeet_script: Option<PathBuf>,

    #[arg(long)]
    pub parakeet_model: Option<String>,
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
