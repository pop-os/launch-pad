// SPDX-License-Identifier: MPL-2.0

use std::future::Future;

pub type ReturnFuture = Box<dyn Future<Output = ()> + Unpin + Send + Sync + 'static>;
pub type StringCallback = Box<dyn Fn(String) -> ReturnFuture + Unpin + Send + Sync + 'static>;
pub type StartedCallback = Box<dyn Fn(bool) -> ReturnFuture + Unpin + Send + Sync + 'static>;
pub type ExitedCallback =
	Box<dyn Fn(Option<i32>, bool) -> ReturnFuture + Unpin + Send + Sync + 'static>;

#[derive(Default)]
pub struct Process {
	pub(crate) executable: String,
	pub(crate) args: Vec<String>,
	pub(crate) env: Vec<(String, String)>,
	pub(crate) on_stdout: Option<StringCallback>,
	pub(crate) on_stderr: Option<StringCallback>,
	pub(crate) on_start: Option<StartedCallback>,
	pub(crate) on_exit: Option<ExitedCallback>,
}

impl Process {
	pub fn new() -> Self {
		Self::default()
	}

	/// Sets the executable to run.
	pub fn with_executable(mut self, executable: impl ToString) -> Self {
		self.executable = executable.to_string();
		self
	}

	/// Sets the arguments to pass to the executable.
	pub fn with_args(mut self, args: impl IntoIterator<Item = impl ToString>) -> Self {
		self.args = args.into_iter().map(|s| s.to_string()).collect();
		self
	}

	/// Sets the environment variables to pass to the executable.
	pub fn with_env(
		mut self,
		env: impl IntoIterator<Item = (impl ToString, impl ToString)>,
	) -> Self {
		self.env = env
			.into_iter()
			.map(|(k, v)| (k.to_string(), v.to_string()))
			.collect();
		self
	}

	/// Sets the callback to run when the process writes to stdout.
	pub fn with_on_stdout<F, A>(mut self, on_stdout: F) -> Self
	where
		F: Fn(String) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Unpin + Send + Sync + 'static,
	{
		self.on_stdout = Some(Box::new(move |s| Box::new(on_stdout(s))));
		self
	}

	/// Sets the callback to run when the process writes to stderr.
	pub fn with_on_stderr<F, A>(mut self, on_stderr: F) -> Self
	where
		F: Fn(String) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Unpin + Send + Sync + 'static,
	{
		self.on_stderr = Some(Box::new(move |s| Box::new(on_stderr(s))));
		self
	}

	/// Sets the callback to run when the process starts.
	/// This is called before the process is started.
	///
	/// It passes a single argument: a bool indicating whether the process was
	/// restarted or if it was started for the first time.
	pub fn with_on_start<F, A>(mut self, on_start: F) -> Self
	where
		F: Fn(bool) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Unpin + Send + Sync + 'static,
	{
		self.on_start = Some(Box::new(move |r| Box::new(on_start(r))));
		self
	}

	/// Sets the callback to run when the process exits.
	/// This is called after the process exits, or before it restarts.
	///
	/// It passes two arguments: an optional exit code, and a bool indicating
	/// whether the process is going to be restarted or not.
	pub fn with_on_exit<F, A>(mut self, on_exit: F) -> Self
	where
		F: Fn(Option<i32>, bool) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Unpin + Send + Sync + 'static,
	{
		self.on_exit = Some(Box::new(move |code, restarting| {
			Box::new(on_exit(code, restarting))
		}));
		self
	}
}
