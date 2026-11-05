use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::SystemTime;

use crate::config::ServerConfig;

/// Lifecycle state of a server process, as tracked between frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

impl ServerState {
    pub fn label(&self) -> &'static str {
        match self {
            ServerState::Stopped => "Stopped",
            ServerState::Starting => "Starting",
            ServerState::Running => "Running",
            ServerState::Stopping => "Stopping",
        }
    }
}

/// Which pipe a log line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// One line of server output, drained from the reader threads.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub stream: LogStream,
    pub text: String,
    pub timestamp: SystemTime,
}

/// Owns a running (or stopped) server child process.
///
/// Stdout/stderr are drained on background threads into a channel; the UI
/// drains it with `poll_logs` each frame and calls `update` to track
/// process state.
pub struct ServerHandle {
    state: ServerState,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    log_rx: Receiver<LogLine>,
}

impl ServerHandle {
    pub fn state(&self) -> ServerState {
        self.state
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Start the server with the given `java` command — either `"java"` to use
    /// the system PATH, or the path to a `bin/java[.exe]` from a managed runtime.
    pub fn start(config: &ServerConfig, java: &str) -> std::io::Result<Self> {
        let mut command = Command::new(java);
        command
            .current_dir(&config.working_dir)
            .arg(format!("-Xms{}M", config.min_memory_mb))
            .arg(format!("-Xmx{}M", config.max_memory_mb))
            .args(&config.jvm_args)
            .arg("-jar")
            .arg(&config.jar_path)
            .args(&config.server_args)
            .arg("--nogui")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;

            // Java is a console-subsystem process; don't create a visible console window.
            command.creation_flags(0x0800_0000);
        }

        let mut child = command.spawn()?;
        let stdin = child.stdin.take();

        let (log_tx, log_rx) = mpsc::channel();
        spawn_reader(child.stdout.take(), LogStream::Stdout, log_tx.clone());
        spawn_reader(child.stderr.take(), LogStream::Stderr, log_tx);

        Ok(Self {
            state: ServerState::Starting,
            child: Some(child),
            stdin,
            log_rx,
        })
    }

    /// Drain any log lines produced since the last call.
    pub fn poll_logs(&mut self) -> Vec<LogLine> {
        let mut lines = Vec::new();
        while let Ok(line) = self.log_rx.try_recv() {
            lines.push(line);
        }
        lines
    }

    /// Send a command to the server console (e.g. "say hello").
    pub fn send_command(&mut self, command: &str) {
        if let Some(stdin) = &mut self.stdin {
            let _ = writeln!(stdin, "{command}");
            let _ = stdin.flush();
        }
    }

    /// Ask the server to shut down gracefully via the `stop` command.
    pub fn request_stop(&mut self) {
        self.send_command("stop");
        if self.child.is_some() {
            self.state = ServerState::Stopping;
        }
    }

    /// Kill the process immediately.
    pub fn kill(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
    }

    /// Refresh state from the child process. Call once per frame/tick.
    pub fn update(&mut self) {
        let Some(child) = &mut self.child else {
            self.state = ServerState::Stopped;
            return;
        };
        match child.try_wait() {
            Ok(Some(_status)) => {
                self.state = ServerState::Stopped;
                self.child = None;
                self.stdin = None;
            }
            Ok(None) => {
                if self.state == ServerState::Starting {
                    self.state = ServerState::Running;
                }
            }
            Err(_) => {}
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
    stream: LogStream,
    tx: mpsc::Sender<LogLine>,
) {
    if let Some(pipe) = pipe {
        std::thread::spawn(move || {
            let reader = BufReader::new(pipe);
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        let line = LogLine {
                            stream,
                            text,
                            timestamp: SystemTime::now(),
                        };
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
}
