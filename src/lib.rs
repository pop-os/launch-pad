// SPDX-License-Identifier: MPL-2.0
pub mod process;

pub use self::process::Process;
use slotmap::{new_key_type, SlotMap};
use std::sync::Arc;
use tokio::sync::RwLock;

new_key_type! { pub struct ProcessKey; }

pub struct ProcessManager {
	inner: Arc<RwLock<ProcessManagerInner>>,
}

impl ProcessManager {
	pub async fn start(&self, process: Process) -> ProcessKey {
		self.start_impl(process).await
	}

	async fn start_impl(&self, process: Process) -> ProcessKey {
		let mut inner = self.inner.write().await;
		inner.processes.insert(process)
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
