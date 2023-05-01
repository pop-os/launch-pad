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
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use slotmap::{new_key_type, SlotMap};
use std::{future::Future, os::unix::process::ExitStatusExt, pin::Pin, process::Stdio, sync::Arc};
use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
	process::{Child, Command},
	sync::{mpsc, oneshot, RwLock},
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

	async fn gen_and_send_psk<F, A>(
		&self,
		on_psk: F,
		command: &mut Child,
		key: ProcessKey,
		callback_tx: mpsc::UnboundedSender<Pin<Box<dyn Future<Output = ()> + Send + Sync>>>,
	) -> Result<()>
	where
		F: Fn(ProcessManager, ProcessKey, Vec<u8>) -> A + Unpin + Send + Sync,
		A: Future<Output = ()> + Send + Sync + 'static,
	{
		let mut hasher = Sha256::new();
		let stdin = command.stdin.as_mut().unwrap();
		// Drop key asap.
		{
			let mut key = [0u8; 64];
			OsRng.fill_bytes(&mut key);
			stdin.write_all(&key).await.map_err(Error::Io)?;
			hasher.update(key);
		}
		callback_tx
			.send(Box::pin(on_psk(
				self.clone(),
				key,
				hasher.finalize()[..].to_owned(),
			)))
			.map_err(|_| Error::SendMessage)?;
		Ok(())
	}

	pub async fn start(&self, process: Process) -> Result<ProcessKey> {
		let (return_tx, return_rx) = oneshot::channel();
		let _ = self.tx.send((process, return_tx));
		return_rx.await?
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

	pub async fn start_process(&self, mut process: Process) -> Result<ProcessKey> {
		if self.is_stopped() {
			return Err(Error::Stopped);
		}
		let mut inner = self.inner.write().await;
		info!(
			"starting process '{} {} {}'",
			process.env_text(),
			process.exe_text(),
			process.args_text()
		);
		let cancel_token = self.cancel_token.child_token();
		let callbacks = std::mem::take(&mut process.callbacks);
		let stdin = match &callbacks.on_psk {
			Some(_) => Stdio::piped(),
			None => Stdio::null(),
		};
		let mut command = Command::new(&process.executable)
			.args(&process.args)
			.envs(process.env.clone())
			.stdin(stdin)
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(Error::Process)?;

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
		if let Some(on_psk) = &callbacks.on_psk {
			let _ = self
				.gen_and_send_psk(on_psk, &mut command, key, callback_tx.clone())
				.await;
		}

		tokio::spawn(self.clone().process_loop(
			key,
			cancel_token.child_token(),
			command,
			callbacks,
			callback_tx,
		));
		Ok(key)
	}

	async fn restart_process(&self, process_key: ProcessKey) -> Result<Child> {
		let mut inner = self.inner.write().await;
		let process_data = inner
			.processes
			.get_mut(process_key)
			.ok_or(Error::InvalidProcess(process_key))?;
		let command = Command::new(&process_data.process.executable)
			.args(&process_data.process.args)
			.envs(process_data.process.env.clone())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.stdin(Stdio::null())
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
								if let Some(on_psk) = &callbacks.on_psk {
									let _ = self.gen_and_send_psk(on_psk, &mut command, key, callback_tx.clone()).await;
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
	max_restarts: usize,
	processes: SlotMap<ProcessKey, ProcessData>,
}
