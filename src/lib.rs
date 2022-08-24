// SPDX-License-Identifier: MPL-2.0
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct ProcessManager {
	inner: Arc<RwLock<ProcessManagerInner>>,
}

impl ProcessManager {
	pub fn start(
		&self,
		name: impl AsRef<str>,
		args: impl Into<Option<Vec<String>>>,
		env: impl Into<Option<Vec<(String, String)>>>,
	) {
		self.start_impl(
			name.as_ref(),
			args.into().unwrap_or_default(),
			env.into().unwrap_or_default(),
		);
	}

	fn start_impl(&self, name: &str, args: Vec<String>, env: Vec<(String, String)>) {}
}

struct ProcessManagerInner {
	max_restarts: usize,
}
