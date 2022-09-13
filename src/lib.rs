// SPDX-License-Identifier: MPL-2.0
pub mod message;
pub mod process;

use self::process::{Process, ProcessCallbacks, ReturnFuture};
use slotmap::{new_key_type, SlotMap};
use std::{collections::VecDeque, process::Stdio, sync::Arc};
use tokio::{
	io::{AsyncBufReadExt, BufReader},
	process::{Child, Command},
	sync::{mpsc, oneshot, Mutex, RwLock},
};

new_key_type! { pub struct ProcessKey; }

#[derive(Clone)]
pub struct ProcessManager {
	inner: Arc<RwLock<ProcessManagerInner>>,
	tx: mpsc::UnboundedSender<(Process, oneshot::Sender<ProcessKey>)>,
}

impl ProcessManager {
	pub async fn new() -> Self {
		let (tx, mut rx) = mpsc::unbounded_channel();
		let inner = Arc::new(RwLock::new(ProcessManagerInner {
			max_restarts: 3,
			processes: SlotMap::with_key(),
		}));
		let manager = ProcessManager { inner, tx };
		let manager_clone = manager.clone();
		tokio::spawn(async move {
			loop {
				while let Some((process, return_tx)) = rx.recv().await {
					return_tx
						.send(manager_clone.start_process(process).await)
						.unwrap();
				}
			}
		});
		manager
	}

	pub async fn start(&self, process: Process) -> ProcessKey {
		let (return_tx, return_rx) = oneshot::channel();
		let _ = self.tx.send((process, return_tx));
		return_rx.await.unwrap()
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

	pub async fn start_process(&self, mut process: Process) -> ProcessKey {
		let mut inner = self.inner.write().await;
		let command = Command::new(&process.executable)
			.args(&process.args)
			.envs(process.env.clone())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.stdin(Stdio::null())
			.kill_on_drop(true)
			.spawn()
			.expect("failed to start process");
		let callbacks = std::mem::take(&mut process.callbacks);
		let key = inner.processes.insert(ProcessData {
			process,
			restarts: 0,
		});
		// This adds futures into a queue and executes them in a separate task, in order
		// to both ensure execution of callbacks is in the same order the events are
		// received, and to avoid blocking the reception of events if a callback is slow
		// to return.
		let queue = Arc::new(Mutex::new(VecDeque::<ReturnFuture>::new()));
		let queue_clone = queue.clone();
		tokio::spawn(async move {
			loop {
				let queued_callback = { queue_clone.lock().await.pop_front() };
				if let Some(future) = queued_callback {
					future.await;
				};
				tokio::task::yield_now().await;
			}
		});
		if let Some(on_start) = &callbacks.on_start {
			queue.lock().await.push_back(on_start(false));
		}
		tokio::spawn(self.clone().process_loop(key, command, callbacks, queue));
		key
	}

	async fn restart_process(&self, process_key: ProcessKey) -> Option<Child> {
		let mut inner = self.inner.write().await;
		let process = inner.processes.get_mut(process_key).unwrap();
		let command = Command::new(&process.process.executable)
			.args(&process.process.args)
			.envs(process.process.env.clone())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.stdin(Stdio::null())
			.kill_on_drop(true)
			.spawn()
			.expect("failed to restart process");
		process.restarts += 1;
		Some(command)
	}

	async fn process_loop(
		self,
		key: ProcessKey,
		mut command: Child,
		callbacks: ProcessCallbacks,
		queue: Arc<Mutex<VecDeque<ReturnFuture>>>,
	) {
		let mut stdout = BufReader::new(command.stdout.take().unwrap()).lines();
		let mut stderr = BufReader::new(command.stderr.take().unwrap()).lines();
		loop {
			tokio::select! {
				Ok(Some(stdout_line)) = stdout.next_line() => {
					if let Some(on_stdout) = &callbacks.on_stdout {
						queue.lock().await.push_back(on_stdout(stdout_line));
					}
				}
				Ok(Some(stderr_line)) = stderr.next_line() => {
					if let Some(on_stderr) = &callbacks.on_stderr {
						queue.lock().await.push_back(on_stderr(stderr_line));
					}
				}
				ret = command.wait() => {
					let ret = ret.unwrap();
					let is_restarting = {
						let inner = self.inner.read().await;
						let process = inner.processes.get(key).unwrap();
						inner.max_restarts > process.restarts
					};
					if let Some(on_exit) = &callbacks.on_exit {
						queue.lock().await.push_back(on_exit(ret.code(), is_restarting));
					}
					if is_restarting {
						if let Some(new_command) = self.restart_process(key).await {
							command = new_command;
							stdout = BufReader::new(command.stdout.take().unwrap()).lines();
							stderr = BufReader::new(command.stderr.take().unwrap()).lines();
							if let Some(on_start) = &callbacks.on_start {
								queue.lock().await.push_back(on_start(true));
							}
							continue;
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
}

struct ProcessManagerInner {
	max_restarts: usize,
	processes: SlotMap<ProcessKey, ProcessData>,
}
