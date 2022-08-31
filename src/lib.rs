// SPDX-License-Identifier: MPL-2.0
pub mod message;
pub mod process;

pub use self::{message::ProcessMessage, process::Process};
use slotmap::{new_key_type, SlotMap};
use std::{process::Stdio, sync::Arc};
use tokio::{
	io::{AsyncBufReadExt, BufReader},
	process::Command,
	sync::{broadcast, mpsc, oneshot, RwLock},
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
		let manager = ProcessManager {
			inner: inner.clone(),
			tx,
		};
		tokio::spawn(async move {
			loop {
				while let Some((process, return_tx)) = rx.recv().await {
					return_tx
						.send(inner.write().await.start_process(process))
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
}

struct ProcessData {
	process: Process,
	restarts: usize,
}

struct ProcessManagerInner {
	max_restarts: usize,
	processes: SlotMap<ProcessKey, ProcessData>,
}

impl ProcessManagerInner {
	pub fn start_process(&mut self, mut process: Process) -> ProcessKey {
		let mut command = Command::new(&process.executable)
			.args(&process.args)
			.envs(process.env.clone())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.stdin(Stdio::null())
			.kill_on_drop(true)
			.spawn()
			.expect("failed to start process");
		let (on_stdout, on_stderr, on_start, on_exit) = (
			process.on_stdout.take(),
			process.on_stderr.take(),
			process.on_start.take(),
			process.on_exit.take(),
		);
		let key = self.processes.insert(ProcessData {
			process,
			restarts: 0,
		});
		if let Some(on_start) = &on_start {
			tokio::spawn(on_start(false));
		}
		tokio::spawn(async move {
			let mut stdout = BufReader::new(command.stdout.take().unwrap()).lines();
			let mut stderr = BufReader::new(command.stderr.take().unwrap()).lines();
			loop {
				tokio::select! {
					Ok(Some(stdout_line)) = stdout.next_line() => {
						if let Some(on_stdout) = &on_stdout {
							on_stdout(stdout_line).await;
						}
					}
					Ok(Some(stderr_line)) = stderr.next_line() => {
						if let Some(on_stderr) = &on_stderr {
							on_stderr(stderr_line).await;
						}
					}
					ret = command.wait() => {
						let ret = ret.unwrap();
						if let Some(on_exit) = &on_exit {
							on_exit(ret.code(), false).await;
						}
						break;
					}
				}
			}
		});
		key
	}
}
