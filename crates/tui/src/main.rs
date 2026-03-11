mod app;
mod keymap;
mod ui;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as OsCommand, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use app::TuiApp;
use base64::Engine as _;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use anvl_core::{spawn_core, CoreHandle};
use protocol::{AttentionLevel, Command, Event as CoreEvent, Route, TerminalKind};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

#[derive(Debug)]
enum LaunchMode {
    Local,
    CreateSession { name: String },
    AttachSession { name: String },
    RemoveSession { name: String },
    ListSessions,
    RunDaemon { name: String },
    Update,
}

#[derive(Debug)]
struct Cli {
    mode: LaunchMode,
    detach: bool,
    version: bool,
    help: bool,
}

struct Backend {
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<CoreEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionEntry {
    name: String,
    socket_path: String,
    pid: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionRegistry {
    sessions: Vec<SessionEntry>,
}

fn print_help() {
    println!(
        "\
anvl {}

USAGE:
    anvl [OPTIONS]

OPTIONS:
    -s, --session <name>   Create (or reattach to) a named session
    -a <name>              Attach to an existing session
    -r, --remove <name>    Remove a session (stops its daemon)
    -l, --list             List active sessions
    -d, --detach           Start session in background only (with -s or -a)
    -u, --update           Update to the latest release from GitHub
    -V, --version          Print version
    -h, --help             Print this help

EXAMPLES:
    anvl                   Launch in local (non-session) mode
    anvl -s work           Create or reattach to session 'work'
    anvl -s work -d        Start session 'work' in background
    anvl -a work           Attach to running session 'work'
    anvl -l                List sessions
    anvl -r work           Remove session 'work'",
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_cli(args: Vec<String>) -> Result<Cli> {
    let mut i = 0usize;
    let mut mode = LaunchMode::Local;
    let mut detach = false;
    let mut version = false;
    let mut help = false;
    let mut daemon_name: Option<String> = None;

    while i < args.len() {
        match args[i].as_str() {
            "-s" | "--session" => {
                let Some(name) = args.get(i + 1).cloned() else {
                    return Err(anyhow!("missing session name for {}", args[i]));
                };
                mode = LaunchMode::CreateSession { name };
                i += 2;
            }
            "-a" => {
                let Some(name) = args.get(i + 1).cloned() else {
                    return Err(anyhow!("missing session name for -a"));
                };
                mode = LaunchMode::AttachSession { name };
                i += 2;
            }
            "-r" | "--remove" => {
                let Some(name) = args.get(i + 1).cloned() else {
                    return Err(anyhow!("missing session name for {}", args[i]));
                };
                mode = LaunchMode::RemoveSession { name };
                i += 2;
            }
            "-V" | "--version" => {
                version = true;
                i += 1;
            }
            "-d" | "--detach" => {
                detach = true;
                i += 1;
            }
            "-l" | "--list" => {
                mode = LaunchMode::ListSessions;
                i += 1;
            }
            "-u" | "--update" => {
                mode = LaunchMode::Update;
                i += 1;
            }
            "-h" | "--help" => {
                help = true;
                i += 1;
            }
            "--run-daemon" => {
                mode = LaunchMode::RunDaemon {
                    name: String::new(),
                };
                i += 1;
            }
            "--session-name" => {
                let Some(name) = args.get(i + 1).cloned() else {
                    return Err(anyhow!("missing name for --session-name"));
                };
                daemon_name = Some(name);
                i += 2;
            }
            other => {
                return Err(anyhow!("unknown argument: {other}"));
            }
        }
    }

    if matches!(mode, LaunchMode::RunDaemon { .. }) {
        let name = daemon_name.unwrap_or_default();
        return Ok(Cli {
            mode: LaunchMode::RunDaemon { name },
            detach,
            version,
            help,
        });
    }

    if detach
        && matches!(
            mode,
            LaunchMode::RemoveSession { .. } | LaunchMode::ListSessions
        )
    {
        return Err(anyhow!(
            "--detach is only valid with session create/attach (-s or -a)"
        ));
    }

    Ok(Cli {
        mode,
        detach,
        version,
        help,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_cli(std::env::args().skip(1).collect::<Vec<_>>())?;
    if cli.help {
        print_help();
        return Ok(());
    }
    if cli.version {
        println!("anvl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    match cli.mode {
        LaunchMode::Update => self_update(),
        LaunchMode::RunDaemon { name } => run_daemon(&name).await,
        LaunchMode::RemoveSession { name } => delete_session(&name),
        LaunchMode::ListSessions => list_sessions(),
        LaunchMode::CreateSession { name } => {
            let entry = ensure_session_running(&name).await?;
            if cli.detach {
                println!(
                    "session '{}' running in background (detached)",
                    entry.name
                );
                return Ok(());
            }
            let backend = build_remote_backend(&entry.socket_path).await?;
            run_tui(backend).await
        }
        LaunchMode::AttachSession { name } => {
            let entry = get_session(&name)?.ok_or_else(|| {
                anyhow!(
                    "session '{}' not found. create it with: anvl -s {}",
                    name,
                    name
                )
            })?;
            let entry = if !socket_alive(&entry.socket_path) {
                eprintln!("session '{}' is stale, restarting…", name);
                ensure_session_running(&name).await?
            } else {
                entry
            };
            if cli.detach {
                println!(
                    "session '{}' is running (detached)",
                    entry.name
                );
                return Ok(());
            }
            let backend = build_remote_backend(&entry.socket_path).await?;
            run_tui(backend).await
        }
        LaunchMode::Local => {
            if cli.detach {
                return Err(anyhow!(
                    "--detach requires a named session: use `anvl -s <name> -d` or `anvl -a <name> -d`"
                ));
            }
            let (backend, _core) = build_local_backend();
            run_tui(backend).await
        }
    }
}

fn self_update() -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    // Fetch latest release info from GitHub
    let api_output = OsCommand::new("curl")
        .args(["-fsSL", "https://api.github.com/repos/inhesrom/anvl/releases/latest"])
        .output()
        .context("failed to run curl — is it installed?")?;
    if !api_output.status.success() {
        return Err(anyhow!(
            "failed to fetch latest release info from GitHub (curl exit {})",
            api_output.status
        ));
    }
    let api_body = String::from_utf8_lossy(&api_output.stdout);

    // Parse tag_name from JSON (avoid adding a serde_json dep for this one field)
    let tag = api_body
        .lines()
        .find(|l| l.contains("\"tag_name\""))
        .and_then(|l| {
            let start = l.find('"')? + 1; // skip to first quote
            let rest = &l[start..];
            let start2 = rest.find('"')? + 1;
            let rest2 = &rest[start2..];
            let start3 = rest2.find('"')? + 1;
            let rest3 = &rest2[start3..];
            let end = rest3.find('"')?;
            Some(rest3[..end].to_string())
        })
        .ok_or_else(|| anyhow!("could not parse tag_name from GitHub API response"))?;

    let latest_version = tag.strip_prefix('v').unwrap_or(&tag);

    if latest_version == current_version {
        println!("anvl is already up to date (v{current_version})");
        return Ok(());
    }

    println!("updating anvl v{current_version} -> v{latest_version}...");

    // Detect platform
    let os_output = OsCommand::new("uname").arg("-s").output()?;
    let os_name = String::from_utf8_lossy(&os_output.stdout)
        .trim()
        .to_lowercase();

    let arch_output = OsCommand::new("uname").arg("-m").output()?;
    let arch_name = String::from_utf8_lossy(&arch_output.stdout)
        .trim()
        .to_string();

    let target = match (os_name.as_str(), arch_name.as_str()) {
        ("darwin", "arm64" | "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => return Err(anyhow!("unsupported platform: {os_name} {arch_name}")),
    };

    let url = format!(
        "https://github.com/inhesrom/anvl/releases/download/{tag}/anvl-{target}.tar.gz"
    );

    // Download to a temp directory
    let tmp_dir = std::env::temp_dir().join(format!("anvl-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir)?;
    let _cleanup = TempDirGuard(tmp_dir.clone());

    let tarball = tmp_dir.join("anvl.tar.gz");
    let dl_status = OsCommand::new("curl")
        .args(["-fsSL", &url, "-o"])
        .arg(&tarball)
        .status()
        .context("failed to run curl for download")?;
    if !dl_status.success() {
        return Err(anyhow!("failed to download release tarball from {url}"));
    }

    // Extract
    let extract_status = OsCommand::new("tar")
        .arg("xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&tmp_dir)
        .status()
        .context("failed to run tar")?;
    if !extract_status.success() {
        return Err(anyhow!("failed to extract release tarball"));
    }

    // Replace the current binary
    let current_exe = std::env::current_exe().context("cannot determine current executable path")?;
    let new_binary = tmp_dir.join("anvl");
    if !new_binary.exists() {
        return Err(anyhow!("extracted archive does not contain 'anvl' binary"));
    }

    // Remove the running binary first — Linux allows unlinking an in-use
    // executable but blocks writing to it (ETXTBSY / "Text file busy").
    std::fs::remove_file(&current_exe).with_context(|| {
        format!(
            "failed to remove old binary at {}. You may need to run with sudo.",
            current_exe.display()
        )
    })?;
    std::fs::copy(&new_binary, &current_exe).with_context(|| {
        format!(
            "failed to install new binary at {}.",
            current_exe.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&current_exe, std::fs::Permissions::from_mode(0o755))?;
    }

    println!("anvl updated to v{latest_version}");
    Ok(())
}

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn build_local_backend() -> (Backend, CoreHandle) {
    let core = spawn_core();
    let cmd_tx = core.cmd_tx.clone();

    let (evt_tx, evt_rx) = mpsc::channel::<CoreEvent>(1024);
    let mut broadcast_rx = core.evt_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match broadcast_rx.recv().await {
                Ok(evt) => {
                    if evt_tx.send(evt).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
                Err(RecvError::Lagged(_)) => continue,
            }
        }
    });

    (Backend { cmd_tx, evt_rx }, core)
}

// ---------------------------------------------------------------------------
// Unix-domain-socket session infrastructure
// ---------------------------------------------------------------------------

fn session_socket_dir() -> Result<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        return Err(anyhow!("cannot determine config directory"));
    };
    Ok(base.join("anvl").join("sessions"))
}

fn session_socket_path(name: &str) -> Result<PathBuf> {
    let safe = sanitize_session_name(name);
    Ok(session_socket_dir()?.join(format!("{safe}.sock")))
}

/// 4-byte big-endian length prefix + JSON payload.
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(anyhow!("frame too large: {} bytes", len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    let len = (data.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(data).await?;
    w.flush().await?;
    Ok(())
}

/// Rolling buffer that stores recent terminal events for replay on client reconnect.
/// Caps total size at ~2 MB to bound memory usage.
struct EventHistory {
    /// Serialized event frames (each is a JSON payload).
    frames: Vec<Vec<u8>>,
    total_bytes: usize,
}

const EVENT_HISTORY_MAX_BYTES: usize = 2 * 1024 * 1024; // 2 MB

impl EventHistory {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            total_bytes: 0,
        }
    }

    fn push(&mut self, payload: Vec<u8>) {
        self.total_bytes += payload.len();
        self.frames.push(payload);
        // Evict oldest frames when over budget
        while self.total_bytes > EVENT_HISTORY_MAX_BYTES && !self.frames.is_empty() {
            let removed = self.frames.remove(0);
            self.total_bytes -= removed.len();
        }
    }

    fn snapshot(&self) -> Vec<Vec<u8>> {
        self.frames.clone()
    }
}

/// Returns true if the event is worth replaying to a reconnecting client.
fn is_replayable(evt: &CoreEvent) -> bool {
    matches!(
        evt,
        CoreEvent::TerminalOutput { .. }
            | CoreEvent::TerminalStarted { .. }
            | CoreEvent::TerminalExited { .. }
            | CoreEvent::WorkspaceList { .. }
            | CoreEvent::WorkspaceGitUpdated { .. }
            | CoreEvent::WorkspaceAttentionChanged { .. }
    )
}

async fn run_daemon(name: &str) -> Result<()> {
    let sock_path = session_socket_path(name)?;
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove stale socket file if it exists
    let _ = std::fs::remove_file(&sock_path);

    let core = spawn_core();
    let listener = tokio::net::UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind unix socket: {}", sock_path.display()))?;

    // Clean up socket on exit
    struct CleanupGuard(PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = CleanupGuard(sock_path.clone());

    // Shared history buffer for replaying events to reconnecting clients
    let history = std::sync::Arc::new(tokio::sync::Mutex::new(EventHistory::new()));

    // Background task: record replayable events into history
    {
        let history = history.clone();
        let mut evt_rx = core.evt_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match evt_rx.recv().await {
                    Ok(evt) => {
                        if is_replayable(&evt) {
                            if let Ok(payload) = serde_json::to_vec(&evt) {
                                history.lock().await.push(payload);
                            }
                        }
                    }
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let (mut reader, mut writer) = stream.into_split();
        let cmd_tx = core.cmd_tx.clone();
        let mut evt_rx = core.evt_tx.subscribe();
        let history = history.clone();

        // Bridge: read Commands from socket, send Events back
        tokio::spawn(async move {
            let (local_evt_tx, mut local_evt_rx) = mpsc::channel::<CoreEvent>(1024);

            // Forward broadcast events to local channel
            tokio::spawn(async move {
                loop {
                    match evt_rx.recv().await {
                        Ok(evt) => {
                            if local_evt_tx.send(evt).await.is_err() {
                                break;
                            }
                        }
                        Err(RecvError::Closed) => break,
                        Err(RecvError::Lagged(_)) => continue,
                    }
                }
            });

            // Write events to socket
            let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(1024);
            tokio::spawn(async move {
                while let Some(data) = write_rx.recv().await {
                    if write_frame(&mut writer, &data).await.is_err() {
                        break;
                    }
                }
            });

            // Replay historical events to the newly connected client
            {
                let snapshot = history.lock().await.snapshot();
                for frame in snapshot {
                    if write_tx.send(frame).await.is_err() {
                        return;
                    }
                }
            }

            // Forward events to write channel
            let write_tx2 = write_tx.clone();
            tokio::spawn(async move {
                while let Some(evt) = local_evt_rx.recv().await {
                    if let Ok(payload) = serde_json::to_vec(&evt) {
                        if write_tx2.send(payload).await.is_err() {
                            break;
                        }
                    }
                }
            });

            // Read commands from socket
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(data)) => {
                        if let Ok(cmd) = serde_json::from_slice::<Command>(&data) {
                            if cmd_tx.send(cmd).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });
    }
}

async fn build_remote_backend(socket_path: &str) -> Result<Backend> {
    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to daemon socket: {socket_path}"))?;
    let (mut reader, mut writer) = stream.into_split();

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(1024);
    let (evt_tx, evt_rx) = mpsc::channel::<CoreEvent>(1024);

    // Write commands to socket
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            if let Ok(payload) = serde_json::to_vec(&cmd) {
                if write_frame(&mut writer, &payload).await.is_err() {
                    break;
                }
            }
        }
    });

    // Read events from socket
    tokio::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(data)) => {
                    if let Ok(evt) = serde_json::from_slice::<CoreEvent>(&data) {
                        if evt_tx.send(evt).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });

    Ok(Backend { cmd_tx, evt_rx })
}

// ---------------------------------------------------------------------------
// Session management
// ---------------------------------------------------------------------------

async fn ensure_session_running(name: &str) -> Result<SessionEntry> {
    let mut registry = load_registry()?;
    if let Some(existing) = registry.sessions.iter().find(|s| s.name == name).cloned() {
        if socket_alive(&existing.socket_path) {
            return Ok(existing);
        }
        registry.sessions.retain(|s| s.name != name);
    }

    let pid = spawn_daemon_process(name)?;
    let sock_path = session_socket_path(name)?;
    let sock_str = sock_path.display().to_string();

    wait_for_socket(&sock_str, Duration::from_secs(8)).await?;

    let entry = SessionEntry {
        name: name.to_string(),
        socket_path: sock_str,
        pid,
    };
    registry.sessions.retain(|s| s.name != name);
    registry.sessions.push(entry.clone());
    save_registry(&registry)?;
    Ok(entry)
}

fn get_session(name: &str) -> Result<Option<SessionEntry>> {
    let registry = load_registry()?;
    Ok(registry.sessions.into_iter().find(|s| s.name == name))
}

fn delete_session(name: &str) -> Result<()> {
    let mut registry = load_registry()?;
    let Some(entry) = registry.sessions.iter().find(|s| s.name == name).cloned() else {
        println!("session '{}' not found", name);
        return Ok(());
    };

    print!(
        "Delete session '{}'? This will stop running terminals. [y/N]: ",
        entry.name
    );
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let confirm = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
    if !confirm {
        println!("aborted");
        return Ok(());
    }

    if is_expected_daemon_process(&entry) {
        let _ = OsCommand::new("kill").arg(entry.pid.to_string()).status();
    } else {
        println!(
            "warning: pid {} does not look like session daemon '{}'; skipping kill and removing registry entry only",
            entry.pid, entry.name
        );
    }

    // Clean up socket file
    let _ = std::fs::remove_file(&entry.socket_path);

    registry.sessions.retain(|s| s.name != name);
    save_registry(&registry)?;
    if let Some(path) = session_workspaces_persist_path(name) {
        let _ = std::fs::remove_file(path);
    }
    println!("deleted session '{}'", name);
    Ok(())
}

fn list_sessions() -> Result<()> {
    let registry = load_registry()?;
    if registry.sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    println!("sessions:");
    for s in registry.sessions {
        let state = if socket_alive(&s.socket_path) {
            "running"
        } else {
            "stale"
        };
        println!("- {}  (pid {} {})", s.name, s.pid, state);
    }
    Ok(())
}

fn spawn_daemon_process(name: &str) -> Result<u32> {
    let exe = std::env::current_exe()?;
    let child = OsCommand::new(exe)
        .env("ANVL_SESSION_NAME", name)
        .arg("--run-daemon")
        .arg("--session-name")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn daemon for session '{}'", name))?;
    Ok(child.id())
}

async fn wait_for_socket(path: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if socket_alive(path) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    Err(anyhow!("daemon did not become ready at {}", path))
}

fn socket_alive(path: &str) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

fn is_expected_daemon_process(entry: &SessionEntry) -> bool {
    let output = match OsCommand::new("ps")
        .arg("-p")
        .arg(entry.pid.to_string())
        .arg("-o")
        .arg("command=")
        .output()
    {
        Ok(out) => out,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let cmdline = String::from_utf8_lossy(&output.stdout);
    cmdline.contains("--run-daemon")
        && cmdline.contains(&format!("--session-name {}", entry.name))
}

fn session_registry_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        return None;
    };
    Some(base.join("anvl").join("sessions.json"))
}

fn session_workspaces_persist_path(name: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let safe = sanitize_session_name(name);
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("anvl")
            .join(format!("workspaces.{safe}.json")),
    )
}

fn sanitize_session_name(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "default".to_string();
    }
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

fn load_registry() -> Result<SessionRegistry> {
    let Some(path) = session_registry_path() else {
        return Ok(SessionRegistry::default());
    };
    if !path.exists() {
        return Ok(SessionRegistry::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read session registry: {}", path.display()))?;
    let registry = serde_json::from_str::<SessionRegistry>(&raw).unwrap_or_default();
    Ok(registry)
}

fn save_registry(registry: &SessionRegistry) -> Result<()> {
    let Some(path) = session_registry_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(registry)?;
    std::fs::write(&path, raw)
        .with_context(|| format!("failed to write session registry: {}", path.display()))?;
    Ok(())
}

async fn run_tui(mut backend: Backend) -> Result<()> {
    // Install a panic hook that restores the terminal before printing the panic.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = stdout.execute(DisableBracketedPaste);
        let _ = stdout.execute(DisableMouseCapture);
        let _ = stdout.execute(crossterm::cursor::Show);
        let _ = stdout.execute(LeaveAlternateScreen);
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;
    stdout.execute(EnableBracketedPaste)?;

    let backend_term = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend_term)?;
    let mut app = TuiApp::default();
    let mut last_flash_toggle = Instant::now();

    'main: loop {
        for _ in 0..128 {
            match backend.evt_rx.try_recv() {
                Ok(evt) => apply_event(&mut app, evt),
                Err(_) => break,
            }
        }

        // Check if deferred git result can now be shown (spinner min duration met).
        if let Some((id, msg)) = app.deferred_git_result.take() {
            if app.finish_git_op(id) {
                app.git_action_message = Some((msg, std::time::Instant::now()));
            } else {
                app.deferred_git_result = Some((id, msg));
            }
        }

        if let Route::Workspace { id } = app.route {
            if let Ok(size) = terminal.size() {
                let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                let inner = ui::screens::workspace::terminal_content_rect(area, app.focus, app.terminal_fullscreen);
                let cols = inner.width.max(1);
                let rows = inner.height.max(1);
                let tid = app.active_tab_id();
                let kind = app.active_tab_kind();
                if app.has_terminal_tab(id, &tid) && app.should_send_resize(id, &tid, cols, rows) {
                    app.resize_terminal_parser(id, &tid, cols, rows);
                    let _ = backend
                        .cmd_tx
                        .send(Command::ResizeTerminal {
                            id,
                            kind,
                            tab_id: Some(tid),
                            cols,
                            rows,
                        })
                        .await;
                }
            }
        }

        let mut pending_clipboard_text: Option<String> = None;
        terminal.draw(|frame| {
            match app.route {
                Route::Home => ui::screens::home::render(frame, frame.area(), &app),
                Route::Workspace { .. } => {
                    ui::screens::workspace::render(frame, frame.area(), &app)
                }
            }
            // Extract selected text from the rendered buffer before applying highlights.
            if let Some(sel) = &app.pending_copy_selection {
                pending_clipboard_text =
                    Some(extract_selected_text_from_buf(frame.buffer_mut(), sel));
            }
            if let Some(sel) = &app.mouse_selection {
                if !sel.is_empty() {
                    apply_selection_highlight(frame, sel);
                }
            }
        })?;
        if let Some(text) = pending_clipboard_text {
            app.pending_copy_selection = None;
            if !text.is_empty() {
                let copied = if cfg!(target_os = "linux") {
                    // On Wayland, arboard's clipboard doesn't persist after drop.
                    // Use wl-copy which forks a background process to serve paste requests.
                    std::process::Command::new("wl-copy")
                        .stdin(std::process::Stdio::piped())
                        .spawn()
                        .and_then(|mut child| {
                            use std::io::Write;
                            if let Some(stdin) = child.stdin.as_mut() {
                                stdin.write_all(text.as_bytes())?;
                            }
                            child.wait()
                        })
                        .is_ok()
                } else {
                    arboard::Clipboard::new()
                        .and_then(|mut clipboard| clipboard.set_text(text))
                        .is_ok()
                };
                if copied {
                    app.git_action_message =
                        Some(("Copied to clipboard".to_string(), Instant::now()));
                }
            }
        }

        let mut poll_timeout = Duration::from_millis(16);
        while event::poll(poll_timeout)? {
            poll_timeout = Duration::ZERO;
            match event::read()? {
                Event::Key(key) => {
                    if matches!(key.kind, KeyEventKind::Release) {
                        continue;
                    }

                    if keymap::is_quit(key)
                        && !app.is_adding_workspace()
                        && !app.is_adding_ssh_workspace()
                        && app.ssh_history_picker.is_none()
                        && !app.is_confirming_delete()
                        && !app.is_renaming_workspace()
                        && !app.is_renaming_tab()
                        && !app.is_committing()
                        && !app.is_creating_branch()
                        && !app.is_confirming_discard()
                        && !app.is_confirming_stash_pull_pop()
                        && !app.is_stashing()
                        && !app.is_settings_open()
                        && !matches!(app.focus, app::Focus::WsTerminal)
                    {
                        break 'main;
                    }

                    match app.route {
                        Route::Home => {
                            if app.is_settings_open() {
                                match key.code {
                                    KeyCode::Esc | KeyCode::Char('S') => app.close_settings(),
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        app.settings_selected = (app.settings_selected + 1)
                                            .min(app.settings_count() - 1);
                                    }
                                    KeyCode::Up | KeyCode::Char('k') => {
                                        app.settings_selected =
                                            app.settings_selected.saturating_sub(1);
                                    }
                                    KeyCode::Enter | KeyCode::Char(' ') => {
                                        app.toggle_selected_setting()
                                    }
                                    _ => {}
                                }
                            } else if app.is_confirming_delete() {
                                match key.code {
                                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                                        if let Some(id) = app.take_delete_workspace() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RemoveWorkspace { id })
                                                .await;
                                        }
                                    }
                                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                        app.cancel_delete_workspace()
                                    }
                                    _ => {}
                                }
                            } else if app.ssh_history_picker.is_some() {
                                match key.code {
                                    KeyCode::Char('j') | KeyCode::Down => {
                                        if let Some(ref mut picker) = app.ssh_history_picker {
                                            let len = app.ssh_history.len();
                                            if len > 0 {
                                                picker.selected = (picker.selected + 1) % len;
                                            }
                                        }
                                    }
                                    KeyCode::Char('k') | KeyCode::Up => {
                                        if let Some(ref mut picker) = app.ssh_history_picker {
                                            let len = app.ssh_history.len();
                                            if len > 0 {
                                                picker.selected = (picker.selected + len - 1) % len;
                                            }
                                        }
                                    }
                                    KeyCode::Enter => app.select_ssh_history_entry(),
                                    KeyCode::Char('n') => app.begin_new_ssh_from_picker(),
                                    KeyCode::Esc => app.cancel_ssh_history_picker(),
                                    _ => {}
                                }
                                continue;
                            } else if app.is_adding_ssh_workspace() {
                                match key.code {
                                    KeyCode::Esc => app.cancel_ssh_workspace(),
                                    KeyCode::Tab | KeyCode::BackTab => {
                                        if let Some(ref mut input) = app.ssh_workspace_input {
                                            input.cycle_field();
                                        }
                                    }
                                    KeyCode::Enter => {
                                        // Record history before taking the request
                                        if let Some(ref input) = app.ssh_workspace_input {
                                            let host = input.host.trim().to_string();
                                            let path = input.path.trim().to_string();
                                            if !host.is_empty() && !path.is_empty() {
                                                let user = if input.user.trim().is_empty() {
                                                    None
                                                } else {
                                                    Some(input.user.trim().to_string())
                                                };
                                                app.record_ssh_history(app::SshHistoryEntry {
                                                    host,
                                                    user,
                                                    path,
                                                });
                                            }
                                        }
                                        if let Some((name, path, target)) =
                                            app.take_ssh_workspace_request()
                                        {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::AddWorkspace {
                                                    name,
                                                    path,
                                                    ssh: Some(target),
                                                })
                                                .await;
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(ref mut input) = app.ssh_workspace_input {
                                            input.active_input_mut().push(c);
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(ref mut input) = app.ssh_workspace_input {
                                            input.active_input_mut().pop();
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            } else if app.is_adding_workspace() {
                                let editing = app.dir_browser.as_ref().map_or(false, |b| b.editing_path);
                                if editing {
                                    match key.code {
                                        KeyCode::Esc => app.cancel_add_workspace(),
                                        KeyCode::Enter => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.confirm_path_edit();
                                            }
                                        }
                                        KeyCode::Backspace => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.path_input.pop();
                                            }
                                        }
                                        KeyCode::Tab => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                apply_path_autocomplete(&mut browser.path_input);
                                                browser.confirm_path_edit();
                                            }
                                        }
                                        KeyCode::Char(c) => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.path_input.push(c);
                                            }
                                        }
                                        _ => {}
                                    }
                                } else {
                                    match key.code {
                                        KeyCode::Esc => app.cancel_add_workspace(),
                                        KeyCode::Char('j') | KeyCode::Down => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.move_selection(1);
                                            }
                                        }
                                        KeyCode::Char('k') | KeyCode::Up => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.move_selection(-1);
                                            }
                                        }
                                        KeyCode::Enter => {
                                            if let Some((name, path)) = app.take_add_workspace_request()
                                            {
                                                let _ = backend
                                                    .cmd_tx
                                                    .send(Command::AddWorkspace { name, path, ssh: None })
                                                    .await;
                                            }
                                        }
                                        KeyCode::Backspace => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.go_up();
                                            }
                                        }
                                        KeyCode::Char('.') => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.toggle_hidden();
                                            }
                                        }
                                        KeyCode::Char('/') => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.begin_path_edit();
                                            }
                                        }
                                        KeyCode::Tab => {
                                            if let Some(browser) = app.dir_browser_mut() {
                                                browser.enter_selected();
                                            }
                                        }
                                        KeyCode::Char(' ') => {
                                            let child_path = app
                                                .dir_browser
                                                .as_ref()
                                                .and_then(|b| b.selected_child_path());
                                            if let Some(path) = child_path {
                                                if let Some((name, path)) =
                                                    app.take_add_workspace_request_with_path(path)
                                                {
                                                    let _ = backend
                                                        .cmd_tx
                                                        .send(Command::AddWorkspace { name, path, ssh: None })
                                                        .await;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            } else if app.is_renaming_workspace() {
                                match key.code {
                                    KeyCode::Esc => app.cancel_rename_workspace(),
                                    KeyCode::Enter => {
                                        if let Some((id, name)) =
                                            app.take_rename_request_home()
                                        {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RenameWorkspace { id, name })
                                                .await;
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.rename_input_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.rename_input_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Esc => {
                                        app.go_home();
                                    }
                                    KeyCode::Enter => {
                                        if let Some(id) = app.selected_workspace_id() {
                                            app.open_workspace(id);
                                            start_workspace_tab_terminals(
                                                &backend.cmd_tx,
                                                id,
                                                &app.ws_tabs,
                                            )
                                            .await;
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RefreshGit { id })
                                                .await;
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::ClearAttention { id })
                                                .await;
                                        }
                                    }
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        app.move_home_selection(0, 1)
                                    }
                                    KeyCode::Up | KeyCode::Char('k') => app.move_home_selection(0, -1),
                                    KeyCode::Left | KeyCode::Char('h') => {
                                        app.move_home_selection(-1, 0)
                                    }
                                    KeyCode::Right | KeyCode::Char('l') => {
                                        app.move_home_selection(1, 0)
                                    }
                                    KeyCode::Char('n') => {
                                        let cwd = std::env::current_dir()
                                            .unwrap_or_else(|_| PathBuf::from("."))
                                            .display()
                                            .to_string();
                                        app.begin_add_workspace(cwd);
                                    }
                                    KeyCode::Char('R') => app.begin_add_ssh_workspace(),
                                    KeyCode::Char('D') => app.begin_delete_workspace(),
                                    KeyCode::Char('e') => app.begin_rename_workspace_home(),
                                    KeyCode::Char('S') => app.open_settings(),
                                    KeyCode::Char('!') => {
                                        if let Some(id) = app.selected_workspace_id() {
                                            let level = app
                                                .workspaces
                                                .get(app.home_selected)
                                                .map(|w| w.attention)
                                                .unwrap_or(AttentionLevel::None);
                                            let cmd = if matches!(
                                                level,
                                                AttentionLevel::NeedsInput | AttentionLevel::Error
                                            ) {
                                                Command::ClearAttention { id }
                                            } else {
                                                Command::SetAttention {
                                                    id,
                                                    level: AttentionLevel::NeedsInput,
                                                }
                                            };
                                            let _ = backend.cmd_tx.send(cmd).await;
                                        }
                                    }
                                    KeyCode::Char('g') => {
                                        if let Some(id) = app.selected_workspace_id() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RefreshGit { id })
                                                .await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Route::Workspace { id } => {
                            if app.is_renaming_tab() {
                                match key.code {
                                    KeyCode::Esc => app.cancel_rename_tab(),
                                    KeyCode::Enter => app.apply_rename_tab(),
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.rename_tab_input_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.rename_tab_input_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_renaming_workspace() {
                                match key.code {
                                    KeyCode::Esc => app.cancel_rename_workspace(),
                                    KeyCode::Enter => {
                                        if let Some((id, name)) = app.take_rename_request() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::RenameWorkspace { id, name })
                                                .await;
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.rename_input_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.rename_input_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_creating_branch() {
                                match key.code {
                                    KeyCode::Esc => { app.cancel_create_branch(); }
                                    KeyCode::Enter => {
                                        if let Some(name) = app.create_branch_input.take() {
                                            let trimmed = name.trim().to_string();
                                            if !trimmed.is_empty() {
                                                app.ws_pending_select_head_branch = true;
                                                let _ = backend
                                                    .cmd_tx
                                                    .send(Command::GitCreateBranch {
                                                        id,
                                                        branch: trimmed,
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.create_branch_input.as_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.create_branch_input.as_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_committing() {
                                match key.code {
                                    KeyCode::Esc => { app.commit_input = None; }
                                    KeyCode::Enter => {
                                        if let Some(msg) = app.commit_input.take() {
                                            let trimmed = msg.trim().to_string();
                                            if !trimmed.is_empty() {
                                                let _ = backend
                                                    .cmd_tx
                                                    .send(Command::GitCommit {
                                                        id,
                                                        message: trimmed,
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.commit_input.as_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.commit_input.as_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_confirming_discard() {
                                match key.code {
                                    KeyCode::Char('y') | KeyCode::Enter => {
                                        if let Some(file) = app.take_discard_file() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::GitDiscardFile { id, file })
                                                .await;
                                        }
                                    }
                                    KeyCode::Char('n') | KeyCode::Esc => {
                                        app.cancel_discard();
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_confirming_stash_pull_pop() {
                                match key.code {
                                    KeyCode::Char('y') | KeyCode::Enter => {
                                        if let Some(ws_id) = app.take_stash_pull_pop() {
                                            app.begin_git_op(ws_id);
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::GitStashPullPop { id: ws_id })
                                                .await;
                                        }
                                    }
                                    KeyCode::Char('n') | KeyCode::Esc => {
                                        app.cancel_stash_pull_pop();
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if app.is_stashing() {
                                match key.code {
                                    KeyCode::Esc => { app.stash_input = None; }
                                    KeyCode::Enter => {
                                        if let Some(msg) = app.stash_input.take() {
                                            let message = if msg.trim().is_empty() { None } else { Some(msg) };
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::GitStash { id, message })
                                                .await;
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if let Some(input) = app.stash_input.as_mut() {
                                            input.pop();
                                        }
                                    }
                                    KeyCode::Char(c) => {
                                        if let Some(input) = app.stash_input.as_mut() {
                                            input.push(c);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            // Ctrl+G toggles terminal passthrough mode.
                            if key.code == KeyCode::Char('g')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                                && matches!(app.focus, app::Focus::WsTerminal)
                            {
                                app.toggle_active_tab_passthrough();
                                continue;
                            }

                            // In passthrough mode, forward everything (including Esc/Tab)
                            // to the terminal.
                            if app.active_tab_passthrough()
                                && matches!(app.focus, app::Focus::WsTerminal)
                            {
                                if let Some(bytes) = key_to_terminal_bytes(key) {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::SendTerminalInput {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                            data_b64: base64::engine::general_purpose::STANDARD
                                                .encode(bytes),
                                        })
                                        .await;
                                }
                                continue;
                            }

                            // Shift+F toggles terminal fullscreen from any workspace pane.
                            if key.code == KeyCode::Char('F') {
                                app.toggle_terminal_fullscreen();
                                continue;
                            }

                            if key.code == KeyCode::Esc {
                                if matches!(app.focus, app::Focus::WsTerminal) {
                                    app.focus = app::Focus::WsTerminalTabs;
                                } else {
                                    app.go_home();
                                }
                                continue;
                            }

                            if matches!(app.focus, app::Focus::WsTerminal)
                                && key.code != KeyCode::Tab
                                && key.code != KeyCode::BackTab
                            {
                                if let Some(bytes) = key_to_terminal_bytes(key) {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::SendTerminalInput {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                            data_b64: base64::engine::general_purpose::STANDARD
                                                .encode(bytes),
                                        })
                                        .await;
                                    continue;
                                }
                            }

                            match key.code {
                                KeyCode::Enter => {
                                    if matches!(app.focus, app::Focus::WsLog) {
                                        match app.log_item_at(app.ws_selected_commit) {
                                            app::LogItem::UncommittedHeader => {
                                                app.ws_uncommitted_expanded = !app.ws_uncommitted_expanded;
                                            }
                                            app::LogItem::ChangedFile(_) => {
                                                if let Some(file) = app.selected_log_file() {
                                                    let _ = backend
                                                        .cmd_tx
                                                        .send(Command::LoadDiff { id, file })
                                                        .await;
                                                }
                                            }
                                            app::LogItem::Commit(ci) => {
                                                if app.ws_expanded_commit == Some(ci) {
                                                    app.ws_expanded_commit = None;
                                                } else {
                                                    app.ws_expanded_commit = Some(ci);
                                                    if let Some(hash) = app.selected_commit_hash() {
                                                        if !app.commit_files_cache.contains_key(&hash) {
                                                            let _ = backend
                                                                .cmd_tx
                                                                .send(Command::LoadCommitFiles { id, hash })
                                                                .await;
                                                        }
                                                    }
                                                }
                                            }
                                            app::LogItem::CommitFile(_, _) => {
                                                if let Some((hash, file)) = app.selected_commit_file() {
                                                    let _ = backend
                                                        .cmd_tx
                                                        .send(Command::LoadCommitFileDiff { id, hash, file })
                                                        .await;
                                                }
                                            }
                                        }
                                    }
                                }
                                KeyCode::Tab => {
                                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.focus = cycle_workspace_focus_reverse(app.focus);
                                    } else {
                                        app.focus = cycle_workspace_focus(app.focus);
                                    }
                                }
                                KeyCode::BackTab => {
                                    app.focus = cycle_workspace_focus_reverse(app.focus)
                                }
                                KeyCode::Char('g') => {
                                    let _ = backend.cmd_tx.send(Command::RefreshGit { id }).await;
                                }
                                KeyCode::Down | KeyCode::Char('j') => match app.focus {
                                    app::Focus::WsLog => {
                                        app.move_workspace_commit_selection(1);
                                        if let Some(file) = app.selected_log_file() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::LoadDiff { id, file })
                                                .await;
                                        } else if let Some((hash, file)) = app.selected_commit_file() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::LoadCommitFileDiff { id, hash, file })
                                                .await;
                                        }
                                    }
                                    app::Focus::WsBranches => {
                                        app.move_branch_selection(1);
                                    }
                                    app::Focus::WsDiff => {
                                        app.ws_diff_scroll = app.ws_diff_scroll.saturating_add(1)
                                    }
                                    _ => {}
                                },
                                KeyCode::Up | KeyCode::Char('k') => match app.focus {
                                    app::Focus::WsLog => {
                                        app.move_workspace_commit_selection(-1);
                                        if let Some(file) = app.selected_log_file() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::LoadDiff { id, file })
                                                .await;
                                        } else if let Some((hash, file)) = app.selected_commit_file() {
                                            let _ = backend
                                                .cmd_tx
                                                .send(Command::LoadCommitFileDiff { id, hash, file })
                                                .await;
                                        }
                                    }
                                    app::Focus::WsBranches => {
                                        app.move_branch_selection(-1);
                                    }
                                    app::Focus::WsDiff => {
                                        app.ws_diff_scroll = app.ws_diff_scroll.saturating_sub(1)
                                    }
                                    _ => {}
                                },
                                KeyCode::Char(' ')
                                    if matches!(app.focus, app::Focus::WsLog)
                                        && matches!(app.log_item_at(app.ws_selected_commit), app::LogItem::ChangedFile(_)) =>
                                {
                                    // Toggle stage/unstage selected file
                                    if let app::LogItem::ChangedFile(fi) = app.log_item_at(app.ws_selected_commit) {
                                        if let Some(git) = app.workspace_git.get(&id) {
                                            if let Some(f) = git.changed.get(fi) {
                                                let file = f.path.clone();
                                                let is_staged = f.index_status != ' ' && f.index_status != '?';
                                                let cmd = if is_staged {
                                                    Command::GitUnstageFile { id, file }
                                                } else {
                                                    Command::GitStageFile { id, file }
                                                };
                                                let _ = backend.cmd_tx.send(cmd).await;
                                            }
                                        }
                                    }
                                }
                                KeyCode::Char('+')
                                    if matches!(app.focus, app::Focus::WsLog)
                                        && app.log_item_is_file_context() =>
                                {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::GitStageAll { id })
                                        .await;
                                }
                                KeyCode::Char('-')
                                    if matches!(app.focus, app::Focus::WsLog)
                                        && app.log_item_is_file_context() =>
                                {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::GitUnstageAll { id })
                                        .await;
                                }
                                KeyCode::Char('c')
                                    if matches!(app.focus, app::Focus::WsLog)
                                        && app.log_item_is_file_context() =>
                                {
                                    app.commit_input = Some(String::new());
                                }
                                KeyCode::Char('d')
                                    if matches!(app.focus, app::Focus::WsLog)
                                        && matches!(app.log_item_at(app.ws_selected_commit), app::LogItem::ChangedFile(_)) =>
                                {
                                    app.begin_discard();
                                }
                                KeyCode::Char('s')
                                    if matches!(app.focus, app::Focus::WsLog)
                                        && app.log_item_is_file_context() =>
                                {
                                    app.stash_input = Some(String::new());
                                }
                                KeyCode::Char('t')
                                    if matches!(app.focus, app::Focus::WsLog) =>
                                {
                                    app.ws_tag_filter = !app.ws_tag_filter;
                                    app.ws_selected_commit = app.ws_selected_commit.min(app.total_log_items().saturating_sub(1));
                                }
                                KeyCode::Char('c')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    if matches!(app.ws_branch_sub_pane, app::BranchSubPane::Local) {
                                        app.begin_create_branch();
                                    }
                                }
                                KeyCode::Char('[')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    app.toggle_branch_sub_pane(app::BranchSubPane::Local);
                                }
                                KeyCode::Char(']')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    app.toggle_branch_sub_pane(app::BranchSubPane::Remote);
                                }
                                KeyCode::Char(' ')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    match app.ws_branch_sub_pane {
                                        app::BranchSubPane::Local => {
                                            if let Some(branch) = app.selected_local_branch() {
                                                if !branch.is_head {
                                                    let branch_name = branch.name.clone();
                                                    let _ = backend
                                                        .cmd_tx
                                                        .send(Command::GitCheckoutBranch {
                                                            id,
                                                            branch: branch_name,
                                                        })
                                                        .await;
                                                }
                                            }
                                        }
                                        app::BranchSubPane::Remote => {
                                            if let Some(rb) = app.selected_remote_branch() {
                                                let full = rb.full_name.clone();
                                                if let Some(local_name) = full.splitn(2, '/').nth(1) {
                                                    let local_name = local_name.to_string();
                                                    app.ws_pending_select_head_branch = true;
                                                    app.ws_branch_sub_pane = app::BranchSubPane::Local;
                                                    let _ = backend
                                                        .cmd_tx
                                                        .send(Command::GitCheckoutRemoteBranch {
                                                            id,
                                                            remote_branch: full,
                                                            local_name,
                                                        })
                                                        .await;
                                                }
                                            }
                                        }
                                    }
                                }
                                KeyCode::Char('p')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    let _ = backend.cmd_tx.send(Command::GitPull { id }).await;
                                    app.begin_git_op(id);
                                }
                                KeyCode::Char('f')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    let _ = backend.cmd_tx.send(Command::GitFetch { id }).await;
                                    app.begin_git_op(id);
                                }
                                KeyCode::Char('P')
                                    if matches!(app.focus, app::Focus::WsBranches) =>
                                {
                                    let _ = backend.cmd_tx.send(Command::GitPush { id }).await;
                                    app.begin_git_op(id);
                                }
                                KeyCode::Char('1') => app.set_active_tab_index(0),
                                KeyCode::Char('2') => app.set_active_tab_index(1),
                                KeyCode::Right | KeyCode::Char('l')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    app.move_terminal_tab(1);
                                }
                                KeyCode::Left | KeyCode::Char('h')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    app.move_terminal_tab(-1);
                                }
                                KeyCode::Char('n')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    app.add_shell_tab();
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StartTerminal {
                                            id,
                                            kind: TerminalKind::Shell,
                                            tab_id: Some(app.active_tab_id()),
                                            cmd: Vec::new(),
                                        })
                                        .await;
                                }
                                KeyCode::Char('x')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    if let Some(closed) = app.close_active_tab() {
                                        let _ = backend
                                            .cmd_tx
                                            .send(Command::StopTerminal {
                                                id,
                                                kind: closed.kind,
                                                tab_id: Some(closed.id),
                                            })
                                            .await;
                                    }
                                }
                                KeyCode::Char('r')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    app.begin_rename_tab();
                                }
                                KeyCode::Char('a')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StartTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                            cmd: Vec::new(),
                                        })
                                        .await;
                                    app.focus = app::Focus::WsTerminal;
                                }
                                KeyCode::Char('A')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StopTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                        })
                                        .await;
                                }
                                KeyCode::Char('s')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StartTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                            cmd: Vec::new(),
                                        })
                                        .await;
                                }
                                KeyCode::Char('S')
                                    if matches!(app.focus, app::Focus::WsTerminalTabs) =>
                                {
                                    let _ = backend
                                        .cmd_tx
                                        .send(Command::StopTerminal {
                                            id,
                                            kind: app.active_tab_kind(),
                                            tab_id: Some(app.active_tab_id()),
                                        })
                                        .await;
                                }
                                _ => {}
                            }
                        }
                    };
                }
                Event::Paste(text) => {
                    if matches!(app.focus, app::Focus::WsTerminal) {
                        if let Route::Workspace { id } = app.route {
                            // Wrap pasted text in bracketed paste sequences so the
                            // inner shell/editor knows it is a paste (prevents
                            // unintended interpretation of special characters).
                            let mut payload = Vec::new();
                            payload.extend_from_slice(b"\x1b[200~");
                            payload.extend_from_slice(text.as_bytes());
                            payload.extend_from_slice(b"\x1b[201~");
                            let _ = backend
                                .cmd_tx
                                .send(Command::SendTerminalInput {
                                    id,
                                    kind: app.active_tab_kind(),
                                    tab_id: Some(app.active_tab_id()),
                                    data_b64: base64::engine::general_purpose::STANDARD
                                        .encode(payload),
                                })
                                .await;
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    handle_mouse(&mut app, &backend.cmd_tx, &mut terminal, mouse).await;
                }
                _ => {}
            }
        }

        if last_flash_toggle.elapsed() >= Duration::from_millis(250) {
            app.spinner_tick = app.spinner_tick.wrapping_add(1);
            last_flash_toggle = Instant::now();
        }
    }

    disable_raw_mode()?;
    std::io::stdout().execute(DisableBracketedPaste)?;
    std::io::stdout().execute(DisableMouseCapture)?;
    std::io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn apply_event(app: &mut TuiApp, evt: CoreEvent) {
    match evt {
        CoreEvent::WorkspaceList { items } => app.set_workspaces(items),
        CoreEvent::WorkspaceGitUpdated { id, git } => app.set_workspace_git(id, git),
        CoreEvent::WorkspaceDiffUpdated { id, file, diff } => {
            app.set_workspace_diff(id, file, diff)
        }
        CoreEvent::CommitFilesLoaded { id: _, hash, files } => {
            app.commit_files_cache.insert(hash, files);
        }
        CoreEvent::TerminalOutput {
            id,
            kind: _,
            data_b64,
            tab_id,
            ..
        } => {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                let tid = tab_id.unwrap_or_else(|| "shell".to_string());
                app.append_terminal_bytes(id, &tid, &bytes);
            }
        }
        CoreEvent::TerminalExited {
            id,
            kind: _,
            code,
            tab_id,
            ..
        } => {
            let msg = format!("\r\n[terminal exited: {:?}]\r\n", code);
            let tid = tab_id.unwrap_or_else(|| "shell".to_string());
            app.append_terminal_bytes(id, &tid, msg.as_bytes());
        }
        CoreEvent::TerminalStarted {
            id,
            kind: _,
            tab_id,
            ..
        } => {
            let tid = tab_id.unwrap_or_else(|| "shell".to_string());
            app.reset_terminal(id, &tid);
            app.append_terminal_bytes(id, &tid, b"[terminal started]\r\n");
        }
        CoreEvent::GitActionResult {
            id,
            ref action,
            success,
            ref message,
        } => {
            if action == "pull_dirty_tree" && !success {
                // Cancel the spinner and show confirmation modal instead of toast
                let _ = app.finish_git_op(id);
                app.begin_stash_pull_pop(id);
            } else if app.finish_git_op(id) {
                app.git_action_message = Some((message.clone(), std::time::Instant::now()));
            } else {
                // Spinner minimum duration not met; defer the toast.
                app.deferred_git_result = Some((id, message.clone()));
            }
        }
        CoreEvent::WorkspaceAttentionChanged { id, level } => {
            if let Some(ws) = app.workspaces.iter_mut().find(|w| w.id == id) {
                ws.attention = level;
            }
        }
        _ => {}
    }
}

fn cycle_workspace_focus(focus: app::Focus) -> app::Focus {
    match focus {
        app::Focus::WsTerminalTabs => app::Focus::WsTerminal,
        app::Focus::WsTerminal => app::Focus::WsLog,
        app::Focus::WsLog => app::Focus::WsBranches,
        app::Focus::WsBranches => app::Focus::WsDiff,
        app::Focus::WsDiff => app::Focus::WsTerminalTabs,
        _ => app::Focus::WsTerminalTabs,
    }
}

fn cycle_workspace_focus_reverse(focus: app::Focus) -> app::Focus {
    match focus {
        app::Focus::WsTerminalTabs => app::Focus::WsDiff,
        app::Focus::WsTerminal => app::Focus::WsTerminalTabs,
        app::Focus::WsLog => app::Focus::WsTerminal,
        app::Focus::WsBranches => app::Focus::WsLog,
        app::Focus::WsDiff => app::Focus::WsBranches,
        _ => app::Focus::WsTerminalTabs,
    }
}

fn key_to_terminal_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let b = (c as u8) & 0x1f;
                Some(vec![b])
            } else {
                Some(c.to_string().into_bytes())
            }
        }
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                Some(vec![b'\n'])
            } else {
                Some(vec![b'\r'])
            }
        }
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        _ => None,
    }
}

fn apply_path_autocomplete(input: &mut String) {
    let current = input.trim();
    let (dir, prefix) = split_dir_and_prefix(current);
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) {
            matches.push((name, entry.path().is_dir()));
        }
    }
    if matches.is_empty() {
        return;
    }
    matches.sort_by(|a, b| a.0.cmp(&b.0));

    let common = longest_common_prefix(
        &matches
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
    );
    let replacement = if common.len() > prefix.len() {
        common
    } else {
        matches[0].0.clone()
    };

    let mut completed = if dir.as_os_str().is_empty() || dir == Path::new(".") {
        replacement
    } else {
        format!("{}/{}", dir.display(), replacement)
    };

    if matches.len() == 1 && matches[0].1 {
        completed.push('/');
    }
    *input = completed;
}

fn split_dir_and_prefix(input: &str) -> (PathBuf, String) {
    if input.is_empty() {
        return (PathBuf::from("."), String::new());
    }
    if input.ends_with('/') {
        return (PathBuf::from(input), String::new());
    }
    let path = Path::new(input);
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let prefix = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    (dir, prefix)
}

fn longest_common_prefix(parts: &[&str]) -> String {
    let Some(first) = parts.first() else {
        return String::new();
    };
    let mut end = first.len();
    for part in parts.iter().skip(1) {
        while end > 0 && !part.starts_with(&first[..end]) {
            end -= 1;
        }
        if end == 0 {
            break;
        }
    }
    first[..end].to_string()
}

async fn handle_mouse(
    app: &mut TuiApp,
    cmd_tx: &tokio::sync::mpsc::Sender<Command>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mouse: MouseEvent,
) {
    let area = match terminal.size() {
        Ok(s) => ratatui::layout::Rect::new(0, 0, s.width, s.height),
        Err(_) => return,
    };

    // Handle drag selection (works across all routes/panes)
    match mouse.kind {
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(sel) = &mut app.mouse_selection {
                sel.end_col = mouse.column;
                sel.end_row = mouse.row;
            }
            return;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(sel) = app.mouse_selection.take() {
                if !sel.is_empty() {
                    app.pending_copy_selection = Some(sel);
                }
            }
            return;
        }
        _ => {}
    }

    match app.route {
        Route::Home => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                app.mouse_selection = Some(app::MouseSelection::at(mouse.column, mouse.row));
                if app.is_confirming_delete() {
                    let rect = ui::screens::home::delete_modal_rect(area);
                    if point_in_rect(rect, mouse.column, mouse.row) {
                        let mid = rect.x + rect.width / 2;
                        if mouse.column < mid {
                            if let Some(id) = app.take_delete_workspace() {
                                let _ = cmd_tx.send(Command::RemoveWorkspace { id }).await;
                            }
                        } else {
                            app.cancel_delete_workspace();
                        }
                    } else {
                        app.cancel_delete_workspace();
                    }
                    return;
                }
                if app.is_adding_workspace() {
                    let rect = ui::screens::home::add_modal_rect(area);
                    if !point_in_rect(rect, mouse.column, mouse.row) {
                        app.cancel_add_workspace();
                    }
                    return;
                }

                let grid = ui::screens::home::grid_rect(area);
                if let Some(idx) = ui::widgets::tile_grid::index_at(
                    grid,
                    mouse.column,
                    mouse.row,
                    app.workspaces.len(),
                ) {
                    app.set_home_selection(idx);
                    if let Some(id) = app.selected_workspace_id() {
                        app.open_workspace(id);
                        start_workspace_tab_terminals(cmd_tx, id, &app.ws_tabs).await;
                        let _ = cmd_tx.send(Command::RefreshGit { id }).await;
                        let _ = cmd_tx.send(Command::ClearAttention { id }).await;
                    }
                }
            }
            _ => {}
        },
        Route::Workspace { id } => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                app.mouse_selection = Some(app::MouseSelection::at(mouse.column, mouse.row));
                if let Some(hit) =
                    ui::screens::workspace::hit_test(area, app, mouse.column, mouse.row)
                {
                    match hit {
                        ui::screens::workspace::WorkspaceHit::Header => {}
                        ui::screens::workspace::WorkspaceHit::TerminalTab(idx) => {
                            app.focus = app::Focus::WsTerminalTabs;
                            app.set_active_tab_index(idx);
                        }
                        ui::screens::workspace::WorkspaceHit::TerminalPane => {
                            app.focus = app::Focus::WsTerminal;
                        }
                        ui::screens::workspace::WorkspaceHit::LogList(idx) => {
                            app.focus = app::Focus::WsLog;
                            app.ws_selected_commit = idx;
                            if let Some(file) = app.selected_log_file() {
                                let _ = cmd_tx.send(Command::LoadDiff { id, file }).await;
                            } else if let Some((hash, file)) = app.selected_commit_file() {
                                let _ = cmd_tx.send(Command::LoadCommitFileDiff { id, hash, file }).await;
                            }
                        }
                        ui::screens::workspace::WorkspaceHit::BranchesPane(idx) => {
                            app.focus = app::Focus::WsBranches;
                            match app.ws_branch_sub_pane {
                                app::BranchSubPane::Local => {
                                    app.ws_selected_local_branch = idx;
                                }
                                app::BranchSubPane::Remote => {
                                    app.ws_selected_remote_branch = idx;
                                }
                            }
                        }
                        ui::screens::workspace::WorkspaceHit::DiffPane => {
                            app.focus = app::Focus::WsDiff;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                let hit = ui::screens::workspace::hit_test(area, app, mouse.column, mouse.row);
                if matches!(app.focus, app::Focus::WsDiff)
                    || matches!(hit, Some(ui::screens::workspace::WorkspaceHit::DiffPane))
                {
                    app.ws_diff_scroll = app.ws_diff_scroll.saturating_sub(3);
                } else if matches!(app.focus, app::Focus::WsTerminal)
                    || matches!(
                        hit,
                        Some(ui::screens::workspace::WorkspaceHit::TerminalPane)
                    )
                {
                    let tab_id = app.active_tab_id();
                    app.scroll_terminal_scrollback(id, &tab_id, 3);
                }
            }
            MouseEventKind::ScrollDown => {
                let hit = ui::screens::workspace::hit_test(area, app, mouse.column, mouse.row);
                if matches!(app.focus, app::Focus::WsDiff)
                    || matches!(hit, Some(ui::screens::workspace::WorkspaceHit::DiffPane))
                {
                    app.ws_diff_scroll = app.ws_diff_scroll.saturating_add(3);
                } else if matches!(app.focus, app::Focus::WsTerminal)
                    || matches!(
                        hit,
                        Some(ui::screens::workspace::WorkspaceHit::TerminalPane)
                    )
                {
                    let tab_id = app.active_tab_id();
                    app.scroll_terminal_scrollback(id, &tab_id, -3);
                }
            }
            _ => {}
        },
    }
}

fn point_in_rect(r: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    x >= r.x && y >= r.y && x < r.right() && y < r.bottom()
}

/// xterm-256 colour 39 — a medium sky-blue used for mouse selection highlighting.
const SELECTION_BG: ratatui::style::Color = ratatui::style::Color::Indexed(39);

fn apply_selection_highlight(frame: &mut ratatui::Frame, sel: &app::MouseSelection) {
    let ((start_col, start_row), (end_col, end_row)) = sel.ordered();
    let buf = frame.buffer_mut();
    let width = buf.area.width;
    for row in start_row..=end_row {
        let row_start = if row == start_row { start_col } else { 0 };
        let row_end = if row == end_row {
            end_col
        } else {
            width.saturating_sub(1)
        };
        for col in row_start..=row_end {
            if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(col, row)) {
                cell.set_style(
                    ratatui::style::Style::default()
                        .bg(SELECTION_BG)
                        .fg(ratatui::style::Color::Black),
                );
            }
        }
    }
}

fn extract_selected_text_from_buf(
    buf: &ratatui::buffer::Buffer,
    sel: &app::MouseSelection,
) -> String {
    let ((start_col, start_row), (end_col, end_row)) = sel.ordered();
    let width = buf.area.width;
    let mut result = String::new();
    for row in start_row..=end_row {
        let row_start = if row == start_row { start_col } else { 0 };
        let row_end = if row == end_row {
            end_col
        } else {
            width.saturating_sub(1)
        };
        let mut line = String::new();
        for col in row_start..=row_end {
            if let Some(cell) = buf.cell(ratatui::layout::Position::new(col, row)) {
                line.push_str(cell.symbol());
            }
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line.trim_end());
    }
    result
}

async fn start_workspace_tab_terminals(
    cmd_tx: &tokio::sync::mpsc::Sender<Command>,
    id: protocol::WorkspaceId,
    tabs: &[app::TerminalTab],
) {
    for tab in tabs {
        let _ = cmd_tx
            .send(Command::StartTerminal {
                id,
                kind: tab.kind,
                tab_id: Some(tab.id.clone()),
                cmd: Vec::new(),
            })
            .await;
    }
}

