use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
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
    /// Follow the global riff event stream
    Watch(WatchArgs),
    /// Transcribe audio captured so far and keep recording
    Chunk,
    /// Pause transcription capture while continuing to record audio
    Pause,
    /// Resume transcription capture after pause
    Unpause,
    /// Toggle transcription pause state (pause if listening, unpause if paused)
    TogglePause,

    /// Provision riff's private transcription environment
    Setup(SetupArgs),
    /// Check installation, transcription, permissions, and helper health
    Doctor(DoctorArgs),

    /// List recent sessions
    List(ListArgs),
    /// Show note markdown for a session id
    Show(ShowArgs),
    /// Print session transcript, clipboard, and base64 images to stdout
    Copy(CopyArgs),
    /// Copy transcript and paste into focused app
    Send(SendArgs),
    /// Like `send`, but paste actual image data for screenshots instead of file paths
    #[command(name = "send-images")]
    SendImages(SendArgs),
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
    /// Show configured output hooks and transcription commands
    Hooks,
    /// Show startup/shutdown timing summary from perf log
    Perf(PerfArgs),
    #[command(hide = true, name = "watch-clipboard")]
    WatchClipboard(WatchClipboardArgs),
    #[command(hide = true, name = "watch-max-duration")]
    WatchMaxDuration(WatchMaxDurationArgs),
    /// Kill background helper servers (web + parakeet + daemon)
    KillServer,
    /// Stop stray helper servers (including untracked ones) and start fresh
    Restart(RestartArgs),
    /// Manage the riff daemon: control socket for events in/out
    Daemon(DaemonArgs),
    /// Append an event to the global bus
    Emit(EmitArgs),
}

#[derive(Args, Debug)]
pub struct StartArgs {
    #[arg(long)]
    pub screenshot_dir: Option<PathBuf>,

    #[arg(long, default_value = "auto")]
    pub audio_device: String,

    /// Transcription engine: "parakeet" (local, default) or "elevenlabs"
    /// (streaming, needs RIFF_ELEVENLABS_API_KEY). Falls back to RIFF_ENGINE.
    #[arg(long)]
    pub engine: Option<String>,
}

#[derive(Args, Debug)]
pub struct StopArgs {
    #[arg(long)]
    pub no_stop_hooks: bool,

    /// Skip the configured RIFF_HOOKS output-hook chain for this run
    #[arg(long)]
    pub no_hooks: bool,

    /// Add an ad-hoc output hook for this run (script path or shell command).
    /// Repeatable; hooks run in order, after the configured RIFF_HOOKS chain.
    /// A bare path is forwarded the transcript ("$1") and metadata ("$2") files.
    #[arg(long = "with-post-hook", value_name = "CMD")]
    pub with_post_hook: Vec<String>,

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

    /// Used when idle (start path): transcription engine ("parakeet" or "elevenlabs")
    #[arg(long)]
    pub engine: Option<String>,

    /// Used when active (stop path): custom transcription command template
    #[arg(long)]
    pub no_stop_hooks: bool,

    /// Used when active (stop path): skip the RIFF_HOOKS output-hook chain
    #[arg(long)]
    pub no_hooks: bool,

    /// Used when active (stop path): add an ad-hoc output hook (repeatable)
    #[arg(long = "with-post-hook", value_name = "CMD")]
    pub with_post_hook: Vec<String>,

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
pub struct RestartArgs {
    /// Restart only the Parakeet transcription server
    #[arg(long)]
    pub parakeet: bool,

    /// Restart only the local report web server
    #[arg(long)]
    pub web: bool,

    /// Return as soon as the replacement is spawned instead of waiting for ready
    #[arg(long = "no-wait")]
    pub no_wait: bool,
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// Backfill events newer than a duration (for example: 30s, 10m, 2h, 1d)
    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,

    /// Backfill the last N matching events before following
    #[arg(short = 'n', long = "tail", value_name = "COUNT")]
    pub tail: Option<usize>,

    /// Backfill the entire retained event history before following
    #[arg(long)]
    pub all: bool,

    /// Print the backfill and exit instead of following
    #[arg(long)]
    pub once: bool,

    /// Only show these event types (repeatable)
    #[arg(long = "type", value_name = "TYPE")]
    pub event_type: Vec<String>,

    /// Only show events from these commands (repeatable)
    #[arg(long = "command", value_name = "COMMAND")]
    pub command_filter: Vec<String>,

    /// Only show events for a session id, or "current" for the active session
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Only show events whose JSON contains this substring
    #[arg(long, value_name = "TEXT")]
    pub grep: Option<String>,

    /// Poll interval in milliseconds while following
    #[arg(long, default_value_t = 200)]
    pub poll_ms: u64,
}

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Python 3.12 interpreter used to create the private runtime
    #[arg(long)]
    pub python: Option<String>,

    /// Private runtime directory; defaults to ~/Library/Application Support/riff/runtime/python
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,

    /// Skip Python package installation
    #[arg(long)]
    pub skip_packages: bool,

    /// Skip model pre-download
    #[arg(long)]
    pub skip_model: bool,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Attempt slower checks such as importing Python packages
    #[arg(long)]
    pub deep: bool,
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
pub struct WatchMaxDurationArgs {
    /// Session this watchdog belongs to; it refuses to stop anything else
    #[arg(long)]
    pub session_id: String,

    /// Wall-clock seconds after session start before the auto-stop fires
    #[arg(long)]
    pub max_sec: f64,

    /// Recorder pid; the watchdog exits early once it dies
    #[arg(long)]
    pub ffmpeg_pid: i32,

    #[arg(long)]
    pub started_at_epoch: f64,

    #[arg(long, default_value_t = 1000)]
    pub poll_ms: u64,

    /// Recording target; watched to confirm the mic is actually capturing
    #[arg(long)]
    pub audio_path: Option<PathBuf>,

    /// Session events file the `mic_listening` confirmation is written to
    #[arg(long)]
    pub events_path: Option<PathBuf>,
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

#[derive(Args, Debug)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub action: DaemonAction,
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
    /// Run the daemon in the foreground (`daemon start` spawns this detached)
    Run(DaemonRunArgs),
    /// Start the daemon in the background
    Start(DaemonStartArgs),
    /// Stop the running daemon
    Stop,
    /// Show daemon identity and health
    Status,
}

#[derive(Args, Debug)]
pub struct DaemonRunArgs {
    /// Owning RIFF_ROOT, present on the command line so a wedged daemon can
    /// still be identified from the process table. Must match the environment.
    #[arg(long)]
    pub root: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct DaemonStartArgs {
    /// Return as soon as the daemon is spawned instead of waiting for ready
    #[arg(long = "no-wait")]
    pub no_wait: bool,
}

#[derive(Args, Debug)]
pub struct EmitArgs {
    /// Event type, snake_case (for example: build_finished)
    pub event_type: String,

    /// JSON object payload for the event
    #[arg(long, value_name = "JSON")]
    pub data: Option<String>,

    /// Attach to a session id, or "current" for the active session
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Event level: info, warn, or error
    #[arg(long, default_value = "info")]
    pub level: String,
}

impl Commands {
    /// Stable command name used as the `command` field on every bus event.
    pub fn event_name(&self) -> &'static str {
        match self {
            Commands::Start(_) => "start",
            Commands::Shot => "shot",
            Commands::Stop(_) => "stop",
            Commands::Toggle(_) => "toggle",
            Commands::Fork => "fork",
            Commands::Live(_) => "live",
            Commands::Watch(_) => "watch",
            Commands::Chunk => "chunk",
            Commands::Pause => "pause",
            Commands::Unpause => "unpause",
            Commands::TogglePause => "toggle-pause",
            Commands::Setup(_) => "setup",
            Commands::Doctor(_) => "doctor",
            Commands::List(_) => "list",
            Commands::Show(_) => "show",
            Commands::Copy(_) => "copy",
            Commands::Send(_) => "send",
            Commands::SendImages(_) => "send-images",
            Commands::Html(_) => "html",
            Commands::ScreenshotUse(_) => "screenshot-use",
            Commands::Sounds => "sounds",
            Commands::Silence => "silence",
            Commands::Loud => "loud",
            Commands::Status => "status",
            Commands::Hooks => "hooks",
            Commands::Perf(_) => "perf",
            Commands::WatchClipboard(_) => "watch-clipboard",
            Commands::WatchMaxDuration(_) => "watch-max-duration",
            Commands::KillServer => "kill-server",
            Commands::Restart(_) => "restart",
            Commands::Daemon(_) => "daemon",
            Commands::Emit(_) => "emit",
        }
    }

    /// Redacted argument summary for `command_started`. User-supplied command
    /// templates and hook bodies are reported as counts and booleans only —
    /// they can carry arbitrary shell and do not belong in a shared log.
    pub fn event_args(&self) -> Value {
        match self {
            Commands::Start(a) => json!({
                "audio_device": a.audio_device,
                "screenshot_dir_override": a.screenshot_dir.is_some(),
                "engine": a.engine,
            }),
            Commands::Stop(a) => json!({
                "no_hooks": a.no_hooks,
                "no_stop_hooks": a.no_stop_hooks,
                "post_hooks": a.with_post_hook.len(),
                "custom_transcribe_cmd": a.transcribe_cmd.is_some(),
                "custom_post_transcribe_cmd": a.post_transcribe_cmd.is_some(),
                "parakeet_model_override": a.parakeet_model.is_some(),
            }),
            Commands::Toggle(a) => json!({
                "audio_device": a.audio_device,
                "screenshot_dir_override": a.screenshot_dir.is_some(),
                "no_hooks": a.no_hooks,
                "no_stop_hooks": a.no_stop_hooks,
                "post_hooks": a.with_post_hook.len(),
                "custom_transcribe_cmd": a.transcribe_cmd.is_some(),
                "custom_post_transcribe_cmd": a.post_transcribe_cmd.is_some(),
                "parakeet_model_override": a.parakeet_model.is_some(),
            }),
            Commands::Live(a) => json!({ "once": a.once, "poll_ms": a.poll_ms }),
            Commands::Watch(a) => json!({
                "once": a.once,
                "all": a.all,
                "since": a.since,
                "tail": a.tail,
                "types": a.event_type,
                "commands": a.command_filter,
                "session": a.session,
                "grep": a.grep.is_some(),
            }),
            Commands::Setup(a) => json!({
                "skip_packages": a.skip_packages,
                "skip_model": a.skip_model,
                "python_override": a.python.is_some(),
                "runtime_dir_override": a.runtime_dir.is_some(),
            }),
            Commands::Doctor(a) => json!({ "deep": a.deep }),
            Commands::List(a) => json!({ "n": a.n }),
            Commands::Copy(a) => json!({ "n": a.n }),
            Commands::Send(a) | Commands::SendImages(a) => json!({ "n": a.n }),
            Commands::Show(a) => json!({ "session_id": a.session_id }),
            Commands::Html(a) => json!({ "session_id": a.session_id }),
            Commands::Perf(a) => json!({ "n": a.n }),
            Commands::ScreenshotUse(a) => json!({
                "session_id": a.session_id,
                "shot_id": a.shot_id,
                "module": a.module,
            }),
            Commands::Restart(a) => json!({
                "parakeet": a.parakeet,
                "web": a.web,
                "no_wait": a.no_wait,
            }),
            Commands::Daemon(a) => json!({
                "action": match &a.action {
                    DaemonAction::Run(_) => "run",
                    DaemonAction::Start(_) => "start",
                    DaemonAction::Stop => "stop",
                    DaemonAction::Status => "status",
                },
            }),
            // The event type is a validated identifier, not user prose; the
            // payload itself stays out of the redacted argument summary.
            Commands::Emit(a) => json!({
                "type": a.event_type,
                "has_data": a.data.is_some(),
                "session": a.session,
                "level": a.level,
            }),
            Commands::WatchClipboard(a) => json!({ "session_id": a.session_id }),
            Commands::WatchMaxDuration(a) => json!({
                "session_id": a.session_id,
                "max_sec": a.max_sec,
                "ffmpeg_pid": a.ffmpeg_pid,
            }),
            Commands::Shot
            | Commands::Fork
            | Commands::Chunk
            | Commands::Pause
            | Commands::Unpause
            | Commands::TogglePause
            | Commands::Sounds
            | Commands::Silence
            | Commands::Loud
            | Commands::Status
            | Commands::Hooks
            | Commands::KillServer => json!({}),
        }
    }
}
