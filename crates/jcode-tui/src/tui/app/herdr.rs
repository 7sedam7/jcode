use super::App;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const PERMISSION_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(super) struct Reporter {
    inner: crate::herdr::Reporter,
    session_id: Option<String>,
    permission_message: Option<String>,
    permission_poller: PermissionPoller,
}

impl Reporter {
    pub(super) fn from_env() -> Self {
        let inner = crate::herdr::Reporter::from_env();
        let permission_poller = PermissionPoller::new(inner.is_enabled());
        Self {
            inner,
            session_id: None,
            permission_message: None,
            permission_poller,
        }
    }

    fn sync(&mut self, session_id: Option<&str>, is_processing: bool) {
        let Some(session_id) = session_id else {
            return;
        };
        if self.session_id.as_deref() != Some(session_id) {
            self.session_id = Some(session_id.to_string());
            self.permission_message = None;
            self.permission_poller.track(session_id);
        }
        if let Some(message) = self.permission_poller.latest(session_id) {
            self.permission_message = message;
        }
        if is_processing {
            self.inner.report_working(session_id);
            return;
        }

        if let Some(message) = self.permission_message.as_deref() {
            self.inner.report_blocked(session_id, message);
        } else {
            self.inner.report_idle(session_id);
        }
    }
}

enum PollCommand {
    Track(String),
    Stop,
}

struct PermissionPoller {
    command_tx: Option<mpsc::Sender<PollCommand>>,
    result_rx: mpsc::Receiver<(String, Option<String>)>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PermissionPoller {
    fn new(enabled: bool) -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        if !enabled {
            return Self {
                command_tx: None,
                result_rx,
                worker: None,
            };
        }

        let (command_tx, command_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("jcode-herdr-permissions".to_string())
            .spawn(move || {
                let mut session_id: Option<String> = None;
                loop {
                    match command_rx.recv_timeout(PERMISSION_POLL_INTERVAL) {
                        Ok(PollCommand::Track(next)) => session_id = Some(next),
                        Ok(PollCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if let Some(session_id) = session_id.as_deref() {
                        let message = crate::safety::pending_permission_message(session_id);
                        if result_tx.send((session_id.to_string(), message)).is_err() {
                            break;
                        }
                    }
                }
            })
            .ok();

        Self {
            command_tx: worker.as_ref().map(|_| command_tx),
            result_rx,
            worker,
        }
    }

    fn track(&self, session_id: &str) {
        if let Some(sender) = self.command_tx.as_ref() {
            let _ = sender.send(PollCommand::Track(session_id.to_string()));
        }
    }

    fn latest(&self, session_id: &str) -> Option<Option<String>> {
        let mut latest = None;
        while let Ok((reported_session_id, message)) = self.result_rx.try_recv() {
            if reported_session_id == session_id {
                latest = Some(message);
            }
        }
        latest
    }
}

impl Drop for PermissionPoller {
    fn drop(&mut self) {
        if let Some(sender) = self.command_tx.take() {
            let _ = sender.send(PollCommand::Stop);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn sync(reporter: &mut Reporter, session_id: Option<&str>, is_processing: bool) {
    let Some(session_id) = session_id else {
        return;
    };
    reporter.sync(Some(session_id), is_processing);
}

pub(super) fn sync_local(reporter: &mut Reporter, app: &App) {
    sync(reporter, Some(&app.session.id), app.is_processing);
}

pub(super) fn sync_remote(reporter: &mut Reporter, app: &App) {
    let Some(session_id) = app.remote_session_id.as_deref() else {
        return;
    };
    if let Some(message) = app.herdr_blocked_message.as_deref() {
        reporter.inner.report_blocked(session_id, message);
    } else {
        sync(reporter, Some(session_id), app.is_processing);
    }
}
