// SPDX-License-Identifier: MPL-2.0
#[macro_use]
extern crate log;

pub mod error;
pub mod message;
pub mod process;

use self::{
	error::{Error, Result},
	process::{Process, ProcessCallbacks, ReturnFuture},
};
use rand::Rng;
use slotmap::{new_key_type, SlotMap};
use std::{borrow::Cow, os::unix::process::ExitStatusExt, process::Stdio, sync::Arc};
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

	pub async fn start(&self, process: Process) -> Result<ProcessKey> {
		let (return_tx, return_rx) = oneshot::channel();
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
		let cancel_token = self.cancel_token.child_token();
		let command = Command::new(&process.executable)
			.args(&process.args)
			.envs(process.env.clone())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.stdin(Stdio::null())
			.kill_on_drop(true)
			.spawn()
			.map_err(Error::Process)?;
		let callbacks = std::mem::take(&mut process.callbacks);
		let key = inner.processes.insert(ProcessData {
			process,
			restarts: 0,
			cancel_token: cancel_token.clone(),
		});
		// This adds futures into a queue and executes them in a separate task, in order
		// to both ensure execution of callbacks is in the same order the events are
		// received, and to avoid blocking the reception of events if a callback is slow
		// to return.
		let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();
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

	async fn restart_process(&self, process_key: ProcessKey) -> Result<Child> {
		let mut inner = self.inner.write().await;
		let restart_mode = inner.restart_mode;
		let process_data = inner
			.processes
			.get_mut(process_key)
			.ok_or(Error::InvalidProcess(process_key))?;

		// delay before restarting
		match restart_mode {
			RestartMode::ExponentialBackoff(backoff) => {
				let backoff = backoff.as_millis() as u64;
				let jittered_delay: u64 = rand::thread_rng().gen_range(0..backoff);
				let backoff = Duration::from_millis(
					2_u64.saturating_pow(process_data.restarts as u32) * jittered_delay,
				);
				tokio::time::sleep(backoff).await;
			}
			RestartMode::Delayed(backoff) => {
				tokio::time::sleep(backoff).await;
			}
			RestartMode::Instant => {}
		}

		let command = Command::new(&process_data.process.executable)
			.args(&process_data.process.args)
			.envs(process_data.process.env.clone())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.stdin(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(Error::Process)?;
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
		loop {
			tokio::select! {
				_ = cancel_token.cancelled() => {
					info!("process '{:?}' cancelled", key);
					command.kill().await.expect("failed to kill program");
					break;
				},
				Some(message) = stdin_rx.recv() => {
					if let Err(err) = command.stdin.as_mut()
						.expect("No stdin in process, even though we should be piping it")
						.write_all(&message).await {
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
						// drain stdin receiver before restarting
						while let Some(_) = stdin_rx.recv().await {}

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
	restarts: usize,
	cancel_token: CancellationToken,
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
