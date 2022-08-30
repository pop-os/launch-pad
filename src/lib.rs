// SPDX-License-Identifier: MPL-2.0
pub mod process;

pub use self::process::Process;
use slotmap::{new_key_type, SlotMap};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};

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
					let mut inner = inner.write().await;
					return_tx
						.send(inner.processes.insert(process))
						.expect("failed to send response");
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

struct ProcessManagerInner {
	max_restarts: usize,
	processes: SlotMap<ProcessKey, Process>,
}
