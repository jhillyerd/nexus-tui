//! The local tool plane (environments plan, the v1 floor): execute the
//! four environment tools — `bash`, `read_file`, `write_file`,
//! `list_dir` — in this node's own workspace.
//!
//! A `tools/call` arrived HERE because the server routed it to this
//! node, so the arguments' `target` names this node; [`serve_call`]
//! validates it and runs the local path — the node-side twin of the
//! server's remote dispatch, matching the server's own workspace
//! executor semantics:
//!
//! - cwd default (the workspace directory), stdin on the null device
//! - per-call wall-clock timeout (`NEXUS_ENV_TIMEOUT_MS`, floor 1000)
//! - kill-tree on cancel/timeout (process group + `killpg`; `taskkill
//!   /T` elsewhere)
//! - byte caps enforced WHILE READING (`NEXUS_ENV_MAX_OUTPUT_BYTES`,
//!   floor 1024) — post-hoc truncation does not bound memory
//! - lexical path confinement for the fs tools (symlinks are NOT
//!   resolved — hygiene, not a security boundary)
//! - a bash non-zero exit status is DATA (`"exit status N\n" + output`)
//! - an over-cap kill answers Ok with the capped output plus an
//!   omission note; timeout and cancel remain errors carrying partial
//!   output
//!
//! The knobs reuse the server's names and floors so an operator tunes
//! the same numbers on both sides.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// Extra bytes past the cap that may accumulate before the kill fires:
/// the cap bounds accumulation at read, this slack keeps the kill off
/// the chunk boundary.
const CAP_SLACK_BYTES: usize = 4096;

/// How long a killed child's pipe may keep producing after the tree
/// kill before the drain gives up (bounded reap: SIGKILL is immediate,
/// but a grandchild holding the write end must not hang the result),
/// and how long a reaped exit status may take after EOF.
const REAP: Duration = Duration::from_secs(1);

/// The bash deadline knob: `NEXUS_ENV_TIMEOUT_MS`, default 60s, floor 1s.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const TIMEOUT_FLOOR_MS: u64 = 1_000;

/// The byte-cap knob: `NEXUS_ENV_MAX_OUTPUT_BYTES`, default 256 KiB,
/// floor 1 KiB — covers bash output AND `read_file`.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 262_144;
const MAX_OUTPUT_FLOOR_BYTES: usize = 1_024;

const INVALID_BASH_ARGS: &str = "invalid bash arguments: {\"target\", \"command\"}";
const INVALID_READ_FILE_ARGS: &str = "invalid read_file arguments: {\"target\", \"path\"}";
const INVALID_WRITE_FILE_ARGS: &str =
    "invalid write_file arguments: {\"target\", \"path\", \"content\"}";
const INVALID_LIST_DIR_ARGS: &str = "invalid list_dir arguments: {\"target\", \"path\"}";

// ---------------------------------------------------------------------------
// The workspace handle
// ---------------------------------------------------------------------------

/// The node's local workspace: a per-thread directory under a root, the
/// same shape the server freezes for its own workspace
/// (`<root>/<thread id>`).
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    thread_id: String,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>, thread_id: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            thread_id: thread_id.into(),
        }
    }

    /// `<root>/<thread id>` — the working directory every tool runs in.
    pub fn dir(&self) -> PathBuf {
        self.root.join(&self.thread_id)
    }

    /// Create the workspace directory (idempotent) — called before
    /// every tool execution, the lazy-create rule.
    fn ensure(&self) -> Result<(), String> {
        fs::create_dir_all(self.dir()).map_err(|e| format!("workspace unavailable: {e}"))
    }

    /// Resolve `user_path` against the workspace directory and refuse
    /// escapes: `..` segments fold LEXICALLY, absolute paths must
    /// already live inside the workspace, and the normalized result
    /// must be the workspace dir or under it. Symlinks are NOT
    /// resolved (the accepted v1 posture).
    fn confined(&self, user_path: &str) -> Result<PathBuf, String> {
        let dir = normalize(&self.dir().to_string_lossy());
        let joined = if user_path.starts_with('/') {
            user_path.to_owned()
        } else {
            format!("{dir}/{user_path}")
        };
        let normalized = normalize(&joined);
        if normalized == dir || normalized.starts_with(&format!("{dir}/")) {
            Ok(PathBuf::from(normalized))
        } else {
            Err(format!("path escapes the workspace: {user_path}"))
        }
    }
}

/// Lexical normalization: split on '/', fold '.' and '' away, pop on
/// '..' (never above the root), keep the leading '/' for absolute
/// paths. The workspace dir itself never contains '..' segments (the
/// root is operator config, the id registry-minted), so the fold is
/// stable. '/'-based by construction — the same lexical rule the
/// server applies.
fn normalize(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let folded = segments.join("/");
    if absolute {
        format!("/{folded}")
    } else {
        folded
    }
}

// ---------------------------------------------------------------------------
// The knobs
// ---------------------------------------------------------------------------

/// `NEXUS_ENV_TIMEOUT_MS` — one knob for the bash deadline. Unparseable
/// reads default; values under the floor clamp to the floor (the
/// server's `int_knob` rule).
pub fn timeout_ms_from_env() -> u64 {
    knob_from_env("NEXUS_ENV_TIMEOUT_MS", DEFAULT_TIMEOUT_MS, TIMEOUT_FLOOR_MS)
}

/// `NEXUS_ENV_MAX_OUTPUT_BYTES` — one knob for bash output AND
/// `read_file`, named and floored exactly like the server's.
pub fn max_output_bytes_from_env() -> usize {
    let bytes = knob_from_env(
        "NEXUS_ENV_MAX_OUTPUT_BYTES",
        DEFAULT_MAX_OUTPUT_BYTES as u64,
        MAX_OUTPUT_FLOOR_BYTES as u64,
    );
    usize::try_from(bytes).unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
}

fn knob_from_env(name: &str, default: u64, floor: u64) -> u64 {
    clamp_knob(std::env::var(name).ok().as_deref(), default, floor)
}

fn clamp_knob(raw: Option<&str>, default: u64, floor: u64) -> u64 {
    match raw {
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(n) => n.max(floor),
            Err(_) => default,
        },
        None => default,
    }
}

// ---------------------------------------------------------------------------
// The dispatch (the node-side twin of the server's remote dispatch)
// ---------------------------------------------------------------------------

/// Execute one env-tool call LOCALLY: the call arrived here because the
/// server routed it to this node, so the arguments' `target` names this
/// node and rewrites to the reserved local value. `Ok` results carry
/// their text as DATA (a non-zero bash exit included); `Err` becomes an
/// `isError:true` response carrying the message. Unknown tool names
/// refuse: a node serves the four compiled env tools only.
pub async fn serve_call(
    workspace: &Workspace,
    tool: &str,
    arguments: &Value,
    timeout_ms: u64,
    max_output_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String, String> {
    match tool {
        "bash" => {
            let args: BashArgs = parse(arguments, INVALID_BASH_ARGS)?;
            if args.target.trim().is_empty() {
                return Err("bash requires a non-empty `target`".to_owned());
            }
            if args.command.trim().is_empty() {
                return Err("bash requires a non-empty `command`".to_owned());
            }
            bash_local(
                workspace,
                &args.command,
                timeout_ms,
                max_output_bytes,
                cancel,
            )
            .await
        }
        "read_file" => {
            let args: PathArgs = parse(arguments, INVALID_READ_FILE_ARGS)?;
            require_target(&args.target, "read_file")?;
            workspace.ensure()?;
            let path = workspace.confined(&args.path)?;
            read_file_local(&args.path, &path, max_output_bytes)
        }
        "write_file" => {
            let args: WriteArgs = parse(arguments, INVALID_WRITE_FILE_ARGS)?;
            require_target(&args.target, "write_file")?;
            if args.path.trim().is_empty() {
                return Err("write_file requires a non-empty `path`".to_owned());
            }
            workspace.ensure()?;
            let path = workspace.confined(&args.path)?;
            write_file_local(&args.path, &path, &args.content)
        }
        "list_dir" => {
            let args: PathArgs = parse(arguments, INVALID_LIST_DIR_ARGS)?;
            require_target(&args.target, "list_dir")?;
            workspace.ensure()?;
            let path = workspace.confined(&args.path)?;
            list_dir_local(&args.path, &path)
        }
        _ => Err(format!("this node does not serve the tool \"{tool}\"")),
    }
}

/// Deserialize one tool's argument shape; a decode failure (missing or
/// mistyped fields) answers the per-tool wording the server's
/// executors use.
fn parse<T: DeserializeOwned>(arguments: &Value, invalid: &str) -> Result<T, String> {
    serde_json::from_value(arguments.clone()).map_err(|_| invalid.to_owned())
}

fn require_target(target: &str, tool: &str) -> Result<(), String> {
    if target.trim().is_empty() {
        Err(format!("{tool} requires a non-empty `target`"))
    } else {
        Ok(())
    }
}

#[derive(Deserialize)]
struct BashArgs {
    target: String,
    command: String,
}

#[derive(Deserialize)]
struct PathArgs {
    target: String,
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
struct WriteArgs {
    target: String,
    path: String,
    content: String,
}

// ---------------------------------------------------------------------------
// bash: capped streaming with kill-tree
// ---------------------------------------------------------------------------

/// Run one bash command in the workspace directory. stdout and stderr
/// are merged into one stream (the port behavior the server renders);
/// the accumulated output is cap-checked per chunk; the process tree
/// dies on cancel, timeout, and over-cap.
async fn bash_local(
    workspace: &Workspace,
    command: &str,
    timeout_ms: u64,
    max_output_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String, String> {
    workspace.ensure()?;
    // The std Command carries the unix prelude (process group + the
    // dup2 merge); tokio wraps it for the async read and reaping.
    let std_command = build_bash(command, workspace).map_err(|e| format!("bash: {e}"))?;
    let mut spawned = tokio::process::Command::from(std_command);
    // The executor task owning the child can be dropped mid-flight
    // (reconnect cleanup) — the child must not outlive it.
    spawned.kill_on_drop(true);
    let mut child = spawned.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "bash: no bash executable found on PATH".to_owned(),
        _ => format!("bash: {e}"),
    })?;

    // On non-unix the stderr pipe cannot be dup2-merged into stdout, so
    // a side task drains it concurrently and its bytes fold in at the
    // end — the write-order merge is the unix path's nicety.
    #[cfg(not(unix))]
    let stderr_task: Option<tokio::task::JoinHandle<Vec<u8>>> =
        child.stderr.take().map(|mut stderr| {
            tokio::spawn(async move {
                let mut drained = Vec::new();
                let _ = stderr.read_to_end(&mut drained).await;
                drained
            })
        });
    #[cfg(unix)]
    let stderr_task: Option<tokio::task::JoinHandle<Vec<u8>>> = None;

    let mut streams = Streams::new(child.stdout.take());
    let mut output = Output::new(max_output_bytes);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    match stream_until_finished(&mut output, &mut streams, deadline, cancel).await {
        Stream::Eof => match reap_exit(&mut child).await {
            Ok(code) => {
                collect_stderr(&mut output, stderr_task).await;
                finish(&output, code)
            }
            Err(()) => {
                kill_tree(&mut child).await;
                Err("bash: the child closed its output but never exited".to_owned())
            }
        },
        Stream::OverCap => {
            kill_tree(&mut child).await;
            // Drained chunks land in the buffer still cap-checked, but
            // the killed flag discards them — the rendered prefix is
            // what the over-cap note rides.
            drain(&mut output, &mut streams).await;
            collect_stderr(&mut output, stderr_task).await;
            Ok(over_cap(&output))
        }
        Stream::Cancelled => {
            kill_tree(&mut child).await;
            drain(&mut output, &mut streams).await;
            collect_stderr(&mut output, stderr_task).await;
            Err(format!("cancelled. partial output:\n{}", output.render()))
        }
        Stream::TimedOut => {
            kill_tree(&mut child).await;
            drain(&mut output, &mut streams).await;
            collect_stderr(&mut output, stderr_task).await;
            Err(format!(
                "timed out after {timeout_ms} ms. partial output:\n{}",
                output.render()
            ))
        }
    }
}

/// Fold the non-unix stderr drain into the output, bounded at [`REAP`].
#[cfg(not(unix))]
async fn collect_stderr(output: &mut Output, task: Option<tokio::task::JoinHandle<Vec<u8>>>) {
    if let Some(task) = task
        && let Ok(drained) = tokio::time::timeout(REAP, task).await
        && let Ok(drained) = drained
    {
        output.absorb(&drained);
    }
}

#[cfg(unix)]
async fn collect_stderr(output: &mut Output, task: Option<tokio::task::JoinHandle<Vec<u8>>>) {
    let _ = (output, task);
}

#[cfg(unix)]
fn build_bash(command_line: &str, workspace: &Workspace) -> std::io::Result<Command> {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(command_line)
        .current_dir(workspace.dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group: the tree kill signals the whole group, and a
        // mid-flight executor drop cannot orphan grandchildren behind
        // the direct child's corpse.
        .process_group(0);
    // The port merges both streams: dup2 stdout over stderr so the
    // child's output arrives in one pipe, in write order.
    unsafe {
        command.pre_exec(|| {
            if libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

#[cfg(not(unix))]
fn build_bash(command_line: &str, workspace: &Workspace) -> std::io::Result<Command> {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(command_line)
        .current_dir(workspace.dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

/// Kill the whole process group (unix) or the process tree via
/// `taskkill /T` (Windows); the direct-child kill is the fallback for
/// a child that already lost its group or raced the group kill.
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: kill(2) with a negative pid signals the process
        // group — the group `process_group(0)` put the child into.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output()
            .await;
    }
    let _ = child.start_kill();
}

/// Reap the exit status after EOF, bounded at [`REAP`] (the bounded
/// reap rule: the status is there when the pipe closes, but never
/// trust a child to die).
async fn reap_exit(child: &mut tokio::process::Child) -> Result<i32, ()> {
    let status = tokio::time::timeout(REAP, child.wait())
        .await
        .map_err(|_| ())?;
    let status = status.map_err(|_| ())?;
    status.code().map(Ok).unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Ok(128 + signal);
            }
        }
        Err(())
    })
}

enum Stream {
    /// The pipe closed (all read ends EOF).
    Eof,
    /// The cap + slack tripped; the kill must fire and the answer is
    /// the Ok omission note.
    OverCap,
    Cancelled,
    TimedOut,
}

/// The cap-checked accumulation buffer: chunks fold in until the cap +
/// slack trips, then the kill arms once and every later chunk is
/// DISCARDED (a gushing pipe between kill dispatch and child death
/// must not accumulate past the cap, wall-clock unbounded).
struct Output {
    bytes: Vec<u8>,
    max_output_bytes: usize,
    killed: bool,
}

impl Output {
    fn new(max_output_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_output_bytes,
            killed: false,
        }
    }

    fn absorb(&mut self, chunk: &[u8]) {
        if self.killed {
            return;
        }
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > self.max_output_bytes + CAP_SLACK_BYTES {
            self.killed = true;
        }
    }

    fn render(&self) -> String {
        render_output(&self.bytes, self.max_output_bytes)
    }
}

/// Cap the accumulated bytes to the cap and decode lossily; an empty
/// result renders as an explicit marker (a bare empty tool result reads
/// as breakage, not silence).
fn render_output(bytes: &[u8], max_output_bytes: usize) -> String {
    let (bytes, truncated) = if bytes.len() > max_output_bytes {
        (&bytes[..max_output_bytes], true)
    } else {
        (bytes, false)
    };
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let text = if text.is_empty() { "(no output)" } else { text };
    if truncated {
        format!("{text}\n[... truncated at {max_output_bytes} bytes ...]")
    } else {
        text.to_owned()
    }
}

/// A completed run: exit 0 is plain output, non-zero prefixes the
/// status (a failing command is data — the model reacts, the round
/// commits).
fn finish(output: &Output, code: i32) -> Result<String, String> {
    let rendered = output.render();
    if code == 0 {
        Ok(rendered)
    } else {
        Ok(format!("exit status {code}\n{rendered}"))
    }
}

/// Over-cap kill: the capped output returns as data with an omission
/// note naming the kill (the command itself may have been nowhere near
/// done — the note says it was killed).
fn over_cap(output: &Output) -> String {
    format!(
        "{}\n[... output exceeded {} bytes; the command was killed ...]",
        output.render(),
        output.max_output_bytes
    )
}

/// Read the merged output until EOF (then reap the exit status), the
/// cap trips (then the kill fires), the cancel token fires, or the
/// wall-clock deadline passes. The kill itself happens in the caller.
async fn stream_until_finished(
    output: &mut Output,
    streams: &mut Streams,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Stream {
    loop {
        if Instant::now() >= deadline {
            return Stream::TimedOut;
        }
        if output.killed {
            return Stream::OverCap;
        }
        tokio::select! {
            () = cancel.cancelled() => return Stream::Cancelled,
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Stream::TimedOut;
            }
            chunk = streams.read() => match chunk {
                Some(bytes) => output.absorb(&bytes),
                None => return Stream::Eof,
            },
        }
    }
}

/// Drain what remains after a tree kill, bounded at [`REAP`]: the
/// killed child's remaining buffered output lands (still cap-checked),
/// and the pipe is consumed instead of queueing garbage.
async fn drain(output: &mut Output, streams: &mut Streams) {
    let deadline = Instant::now() + REAP;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        tokio::select! {
            chunk = streams.read() => match chunk {
                Some(bytes) => output.absorb(&bytes),
                None => return,
            },
            () = tokio::time::sleep(remaining) => return,
        }
    }
}

/// The read end of the child's merged output stream (unix: stdout and
/// stderr share the one pipe via the dup2 prelude). All ends closed
/// reads as `None` (EOF).
struct Streams {
    reader: Option<tokio::process::ChildStdout>,
    buffer: Vec<u8>,
}

impl Streams {
    fn new(reader: Option<tokio::process::ChildStdout>) -> Self {
        Self {
            reader,
            buffer: vec![0; 8192],
        }
    }

    /// One chunk; `None` when the end is closed.
    async fn read(&mut self) -> Option<Vec<u8>> {
        let reader = self.reader.as_mut()?;
        match reader.read(&mut self.buffer).await {
            Ok(0) | Err(_) => {
                self.reader = None;
                None
            }
            Ok(n) => Some(self.buffer[..n].to_vec()),
        }
    }
}

// ---------------------------------------------------------------------------
// The fs tools
// ---------------------------------------------------------------------------

/// Read a workspace file, capped WHILE READING. Non-UTF-8 bytes are
/// decoded lossily (never a crash on binary content).
fn read_file_local(display: &str, path: &Path, max_output_bytes: usize) -> Result<String, String> {
    // REGULAR FILES ONLY: a workspace FIFO or device node would block
    // the read indefinitely — `bash` can create either inside the
    // workspace, so the guard is not hypothetical. Unreadable and
    // nonexistent paths fall through to the open, which answers its
    // own honest error.
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!("read_file: not a regular file: {display}"));
        }
        _ => {}
    }
    let mut file = fs::File::open(path).map_err(|e| format!("read_file: {e}"))?;
    let mut bytes = Vec::new();
    let mut limited = file.take(max_output_bytes as u64);
    limited
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read_file: {e}"))?;
    file = limited.into_inner();
    let mut probe = [0_u8; 1];
    let truncated = file
        .read(&mut probe)
        .map_err(|e| format!("read_file: {e}"))?
        > 0;
    let text = String::from_utf8_lossy(&bytes);
    if truncated {
        Ok(format!(
            "{text}\n[... truncated at {max_output_bytes} bytes ...]"
        ))
    } else {
        Ok(text.into_owned())
    }
}

/// Write a workspace file (whole-content replace), creating parent
/// directories as needed.
fn write_file_local(display: &str, path: &Path, content: &str) -> Result<String, String> {
    // An EXISTING non-regular target (a FIFO `bash` made) would block
    // the write until a reader arrives — refuse it; a not-yet-existing
    // path is a regular file to be created.
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!("write_file: not a regular file: {display}"));
        }
        _ => {}
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("write_file: {e}"))?;
    }
    fs::write(path, content).map_err(|e| format!("write_file: {e}"))?;
    Ok(format!("wrote {} bytes to {display}", content.len()))
}

/// Cap on listed entries, with an omission note past it (the server's
/// `list_dir_entry_cap`).
const LIST_DIR_ENTRY_CAP: usize = 500;

/// List a workspace directory: sorted entries, directories suffixed
/// '/', capped entries with an omission note.
fn list_dir_local(display: &str, path: &Path) -> Result<String, String> {
    if !path.is_dir() {
        return Err(format!("list_dir: not a directory: {display}"));
    }
    let mut names: Vec<String> = fs::read_dir(path)
        .map_err(|e| format!("list_dir: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("list_dir: {e}"))?
        .into_iter()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    if names.is_empty() {
        return Ok("(empty)".to_owned());
    }
    names.sort();
    let total = names.len();
    let mut lines: Vec<String> = names
        .into_iter()
        .take(LIST_DIR_ENTRY_CAP)
        .map(|name| {
            if path.join(&name).is_dir() {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    if total > LIST_DIR_ENTRY_CAP {
        lines.push(format!(
            "[... {} more entries omitted ...]",
            total - LIST_DIR_ENTRY_CAP
        ));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SHORT_TIMEOUT_MS: u64 = 800;
    const SHORT_CAP: usize = 1024;

    fn workspace(dir: &std::path::Path) -> Workspace {
        let ws = Workspace::new(dir, "thread-1");
        // Tests stage files before the first call creates the dir.
        std::fs::create_dir_all(ws.dir()).unwrap();
        ws
    }

    async fn call(ws: &Workspace, tool: &str, arguments: Value) -> Result<String, String> {
        serve_call(
            ws,
            tool,
            &arguments,
            SHORT_TIMEOUT_MS,
            SHORT_CAP,
            &CancellationToken::new(),
        )
        .await
    }

    fn bash(command: &str) -> Value {
        serde_json::json!({"target": "workspace", "command": command})
    }

    // ---------------------------------------------------------
    // bash
    // ---------------------------------------------------------

    #[tokio::test]
    async fn bash_runs_in_workspace_and_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            call(&ws, "bash", bash("pwd")).await,
            Ok(ws.dir().to_string_lossy().into_owned())
        );
    }

    #[tokio::test]
    async fn bash_merges_stderr_into_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            call(&ws, "bash", bash("echo to-err 1>&2")).await,
            Ok("to-err".to_owned())
        );
    }

    #[tokio::test]
    async fn bash_non_zero_exit_status_is_data() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            call(&ws, "bash", bash("echo out; exit 1")).await,
            Ok("exit status 1\nout".to_owned())
        );
    }

    #[tokio::test]
    async fn bash_empty_output_renders_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            call(&ws, "bash", bash("true")).await,
            Ok("(no output)".to_owned())
        );
    }

    #[tokio::test]
    async fn bash_over_cap_kill_answers_ok_with_omission_note() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let result = call(&ws, "bash", bash("yes")).await.unwrap();
        // The rendered answer carries BOTH notes, like the server's:
        // render_output truncates (its note) and the over-cap kill adds
        // the omission note.
        let body = result
            .strip_suffix(&format!(
                "\n[... output exceeded {SHORT_CAP} bytes; the command was killed ...]"
            ))
            .expect("over-cap note present")
            .strip_suffix(&format!("\n[... truncated at {SHORT_CAP} bytes ...]"))
            .expect("truncation note present");
        assert!(body.lines().all(|line| line == "y"), "capped prefix only");
    }

    #[tokio::test]
    async fn bash_timeout_is_error_carrying_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let arguments = bash("echo early; sleep 30");
        let result = call(&ws, "bash", arguments).await;
        assert_eq!(
            result,
            Err(format!(
                "timed out after {SHORT_TIMEOUT_MS} ms. partial output:\nearly"
            ))
        );
    }

    #[tokio::test]
    async fn bash_cancel_is_error_carrying_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = serve_call(&ws, "bash", &bash("sleep 30"), 30_000, SHORT_CAP, &cancel).await;
        assert_eq!(
            result,
            Err("cancelled. partial output:\n(no output)".to_owned())
        );
    }

    /// The kill-tree: a grandchild in the command's group dies with
    /// it — a `sleep` orphan must not wake up later and write its
    /// marker. `done` landing means the tree survived; the 2.5s wait
    /// is just longer than the orphan's own timer would be.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let command = "touch started; sleep 2 && touch done";
        let result = call(&ws, "bash", bash(command)).await;
        assert!(result.is_err());
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(ws.dir().join("started").exists());
        assert!(!ws.dir().join("done").exists());
    }

    #[tokio::test]
    async fn bash_argument_refusals_word_like_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            call(
                &ws,
                "bash",
                serde_json::json!({"target": "   ", "command": "ls"})
            )
            .await,
            Err("bash requires a non-empty `target`".to_owned())
        );
        assert_eq!(
            call(&ws, "bash", serde_json::json!({"command": "ls"})).await,
            Err("invalid bash arguments: {\"target\", \"command\"}".to_owned())
        );
        assert_eq!(
            call(
                &ws,
                "bash",
                serde_json::json!({"target": "me", "command": "   "})
            )
            .await,
            Err("bash requires a non-empty `command`".to_owned())
        );
        assert_eq!(
            call(&ws, "bash", serde_json::json!({"target": 4})).await,
            Err("invalid bash arguments: {\"target\", \"command\"}".to_owned())
        );
    }

    // ---------------------------------------------------------
    // Path confinement
    // ---------------------------------------------------------

    #[test]
    fn confined_resolves_relative_and_inner_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let inner = dir.path().join("thread-1").join("sub");
        assert_eq!(
            ws.confined("sub/notes.txt").unwrap(),
            inner.join("notes.txt")
        );
        let absolute = inner.join("notes.txt").to_string_lossy().into_owned();
        assert_eq!(ws.confined(&absolute).unwrap(), PathBuf::from(absolute));
        assert_eq!(
            ws.confined("./sub/../root.txt").unwrap(),
            ws.dir().join("root.txt")
        );
    }

    #[test_case("../outside.txt"; "parent escape")]
    #[test_case("sub/../../outside.txt"; "deep escape")]
    #[test_case("/etc/passwd"; "absolute outside")]
    fn confined_refuses_escapes(user_path: &str) {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            ws.confined(user_path),
            Err(format!("path escapes the workspace: {user_path}"))
        );
    }

    #[test]
    fn normalize_folds_lexically_like_the_server() {
        assert_eq!(normalize("/a/./b//c"), "/a/b/c");
        assert_eq!(normalize("/a/../b"), "/b");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("a/b/.."), "a");
    }

    // ---------------------------------------------------------
    // read_file
    // ---------------------------------------------------------

    #[tokio::test]
    async fn read_file_reads_and_refuses_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        fs::write(ws.dir().join("notes.txt"), "hello").unwrap();
        assert_eq!(
            call(
                &ws,
                "read_file",
                serde_json::json!({"target": "workspace", "path": "notes.txt"})
            )
            .await,
            Ok("hello".to_owned())
        );
        assert_eq!(
            call(
                &ws,
                "read_file",
                serde_json::json!({"target": "workspace", "path": "../etc/passwd"})
            )
            .await,
            Err("path escapes the workspace: ../etc/passwd".to_owned())
        );
        assert!(
            call(
                &ws,
                "read_file",
                serde_json::json!({"target": "workspace", "path": "missing.txt"})
            )
            .await
            .unwrap_err()
            .starts_with("read_file: ")
        );
    }

    #[tokio::test]
    async fn read_file_truncates_at_cap_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let content = "a".repeat(SHORT_CAP + 100);
        fs::write(ws.dir().join("big.txt"), &content).unwrap();
        let expected = format!(
            "{}\n[... truncated at {SHORT_CAP} bytes ...]",
            "a".repeat(SHORT_CAP)
        );
        assert_eq!(
            call(
                &ws,
                "read_file",
                serde_json::json!({"target": "workspace", "path": "big.txt"})
            )
            .await,
            Ok(expected)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_refuses_directories_and_non_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        fs::create_dir(ws.dir().join("subdir")).unwrap();
        assert_eq!(
            call(
                &ws,
                "read_file",
                serde_json::json!({"target": "workspace", "path": "subdir"})
            )
            .await,
            Err("read_file: not a regular file: subdir".to_owned())
        );
        std::process::Command::new("mkfifo")
            .arg(ws.dir().join("pipe"))
            .status()
            .unwrap();
        assert_eq!(
            call(
                &ws,
                "read_file",
                serde_json::json!({"target": "workspace", "path": "pipe"})
            )
            .await,
            Err("read_file: not a regular file: pipe".to_owned())
        );
    }

    /// Lexical confinement does NOT resolve symlinks (the recorded v1
    /// posture): a link inside the workspace pointing out reads the
    /// outside file — hygiene, not a security boundary.
    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_through_symlink_stays_lexical() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        let ws = workspace(dir.path());
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), ws.dir().join("link.txt"))
            .unwrap();
        assert_eq!(
            call(
                &ws,
                "read_file",
                serde_json::json!({"target": "workspace", "path": "link.txt"})
            )
            .await,
            Ok("outside".to_owned())
        );
    }

    // ---------------------------------------------------------
    // write_file
    // ---------------------------------------------------------

    #[tokio::test]
    async fn write_file_creates_parents_and_reports_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        let content = "ping";
        assert_eq!(
            call(&ws, "write_file", serde_json::json!({"target": "workspace", "path": "deep/dir/f.txt", "content": content})).await,
            Ok(format!("wrote {} bytes to deep/dir/f.txt", content.len()))
        );
        assert_eq!(
            fs::read_to_string(ws.dir().join("deep/dir/f.txt")).unwrap(),
            content
        );
    }

    #[tokio::test]
    async fn write_file_refuses_empty_path_and_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            call(
                &ws,
                "write_file",
                serde_json::json!({"target": "workspace", "path": "  ", "content": "x"})
            )
            .await,
            Err("write_file requires a non-empty `path`".to_owned())
        );
        assert_eq!(
            call(
                &ws,
                "write_file",
                serde_json::json!({"target": "workspace", "path": "../x", "content": "x"})
            )
            .await,
            Err("path escapes the workspace: ../x".to_owned())
        );
    }

    // ---------------------------------------------------------
    // list_dir
    // ---------------------------------------------------------

    #[tokio::test]
    async fn list_dir_sorts_suffixes_directories_and_reports_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            call(&ws, "list_dir", serde_json::json!({"target": "workspace"})).await,
            Ok("(empty)".to_owned())
        );
        fs::create_dir(ws.dir().join("zeta")).unwrap();
        fs::write(ws.dir().join("alpha.txt"), "").unwrap();
        fs::write(ws.dir().join("mid.txt"), "").unwrap();
        assert_eq!(
            call(&ws, "list_dir", serde_json::json!({"target": "workspace"})).await,
            Ok("alpha.txt\nmid.txt\nzeta/".to_owned())
        );
        assert_eq!(
            call(
                &ws,
                "list_dir",
                serde_json::json!({"target": "workspace", "path": "missing"})
            )
            .await,
            Err("list_dir: not a directory: missing".to_owned())
        );
    }

    #[tokio::test]
    async fn list_dir_omits_past_entry_cap_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        for i in 0..LIST_DIR_ENTRY_CAP + 3 {
            fs::write(ws.dir().join(format!("f{i:04}")), "").unwrap();
        }
        let result = call(&ws, "list_dir", serde_json::json!({"target": "workspace"}))
            .await
            .unwrap();
        assert!(result.contains("[... 3 more entries omitted ...]"));
        assert_eq!(result.lines().count(), LIST_DIR_ENTRY_CAP + 1);
    }

    // ---------------------------------------------------------
    // Dispatch
    // ---------------------------------------------------------

    #[tokio::test]
    async fn unknown_tool_refuses_like_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(dir.path());
        assert_eq!(
            call(&ws, "web_search", serde_json::json!({})).await,
            Err("this node does not serve the tool \"web_search\"".to_owned())
        );
    }

    #[test]
    fn knobs_clamp_to_the_server_floors() {
        assert_eq!(clamp_knob(None, 7, 3), 7);
        assert_eq!(clamp_knob(Some("NaN"), 7, 3), 7);
        assert_eq!(clamp_knob(Some("1"), 7, 3), 3);
        assert_eq!(clamp_knob(Some("99"), 7, 3), 99);
    }
}
