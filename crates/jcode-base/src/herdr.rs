//! Best-effort lifecycle reporting for Jcode processes running inside Herdr.
//!
//! Herdr injects `HERDR_ENV` and `HERDR_PANE_ID` into each managed pane. Jcode
//! uses `HERDR_BIN_PATH` when present and otherwise resolves `herdr` from `PATH`
//! to report semantic agent state through Herdr's public CLI. Reports run on a
//! dedicated thread so a slow or broken Herdr client never stalls model
//! streaming or terminal input.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

const SOURCE: &str = "custom:jcode";
const AGENT: &str = "jcode";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const RETRY_DELAY: Duration = Duration::from_millis(250);
const RELEASE_WAIT: Duration = Duration::from_millis(750);
const MAX_PENDING_REPORTS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Report {
    session_id: String,
    state: AgentState,
    message: Option<String>,
    seq: u64,
}

#[derive(Default)]
struct WorkerState {
    pending: VecDeque<Report>,
    release_seq: Option<u64>,
    stop_requested: bool,
    stopped: bool,
}

#[derive(Clone, Debug)]
struct Target {
    binary: OsString,
    pane_id: String,
}

/// Pane-scoped Herdr lifecycle reporter.
///
/// Construct one for the lifetime of the foreground Jcode command. Dropping it
/// releases Jcode's lifecycle authority after all queued reports have been sent.
pub struct Reporter {
    shared: Option<Arc<(Mutex<WorkerState>, Condvar)>>,
    worker: Option<thread::JoinHandle<()>>,
    last_report: Option<Report>,
    next_seq: u64,
}

impl Reporter {
    /// Enable reporting only in a fully identified Herdr pane.
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var_os("HERDR_ENV"),
            std::env::var_os("HERDR_BIN_PATH"),
            std::env::var_os("HERDR_PANE_ID"),
        )
    }

    fn from_values(
        herdr_env: Option<OsString>,
        binary: Option<OsString>,
        pane_id: Option<OsString>,
    ) -> Self {
        Self::from_values_with_seq(herdr_env, binary, pane_id, initial_sequence())
    }

    fn from_values_with_seq(
        herdr_env: Option<OsString>,
        binary: Option<OsString>,
        pane_id: Option<OsString>,
        initial_seq: u64,
    ) -> Self {
        let enabled = herdr_env
            .as_deref()
            .is_some_and(|value| value.to_string_lossy().trim() == "1");
        let binary = binary
            .filter(|value| !value.is_empty())
            .or_else(|| enabled.then(|| OsString::from("herdr")));
        let pane_id = pane_id
            .map(|value| value.to_string_lossy().trim().to_string())
            .filter(|value| !value.is_empty());
        let Some((binary, pane_id)) = binary.zip(pane_id).filter(|_| enabled) else {
            return Self::disabled();
        };

        let target = Target { binary, pane_id };
        let shared = Arc::new((Mutex::new(WorkerState::default()), Condvar::new()));
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("jcode-herdr-reporter".to_string())
            .spawn(move || run_worker(target, worker_shared))
            .ok();

        if worker.is_none() {
            return Self::disabled();
        }

        Self {
            shared: Some(shared),
            worker,
            last_report: None,
            next_seq: initial_seq,
        }
    }

    fn disabled() -> Self {
        Self {
            shared: None,
            worker: None,
            last_report: None,
            next_seq: 0,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.shared.is_some()
    }

    fn take_seq(&mut self) -> u64 {
        self.next_seq = self.next_seq.saturating_add(1);
        self.next_seq
    }

    pub fn report_idle(&mut self, session_id: &str) {
        self.report(session_id, AgentState::Idle, None);
    }

    pub fn report_working(&mut self, session_id: &str) {
        self.report(session_id, AgentState::Working, None);
    }

    /// Report a structured user-decision wait when a Jcode surface has one.
    pub fn report_blocked(&mut self, session_id: &str, message: impl Into<String>) {
        self.report(session_id, AgentState::Blocked, Some(message.into()));
    }

    /// Synchronize the normal Jcode turn lifecycle.
    pub fn sync_activity(&mut self, session_id: Option<&str>, is_processing: bool) {
        let Some(session_id) = session_id else {
            return;
        };
        if is_processing {
            self.report_working(session_id);
        } else {
            self.report_idle(session_id);
        }
    }

    fn report(&mut self, session_id: &str, state: AgentState, message: Option<String>) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        if self.last_report.as_ref().is_some_and(|report| {
            report.session_id == session_id && report.state == state && report.message == message
        }) {
            return;
        }
        let report = Report {
            session_id: session_id.to_string(),
            state,
            message: message.filter(|value| !value.trim().is_empty()),
            seq: self.take_seq(),
        };
        let Some(shared) = self.shared.as_ref() else {
            return;
        };
        let (state, wake) = &**shared;
        if let Ok(mut state) = state.lock()
            && state.release_seq.is_none()
        {
            if state.pending.len() == MAX_PENDING_REPORTS {
                state.pending.pop_front();
            }
            state.pending.push_back(report.clone());
            self.last_report = Some(report);
            wake.notify_one();
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        let Some(shared) = self.shared.take() else {
            return;
        };
        let (state, wake) = &*shared;
        let Ok(mut state) = state.lock() else {
            return;
        };
        if self.last_report.is_some() {
            state.release_seq = Some(self.take_seq());
        } else {
            state.stop_requested = true;
        }
        wake.notify_one();

        let deadline = Instant::now() + RELEASE_WAIT;
        while !state.stopped {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let Ok((next, _)) = wake.wait_timeout(state, deadline.saturating_duration_since(now))
            else {
                return;
            };
            state = next;
        }
        let stopped = state.stopped;
        drop(state);
        if stopped && let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Dropping a still-running JoinHandle detaches it. Exit is bounded even
        // when the Herdr CLI is unavailable or wedged.
    }
}

fn run_worker(target: Target, shared: Arc<(Mutex<WorkerState>, Condvar)>) {
    loop {
        let command = {
            let (state, wake) = &*shared;
            let Ok(mut state) = state.lock() else {
                return;
            };
            while state.pending.is_empty() && state.release_seq.is_none() && !state.stop_requested {
                let Ok(next) = wake.wait(state) else {
                    return;
                };
                state = next;
            }
            if let Some(report) = state.pending.pop_front() {
                WorkerCommand::Report(report)
            } else if let Some(seq) = state.release_seq.take() {
                WorkerCommand::Release(seq)
            } else {
                WorkerCommand::Stop
            }
        };

        let report = match command {
            WorkerCommand::Release(seq) => {
                if let Err(error) = run_release(&target, seq) {
                    crate::logging::debug(&format!("Herdr lifecycle release failed: {error}"));
                }
                let (state, wake) = &*shared;
                if let Ok(mut state) = state.lock() {
                    state.stopped = true;
                    wake.notify_all();
                }
                return;
            }
            WorkerCommand::Stop => {
                let (state, wake) = &*shared;
                if let Ok(mut state) = state.lock() {
                    state.stopped = true;
                    wake.notify_all();
                }
                return;
            }
            WorkerCommand::Report(report) => report,
        };

        if let Err(error) = run_report(&target, &report) {
            crate::logging::debug(&format!("Herdr lifecycle report failed: {error}"));
            thread::sleep(RETRY_DELAY);
            let (state, wake) = &*shared;
            if let Ok(mut state) = state.lock()
                && state.release_seq.is_none()
                && state.pending.is_empty()
            {
                state.pending.push_front(report);
                wake.notify_one();
            }
        }
    }
}

enum WorkerCommand {
    Report(Report),
    Release(u64),
    Stop,
}

fn initial_sequence() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn base_command(target: &Target) -> std::process::Command {
    let mut command = std::process::Command::new(&target.binary);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn run_report(target: &Target, report: &Report) -> std::io::Result<()> {
    let mut command = base_command(target);
    command.args([
        "pane",
        "report-agent",
        target.pane_id.as_str(),
        "--source",
        SOURCE,
        "--agent",
        AGENT,
        "--state",
        report.state.as_str(),
        "--agent-session-id",
        report.session_id.as_str(),
        "--seq",
        &report.seq.to_string(),
    ]);
    if let Some(message) = report.message.as_deref() {
        command.args(["--message", message]);
    }
    run_command_with_timeout(&mut command)
}

fn run_release(target: &Target, seq: u64) -> std::io::Result<()> {
    let mut command = base_command(target);
    command.args([
        "pane",
        "release-agent",
        target.pane_id.as_str(),
        "--source",
        SOURCE,
        "--agent",
        AGENT,
        "--seq",
        &seq.to_string(),
    ]);
    run_command_with_timeout(&mut command)
}

fn run_command_with_timeout(command: &mut std::process::Command) -> std::io::Result<()> {
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "Herdr exited with status {status}"
                )))
            };
        }
        if started.elapsed() >= COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Herdr lifecycle command timed out",
            ));
        }
        thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_environment_requires_enablement_and_pane_but_can_use_path_binary() {
        let enabled = OsString::from("1");
        assert!(
            Reporter::from_values(None, Some("herdr".into()), Some("w1:p1".into()))
                .shared
                .is_none()
        );
        assert!(
            Reporter::from_values(Some("0".into()), Some("herdr".into()), Some("w1:p1".into()))
                .shared
                .is_none()
        );
        assert!(
            Reporter::from_values(Some(enabled.clone()), None, Some("w1:p1".into()))
                .shared
                .is_some()
        );
        assert!(
            Reporter::from_values(Some(enabled), Some("herdr".into()), None)
                .shared
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn reports_ordered_state_session_and_release_through_public_cli() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().expect("temporary Herdr fixture");
        let binary = temp.path().join("fake herdr");
        let log = temp.path().join("calls.log");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n",
                crate::terminal_launch::sh_escape(&log.to_string_lossy())
            ),
        )
        .expect("write fake Herdr");
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();

        {
            let mut reporter = Reporter::from_values_with_seq(
                Some("1".into()),
                Some(binary.into_os_string()),
                Some("w7:p9".into()),
                100,
            );
            reporter.report_idle("ses one");
            reporter.report_working("ses one");
            reporter.report_working("ses one");
            reporter.report_blocked("ses one", "Approval needed");
            reporter.report_idle("ses two");
        }

        let lines: Vec<String> = std::fs::read_to_string(log)
            .expect("read fake Herdr calls")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            lines,
            [
                "pane report-agent w7:p9 --source custom:jcode --agent jcode --state idle --agent-session-id ses one --seq 101",
                "pane report-agent w7:p9 --source custom:jcode --agent jcode --state working --agent-session-id ses one --seq 102",
                "pane report-agent w7:p9 --source custom:jcode --agent jcode --state blocked --agent-session-id ses one --seq 103 --message Approval needed",
                "pane report-agent w7:p9 --source custom:jcode --agent jcode --state idle --agent-session-id ses two --seq 104",
                "pane release-agent w7:p9 --source custom:jcode --agent jcode --seq 105",
            ]
        );
    }
}
