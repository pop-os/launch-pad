// SPDX-License-Identifier: MPL-2.0
#[macro_use]
extern crate log;

pub mod error;
pub mod liveness;
pub mod message;
pub mod process;
pub mod util;

use self::{
    error::{Error, Result},
    process::{Process, ProcessCallbacks, ReturnFuture},
};

use rand::Rng;
use slotmap::{SlotMap, new_key_type};
use std::{
    borrow::Cow,
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::process::ExitStatusExt,
    },
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{RwLock, mpsc, oneshot},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
use rustix::io_uring::Signal;
#[cfg(target_os = "linux")]
use rustix::process::{Pid, kill_process};

new_key_type! { pub struct ProcessKey; }

/// Default ceiling for [`RestartMode::ExponentialBackoff`].
///
/// Without a ceiling the delay doubles without bound: at 20 restarts it is
/// already over an hour, and past 30 it is measured in weeks, so a component
/// that crashes repeatedly becomes unrestartable in practice while the log
/// still cheerfully reports that a restart is pending.
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Default interval for the liveness check described in [`liveness`].
///
/// One small procfs read per process per tick, so the cost is negligible.
pub const DEFAULT_LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

/// How long a process must stay up before its restart counter is forgiven.
///
/// Without this the counter only ever grows, so a process that crashes rarely
/// but over a long session still ends up pinned at the backoff ceiling and
/// eventually exceeds `max_restarts` — even though it was healthy in between.
pub const RESTART_DECAY_UPTIME: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<RwLock<ProcessManagerInner>>,
    /// Transmitter for ProcessManager instances
    /// a Process will be sent to the main loop for spawning
    /// and a key will be sent back to the caller
    tx: mpsc::UnboundedSender<(Process, oneshot::Sender<Result<ProcessKey>>)>,
    cancel_token: CancellationToken,
}

impl ProcessManager {
    pub async fn new() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let inner = Arc::new(RwLock::new(ProcessManagerInner {
            restart_mode: RestartMode::Instant,
            max_restarts: 3,
            max_backoff: DEFAULT_MAX_BACKOFF,
            liveness_interval: Some(DEFAULT_LIVENESS_INTERVAL),
            processes: SlotMap::with_key(),
        }));
        let manager = ProcessManager {
            inner,
            tx,
            cancel_token: cancel.clone(),
        };
        let manager_clone = manager.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = rx.recv() => match msg {
                        Some((process, return_tx)) => {
                            return_tx
                                .send(manager_clone.start_process(process).await)
                                .unwrap();
                        }
                        None => break,
                    }
                }
            }
        });
        manager
    }

    /// Starts a process with the given configuration. implicitly calls
    /// `start_process`
    pub async fn start(&self, process: Process) -> Result<ProcessKey> {
        let (return_tx, return_rx) = oneshot::channel();
        // send a process to spawn and a transmitter to the loop above
        // and wait for the key to be returned
        let _ = self.tx.send((process, return_tx));
        return_rx.await?
    }

    /// Returns the current restart mode.
    pub async fn restart_mode(&self) -> RestartMode {
        self.inner.read().await.restart_mode
    }

    /// Sets the restart mode.
    pub async fn set_restart_mode(&self, restart_mode: RestartMode) {
        self.inner.write().await.restart_mode = restart_mode;
    }

    /// Returns the maximum amount of times a process can be restarted before
    /// giving up.
    pub async fn max_restarts(&self) -> usize {
        self.inner.read().await.max_restarts
    }

    /// Sets the maximum amount of times a process can be restarted before
    /// giving up.
    pub async fn set_max_restarts(&self, max_restarts: usize) {
        self.inner.write().await.max_restarts = max_restarts;
    }

    /// Returns the ceiling applied to [`RestartMode::ExponentialBackoff`].
    pub async fn max_backoff(&self) -> Duration {
        self.inner.read().await.max_backoff
    }

    /// Sets the ceiling applied to [`RestartMode::ExponentialBackoff`].
    ///
    /// Defaults to [`DEFAULT_MAX_BACKOFF`]. The backoff doubles per restart up
    /// to this value and no further.
    pub async fn set_max_backoff(&self, max_backoff: Duration) {
        self.inner.write().await.max_backoff = max_backoff;
    }

    /// Returns how often supervised processes are checked for a death that
    /// `waitpid` cannot report, or `None` if the check is disabled.
    pub async fn liveness_interval(&self) -> Option<Duration> {
        self.inner.read().await.liveness_interval
    }

    /// Sets how often supervised processes are checked for a death that
    /// `waitpid` cannot report — see [`liveness`]. `None` disables the check.
    ///
    /// Defaults to [`DEFAULT_LIVENESS_INTERVAL`]. When the check trips, the
    /// surviving threads are killed so the pending `wait()` can complete and
    /// the process is reported through the normal exit path.
    ///
    /// Leaving this enabled is strongly recommended: the failure it detects is
    /// otherwise permanent and silent.
    pub async fn set_liveness_interval(&self, interval: Option<Duration>) {
        self.inner.write().await.liveness_interval = interval;
    }

    /// Returns whether the process manager has been stopped or not.
    /// If the process manager has been stopped, no new processes can be
    /// started.
    pub fn is_stopped(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    /// Stops the process manager, halting all processes and preventing new
    /// processes from being started.
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }

    /// Stops a single process.
    pub async fn stop_process(&self, key: ProcessKey) -> Result<()> {
        let inner = self.inner.read().await;
        let process = inner.processes.get(key).ok_or(Error::NonExistantProcess)?;
        process.cancel_token.cancel();
        Ok(())
    }

    /// Send a message to a process over stdin
    pub async fn send_message(&self, key: ProcessKey, message: Cow<'static, [u8]>) -> Result<()> {
        let inner = self.inner.read().await;
        let process = inner.processes.get(key).ok_or(Error::NonExistantProcess)?;
        process.process.stdin_tx.send(message).await?;
        Ok(())
    }

    pub async fn start_process(&self, mut process: Process) -> Result<ProcessKey> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }

        let Some(rx) = process.stdin_rx.take() else {
            return Err(Error::MissingStdinReceiver);
        };
        info!(
            "starting process '{} {} {}'",
            process.env_text(),
            process.exe_text(),
            process.args_text()
        );
        let mut callbacks = std::mem::take(&mut process.callbacks);
        let cancel_timeout = process.cancel_timeout;
        let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();

        let cancel_token = self.cancel_token.child_token();

        let fd_list = if let Some(fds) = callbacks.fds.take() {
            fds()
        } else {
            Vec::new()
        };
        let raw_fds = fd_list.iter().map(|fd| fd.as_raw_fd()).collect::<Vec<_>>();

        let mut command = Command::new(&process.executable);

        command
            .args(&process.args)
            .envs(process.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .kill_on_drop(true);

        let key = self.inner.write().await.processes.insert(ProcessData {
            process,
            pid: None,
            restarts: 0,
            started_at: std::time::Instant::now(),
            cancel_token: cancel_token.clone(),
            cancel_timeout,
        });

        let command = unsafe {
            command
                .pre_exec(move || {
                    for fd in &raw_fds {
                        util::mark_as_not_cloexec(BorrowedFd::borrow_raw(*fd))?;
                    }
                    Ok(())
                })
                .spawn()
                .map_err(Error::Process)?
        };
        drop(fd_list);
        self.inner.write().await.processes.get_mut(key).unwrap().pid = command.id();
        // This adds futures into a queue and executes them in a separate task, in order
        // to both ensure execution of callbacks is in the same order the events are
        // received, and to avoid blocking the reception of events if a callback is slow
        // to return.
        tokio::spawn(async move {
            while let Some(f) = callback_rx.recv().await {
                f.await
            }
        });
        if let Some(on_start) = &callbacks.on_start {
            let _ = callback_tx.send(on_start(self.clone(), key, false));
        }
        tokio::spawn(self.clone().process_loop(
            key,
            cancel_token.child_token(),
            command,
            callbacks,
            callback_tx,
            rx,
        ));
        Ok(key)
    }

    /// Just gives you the exe, along with the pid, of a managed process
    pub async fn get_exe_and_pid(&self, key: ProcessKey) -> Result<(String, Option<u32>)> {
        let inner = self.inner.read().await;
        let pdata = inner
            .processes
            .get(key)
            .ok_or(error::Error::NonExistantProcess)?;
        Ok((pdata.process.executable.clone(), pdata.pid))
    }

    /// Get the pid of a managed process
    pub async fn get_pid(&self, key: ProcessKey) -> Result<Option<u32>> {
        let inner = self.inner.read().await;
        Ok(inner
            .processes
            .get(key)
            .ok_or(error::Error::NonExistantProcess)?
            .pid)
    }

    #[allow(rustdoc::private_intra_doc_links)]
    /// Delay before the next restart attempt: capped exponential growth with
    /// full jitter.
    ///
    /// The delay doubles per restart up to `max_backoff`, then stops growing.
    /// A uniform sample between `base` and that ceiling spreads simultaneous
    /// restarts out instead of synchronising them.
    ///
    /// Every step saturates rather than overflowing, and the sampled range is
    /// always non-empty, so no combination of arguments can panic — including a
    /// zero `base`, which the previous `random_range(0..0)` could not survive.
    fn exponential_backoff(base: Duration, max_backoff: Duration, restarts: usize) -> Duration {
        let base_ms = base.as_millis().min(u64::MAX as u128) as u64;
        let ceiling_ms = max_backoff.as_millis().min(u64::MAX as u128) as u64;

        let grown_ms = base_ms
            .saturating_mul(2_u64.saturating_pow(restarts.min(u32::MAX as usize) as u32))
            .min(ceiling_ms);

        // Full jitter between the base delay and the grown ceiling. `low`
        // cannot exceed `high`, so the inclusive range is never empty.
        let low = base_ms.min(grown_ms);
        let delay_ms = if low == grown_ms {
            low
        } else {
            rand::rng().random_range(low..=grown_ms)
        };

        Duration::from_millis(delay_ms)
    }

    async fn restart_process(&self, process_key: ProcessKey) -> Result<Child> {
        let inner = self.inner.read().await;
        let restart_mode = inner.restart_mode;
        let max_backoff = inner.max_backoff;
        let process_data = inner
            .processes
            .get(process_key)
            .ok_or(Error::InvalidProcess(process_key))?;
        let restarts = process_data.restarts;
        let executable = process_data.process.executable.clone();
        drop(inner);

        // delay before restarting
        match restart_mode {
            RestartMode::ExponentialBackoff(backoff) => {
                let backoff = Self::exponential_backoff(backoff, max_backoff, restarts);
                info!(
                    "sleeping for {}ms before restarting process {} (restart {})",
                    backoff.as_millis(),
                    executable,
                    restarts
                );

                tokio::time::sleep(backoff).await;
            }
            RestartMode::Delayed(backoff) => {
                info!(
                    "sleeping for {}ms before restarting process {} (restart {})",
                    backoff.as_millis(),
                    executable,
                    restarts
                );
                tokio::time::sleep(backoff).await;
            }
            RestartMode::Instant => {}
        }
        let mut inner = self.inner.write().await;
        let process_data = inner
            .processes
            .get_mut(process_key)
            .ok_or(Error::InvalidProcess(process_key))?;
        let mut fd_callback = process_data.process.callbacks.fds.take();
        let fd_list = if let Some(fds) = fd_callback.take() {
            fds()
        } else {
            Vec::new()
        };
        let raw_fds = fd_list.iter().map(|fd| fd.as_raw_fd()).collect::<Vec<_>>();

        // Count the attempt before it can fail. Incrementing only on success
        // would let a spawn that keeps failing retry forever at the same delay,
        // since both the backoff and `max_restarts` are driven by this counter.
        process_data.restarts += 1;

        let command = unsafe {
            Command::new(&process_data.process.executable)
                .args(&process_data.process.args)
                .envs(process_data.process.env.clone())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::piped())
                .kill_on_drop(true)
                .pre_exec(move || {
                    for fd in &raw_fds {
                        util::mark_as_not_cloexec(BorrowedFd::borrow_raw(*fd))?;
                    }
                    Ok(())
                })
                .spawn()
                .map_err(Error::Process)?
        };
        process_data.pid = command.id();
        process_data.started_at = std::time::Instant::now();
        drop(fd_list);

        info!(
            "restarted process '{} {} {}', now at {} restarts",
            process_data.process.env_text(),
            process_data.process.exe_text(),
            process_data.process.args_text(),
            process_data.restarts
        );
        Ok(command)
    }

    async fn process_loop(
        self,
        key: ProcessKey,
        cancel_token: CancellationToken,
        mut command: Child,
        callbacks: ProcessCallbacks,
        callback_tx: mpsc::UnboundedSender<ReturnFuture>,
        mut stdin_rx: mpsc::Receiver<Cow<'static, [u8]>>,
    ) {
        let (mut stdout, mut stderr) = match (command.stdout.take(), command.stderr.take()) {
            (Some(stdout), Some(stderr)) => (
                BufReader::new(stdout).lines(),
                BufReader::new(stderr).lines(),
            ),
            (Some(_), None) => panic!("no stderr in process, even though we should be piping it"),
            (None, Some(_)) => panic!("no stdout in process, even though we should be piping it"),
            (None, None) => {
                panic!("no stdout or stderr in process, even though we should be piping it")
            }
        };
        let mut stdin = command
            .stdin
            .take()
            .expect("No stdin in process, even though we should be piping it");

        // A `tokio::time::Interval` rather than a `sleep` per iteration: an
        // interval keeps its own schedule, so a process that produces a steady
        // stream of output cannot starve the check by continually winning the
        // select and resetting a fresh timer.
        let liveness_interval = self.inner.read().await.liveness_interval;
        let mut liveness = liveness_interval.map(|period| {
            let mut interval =
                tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            // A missed tick means the loop was busy, not that we owe a burst of
            // catch-up health checks.
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval
        });
        // Only fire once per pid: if a kill somehow leaves the process in place,
        // this must not become a kill loop.
        let mut liveness_tripped_for: Option<u32> = None;

        loop {
            tokio::select! {
                // Catch the one death `wait()` below can never report: the
                // thread-group leader gone while its siblings run on. Killing the
                // survivors lets that pending `wait()` complete, so the exit is
                // reported through the normal path with no second mechanism.
                Some(_) = async {
                    match liveness.as_mut() {
                        Some(interval) => Some(interval.tick().await),
                        None => None,
                    }
                } => {
                    if let Some(id) = command.id() {
                        if liveness_tripped_for != Some(id) {
                            if let Some(threads) = liveness::stuck_zombie_leader(id) {
                                error!(
                                    "process '{:?}' (pid {}) has exited but {} of its threads are \
                                     still alive, so it can never be reaped - killing the \
                                     survivors so supervision can continue",
                                    key,
                                    id,
                                    threads - 1
                                );
                                liveness_tripped_for = Some(id);

                                #[cfg(target_os = "linux")]
                                if let Some(pid) = Pid::from_raw(id as i32) {
                                    if let Err(err) = kill_process(pid, Signal::KILL) {
                                        error!("failed to kill wedged process: {err:?}");
                                    }
                                }

                                #[cfg(not(target_os = "linux"))]
                                if unsafe { libc::kill(id as i32, libc::SIGKILL) == -1 } {
                                    error!(
                                        "failed to kill wedged process: {:?}",
                                        std::io::Error::last_os_error()
                                    );
                                }
                            }
                        }
                    }
                }
                _ = cancel_token.cancelled() => {
                    info!("process '{:?}' cancelled", key);
                    let mut exit_code = None;
                    if let Some(id) = command.id() {
                        #[cfg(target_os = "linux")]
                        if let Some(pid) = Pid::from_raw(id as i32) {
                            if let Err(err) = kill_process(pid, Signal::TERM) {
                                log::error!("Error sending SIGTERM: {err:?}");
                            }
                        }

                        #[cfg(not(target_os = "linux"))]
                        if unsafe { libc::kill(id as i32, libc::SIGTERM) == -1 } {
                            log::error!("Error sending SIGTERM: {:?}", std::io::Error::last_os_error());
                        }

                        if let Some(t) = {
                            let inner = self.inner.read().await;
                            inner.processes.get(key).and_then(|p| p.cancel_timeout)
                        } {
                            match tokio::time::timeout(t, command.wait()).await {
                                Ok(res) => {
                                    match res {
                                        Ok(status) => {
                                            exit_code = status.code();
                                        },
                                        Err(err) => {
                                            log::error!("Failed to stop program gracefully. {err:?}");
                                        },
                                    }
                                }
                                Err(_) => {
                                    log::error!("Failed to stop program gracefully before cancel timeout.");
                                }
                            };
                        } else {
                            match command.wait().await {
                                Ok(status) => {
                                    exit_code = status.code();
                                },
                                Err(err) => {
                                    log::error!("Failed to stop program gracefully. {err:?}");
                                },
                            }						}

                    } else {
                        log::error!("Failed to stop program gracefully. Missing pid.");
                    }

                    if exit_code.is_none() {
                        if let Err(err) = command.kill().await {
                            log::error!("Failed to kill program. {err:?}");
                        };
                        exit_code = Some(137);
                    }

                    if let Some(on_exit) = &callbacks.on_exit {
                        // wait for this to complete before potentially restarting
                        on_exit(self.clone(), key, exit_code, false).await;
                    }
                    break;
                },
                Some(message) = stdin_rx.recv() => {
                    if let Err(err) =
                        stdin.write_all(&message).await {
                        error!("failed to write to stdin of process '{:?}': {}", key, err);
                    }
                }
                Ok(Some(stdout_line)) = stdout.next_line() => {
                    if let Some(on_stdout) = &callbacks.on_stdout {
                        let _ = callback_tx.send(on_stdout(self.clone(), key, stdout_line));
                    }
                }
                Ok(Some(stderr_line)) = stderr.next_line() => {
                    if let Some(on_stderr) = &callbacks.on_stderr {
                        let _ = callback_tx.send(on_stderr(self.clone(), key, stderr_line));
                    }
                }
                ret = command.wait() => {
                    // A failure here means the child's status is unknowable, not
                    // that it is still alive. Unwrapping would panic this task
                    // and end supervision for the process with no notification
                    // to the consumer, so report it as an exit of unknown cause.
                    let ret = match ret {
                        Ok(ret) => Some(ret),
                        Err(err) => {
                            error!("failed to wait on process '{:?}', treating it as exited: {}", key, err);
                            None
                        }
                    };
                    let success = ret.as_ref().is_some_and(|r| r.success());
                    let exit_code = ret.as_ref().and_then(|r| r.code());

                    let is_restarting = {
                        let mut inner = self.inner.write().await;
                        let max_restarts = inner.max_restarts;
                        // The entry is never removed from the slotmap, but read
                        // defensively rather than unwrapping: a panic here would
                        // silently end supervision.
                        let Some(process) = inner.processes.get_mut(key) else {
                            error!("process '{:?}' vanished from the manager, ending supervision", key);
                            break;
                        };
                        if !success {
                            let env_text = process.process.env_text();
                            let exe_text = process.process.exe_text();
                            let args_text = process.process.args_text();
                            if let Some(signal) = ret.as_ref().and_then(|r| r.signal()) {
                                error!("process '{} {} {}' terminated with signal {}", env_text, exe_text, args_text, signal);
                            } else if let Some(code) = exit_code {
                                error!("process '{} {} {}' failed with code {}", env_text, exe_text, args_text, code);
                            }
                        }
                        // Forgive earlier restarts once a process has proven it
                        // can stay up, so occasional crashes spread over a long
                        // session neither pin the backoff at its ceiling nor
                        // exhaust `max_restarts`.
                        if process.started_at.elapsed() >= RESTART_DECAY_UPTIME && process.restarts > 0 {
                            info!(
                                "process '{}' was up for {}s, forgiving its {} earlier restart(s)",
                                process.process.exe_text(),
                                process.started_at.elapsed().as_secs(),
                                process.restarts
                            );
                            process.restarts = 0;
                        }
                        !success && (max_restarts > process.restarts)
                    };
                    if let Some(on_exit) = &callbacks.on_exit {
                        // wait for this to complete before potentially restarting
                        on_exit(self.clone(), key, exit_code, is_restarting).await;
                    }
                    if is_restarting {
                        info!("draining stdin receiver before restarting process");
                        while let Ok(_) = stdin_rx.try_recv() {}

                        // Keep retrying while restarts remain. A spawn failure is
                        // often transient — ENOMEM under the same pressure that
                        // killed the child, or ENOENT while a package upgrade
                        // swaps the binary — and each attempt backs off further
                        // because `restart_process` counts it.
                        let restarted = loop {
                            match self.restart_process(key).await {
                                Ok(new_command) => break Some(new_command),
                                Err(err) => {
                                    error!("failed to restart process '{:?}': {}", key, err);
                                    let exhausted = {
                                        let inner = self.inner.read().await;
                                        inner
                                            .processes
                                            .get(key)
                                            .is_none_or(|p| p.restarts >= inner.max_restarts)
                                    };
                                    if exhausted {
                                        break None;
                                    }
                                }
                            }
                        };

                        match restarted {
                            Some(new_command) =>  {
                                command = new_command;
                                (stdout, stderr) = match (command.stdout.take(), command.stderr.take()) {
                                    (Some(stdout), Some(stderr)) => (
                                        BufReader::new(stdout).lines(),
                                        BufReader::new(stderr).lines(),
                                    ),
                                    (Some(_), None) => panic!("no stderr in process, even though we should be piping it"),
                                    (None, Some(_)) => panic!("no stdout in process, even though we should be piping it"),
                                    (None, None) => {
                                        panic!("no stdout or stderr in process, even though we should be piping it")
                                    }
                                };
                                stdin = command
                                    .stdin
                                    .take()
                                    .expect("No stdin in process, even though we should be piping it");
                                if let Some(on_start) = &callbacks.on_start {
                                    let _ = callback_tx.send(on_start(self.clone(), key, true));
                                }
                                continue;
                            }
                            None => {
                                // The consumer was told a restart was coming and
                                // may have acted on it. Correct the record so it
                                // can react, rather than leaving it to believe a
                                // restart is still pending.
                                error!("giving up on restarting process '{:?}'", key);
                                if let Some(on_exit) = &callbacks.on_exit {
                                    on_exit(self.clone(), key, exit_code, false).await;
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    /// update the args of a managed process
    /// This will reset previous args if they are not set again
    /// changes will be applied after the process restarts
    pub async fn update_process_args(&mut self, key: &ProcessKey, args: Vec<String>) -> Result<()> {
        let mut r = self.inner.write().await;
        if let Some(pdata) = r.processes.get_mut(*key) {
            pdata.process.args = args;
            Ok(())
        } else {
            Err(Error::NonExistantProcess)
        }
    }

    /// update the env of a managed process
    /// changes will be applied after the process restarts
    pub async fn update_process_env(
        &mut self,
        key: &ProcessKey,
        env: impl IntoIterator<Item = (impl ToString, impl ToString)>,
    ) -> Result<()> {
        let mut r = self.inner.write().await;
        if let Some(pdata) = r.processes.get_mut(*key) {
            let mut new_env: Vec<(_, _)> = env
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            pdata
                .process
                .env
                .retain(|(k, _)| !new_env.iter().any(|(k_new, _)| k == k_new));
            pdata.process.env.append(&mut new_env);
            Ok(())
        } else {
            Err(Error::NonExistantProcess)
        }
    }

    pub async fn update_process_fds<F>(&mut self, key: &ProcessKey, f: F) -> Result<()>
    where
        F: FnOnce() -> Vec<OwnedFd> + Send + Sync + 'static,
    {
        let mut r = self.inner.write().await;
        if let Some(pdata) = r.processes.get_mut(*key) {
            pdata.process.callbacks.fds = Some(Box::new(f));
            Ok(())
        } else {
            Err(Error::NonExistantProcess)
        }
    }

    /// update the env of a managed process
    /// changes will be applied after the process restarts
    pub async fn clear_process_env(&mut self, key: &ProcessKey) -> Result<()> {
        let mut r = self.inner.write().await;
        if let Some(pdata) = r.processes.get_mut(*key) {
            pdata.process.env.clear();
            Ok(())
        } else {
            Err(Error::NonExistantProcess)
        }
    }

    // TODO methods for modifying other process data
}

#[cfg(test)]
mod backoff_tests {
    use super::{DEFAULT_MAX_BACKOFF, ProcessManager};
    use tokio::time::Duration;

    fn delay(base_ms: u64, cap: Duration, restarts: usize) -> Duration {
        ProcessManager::exponential_backoff(Duration::from_millis(base_ms), cap, restarts)
    }

    /// The regression that motivated this: with the old
    /// `2^restarts * U(0, base)` formula and cosmic-session's 10ms base, restart
    /// 21 slept 12582912ms — 3h29m — and by restart 30 it was months.
    #[test]
    fn is_capped() {
        for restarts in [0, 1, 10, 21, 30, 64, 1000, usize::MAX] {
            let d = delay(10, DEFAULT_MAX_BACKOFF, restarts);
            assert!(
                d <= DEFAULT_MAX_BACKOFF,
                "restart {restarts} produced {d:?}, over the {DEFAULT_MAX_BACKOFF:?} cap"
            );
        }
    }

    /// Saturating arithmetic throughout, so absurd inputs clamp instead of
    /// overflowing or panicking.
    #[test]
    fn survives_extreme_inputs() {
        let _ = delay(u64::MAX, Duration::from_millis(u64::MAX), usize::MAX);
        let _ = delay(0, Duration::ZERO, 0);
        let _ = delay(1, Duration::ZERO, 99);
    }

    /// `random_range(0..0)` panics; a zero base must not.
    #[test]
    fn zero_base_does_not_panic() {
        assert_eq!(delay(0, DEFAULT_MAX_BACKOFF, 5), Duration::ZERO);
    }

    /// Never below the base delay, so a restart storm cannot become a hot loop.
    #[test]
    fn never_below_base() {
        for restarts in 0..40 {
            assert!(delay(50, DEFAULT_MAX_BACKOFF, restarts) >= Duration::from_millis(50));
        }
    }

    /// Growth actually happens between the base and the ceiling.
    #[test]
    fn grows_with_restarts() {
        let cap = Duration::from_secs(600);
        // Jittered, so compare the ceilings a large sample can reach.
        let early = (0..200).map(|_| delay(10, cap, 1)).max().unwrap();
        let late = (0..200).map(|_| delay(10, cap, 12)).max().unwrap();
        assert!(late > early, "expected growth: {early:?} -> {late:?}");
    }
}

struct ProcessData {
    process: Process,
    pid: Option<u32>,
    restarts: usize,
    /// When the current incarnation was spawned, used to forgive the restart
    /// counter once a process has proven stable — see [`RESTART_DECAY_UPTIME`].
    started_at: std::time::Instant,
    cancel_token: CancellationToken,
    cancel_timeout: Option<Duration>,
}

struct ProcessManagerInner {
    restart_mode: RestartMode,
    max_restarts: usize,
    max_backoff: Duration,
    liveness_interval: Option<Duration>,
    processes: SlotMap<ProcessKey, ProcessData>,
}

#[derive(Clone, Copy, Debug)]
pub enum RestartMode {
    Instant,
    Delayed(Duration),
    ExponentialBackoff(Duration),
}
