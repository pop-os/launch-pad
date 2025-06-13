// SPDX-License-Identifier: MPL-2.0
#[macro_use]
extern crate log;

pub mod error;
pub mod message;
pub mod process;
pub mod util;

use self::{
	error::{Error, Result},
	process::{Process, ProcessCallbacks, ReturnFuture},
};
use nix::{
	sys::signal::{self, Signal},
	unistd::Pid,
};
use rand::Rng;
use slotmap::{new_key_type, SlotMap};
use std::{
	borrow::Cow,
	os::{
		fd::{AsRawFd, OwnedFd},
		unix::process::ExitStatusExt,
	},
	process::Stdio,
	sync::Arc,
};
use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
	process::{Child, Command},
	sync::{mpsc, oneshot, RwLock},
	time::Duration,
};
use tokio_util::sync::CancellationToken;

new_key_type! { pub struct ProcessKey; }

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
		let mut inner = self.inner.write().await;
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

		let key = inner.processes.insert(ProcessData {
			process,
			pid: None,
			restarts: 0,
			cancel_token: cancel_token.clone(),
			cancel_timeout,
		});
		let process = inner.processes.get_mut(key).unwrap();

		let fd_list = if let Some(fds) = callbacks.fds.take() {
			fds()
		} else {
			Vec::new()
		};
		let raw_fds = fd_list.iter().map(|fd| fd.as_raw_fd()).collect::<Vec<_>>();

		let mut command = Command::new(&process.process.executable);
		let command = unsafe {
			command
				.args(&process.process.args)
				.envs(
					process
						.process
						.env
						.iter()
						.map(|(k, v)| (k.as_str(), v.as_str())),
				)
				.stdout(Stdio::piped())
				.stderr(Stdio::piped())
				.stdin(Stdio::piped())
				.kill_on_drop(true)
				.pre_exec(move || {
					for fd in &raw_fds {
						util::mark_as_not_cloexec(*fd)?;
					}
					Ok(())
				})
				.spawn()
				.map_err(Error::Process)?
		};
		drop(fd_list);
		process.pid = command.id();
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

	async fn restart_process(&self, process_key: ProcessKey) -> Result<Child> {
		let inner = self.inner.read().await;
		let restart_mode = inner.restart_mode;
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
				let backoff = backoff.as_millis() as u64;
				let jittered_delay: u64 = rand::thread_rng().gen_range(0..backoff);
				let backoff = Duration::from_millis(
					2_u64
						.saturating_pow(restarts as u32)
						.saturating_mul(jittered_delay),
				);
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
						util::mark_as_not_cloexec(*fd)?;
					}
					Ok(())
				})
				.spawn()
				.map_err(Error::Process)?
		};
		process_data.pid = command.id();
		drop(fd_list);

		process_data.restarts += 1;
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
		loop {
			tokio::select! {
				_ = cancel_token.cancelled() => {
					info!("process '{:?}' cancelled", key);
					let mut exit_code = None;
					if let Some(id) = command.id() {
						if let Err(err) = signal::kill(Pid::from_raw(id as i32), Signal::SIGTERM) {
							log::error!("Error sending SIGTERM: {err:?}");
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
					let ret = ret.unwrap();
					let is_restarting = {
						let inner = self.inner.read().await;
						let process = inner.processes.get(key).unwrap();
						if !ret.success() {
							let env_text = process.process.env_text();
							let exe_text = process.process.exe_text();
							let args_text = process.process.args_text();
							if let Some(signal) = ret.signal() {
								error!("process '{} {} {}' terminated with signal {}", env_text, exe_text, args_text, signal);
							} else if let Some(code) = ret.code() {
								error!("process '{} {} {}' failed with code {}", env_text, exe_text, args_text, code);
							}
						}
						!ret.success() && (inner.max_restarts > process.restarts)
					};
					if let Some(on_exit) = &callbacks.on_exit {
						// wait for this to complete before potentially restarting
						on_exit(self.clone(), key, ret.code(), is_restarting).await;
					}
					if is_restarting {
						info!("draining stdin receiver before restarting process");
						while let Ok(_) = stdin_rx.try_recv() {}

						match self.restart_process(key).await {
							Ok(new_command) =>  {
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
							Err(err) => {
								error!("failed to restart process '{:?}: {}", key, err);
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

struct ProcessData {
	process: Process,
	pid: Option<u32>,
	restarts: usize,
	cancel_token: CancellationToken,
	cancel_timeout: Option<Duration>,
}

struct ProcessManagerInner {
	restart_mode: RestartMode,
	max_restarts: usize,
	processes: SlotMap<ProcessKey, ProcessData>,
}

#[derive(Clone, Copy, Debug)]
pub enum RestartMode {
	Instant,
	Delayed(Duration),
	ExponentialBackoff(Duration),
}
