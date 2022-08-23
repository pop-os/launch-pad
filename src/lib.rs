// SPDX-License-Identifier: MPL-2.0

pub struct ProcessManager {
	max_restarts: usize,
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
