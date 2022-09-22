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
use slotmap::{new_key_type, SlotMap};
use std::{collections::VecDeque, process::Stdio, sync::Arc};
use tokio::{
	io::{AsyncBufReadExt, BufReader},
	process::{Child, Command},
	sync::{mpsc, oneshot, Mutex, RwLock},
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
		debug!(
			"starting process '{}{}{}'",
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
		let queue = Arc::new(Mutex::new(VecDeque::<ReturnFuture>::new()));
		let queue_clone = queue.clone();
		let queue_token = cancel_token.child_token();
		// tokio::spawn(async move {
		// 	loop {
        //         println!("busy looping");
		// 		let call_next = async {
		// 			let queued_callback = { queue_clone.lock().await.pop_front() };
		// 			if let Some(future) = queued_callback {
		// 				future.await;
		// 			};
		// 			tokio::task::yield_now().await
		// 		};
		// 		tokio::select! {
		// 			_ = call_next => continue,
		// 			_ = queue_token.cancelled() => break,
		// 		}
		// 	}
		// });
		if let Some(on_start) = &callbacks.on_start {
			queue
				.lock()
				.await
				.push_back(on_start(self.clone(), key, false));
		}
		tokio::spawn(self.clone().process_loop(
			key,
			cancel_token.child_token(),
			command,
			callbacks,
			queue,
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
		debug!(
			"restarted process '{}{}{}', now at {} restarts",
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
		queue: Arc<Mutex<VecDeque<ReturnFuture>>>,
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
					debug!("process '{:?}' cancelled", key);
					command.kill().await.expect("failed to kill program");
					break;
				},
				Ok(Some(stdout_line)) = stdout.next_line() => {
					if let Some(on_stdout) = &callbacks.on_stdout {
						tokio::spawn(on_stdout(self.clone(), key, stdout_line));
					}
				}
				Ok(Some(stderr_line)) = stderr.next_line() => {
					if let Some(on_stderr) = &callbacks.on_stderr {
						tokio::spawn(on_stderr(self.clone(), key, stderr_line));
					}
				}
				ret = command.wait() => {
					let ret = ret.unwrap();
					let is_restarting = {
						let inner = self.inner.read().await;
						let process = inner.processes.get(key).unwrap();
						!ret.success() && (inner.max_restarts > process.restarts)
					};
					if let Some(on_exit) = &callbacks.on_exit {
						tokio::spawn(on_exit(self.clone(), key, ret.code(), is_restarting));
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
									tokio::spawn(on_start(self.clone(), key, true));
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
