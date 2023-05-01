// SPDX-License-Identifier: MPL-2.0
use super::{ProcessKey, ProcessManager};
use std::{borrow::Cow, future::Future, pin::Pin};

pub type ReturnFuture = Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>;
pub type StringCallback =
	Box<dyn Fn(ProcessManager, ProcessKey, String) -> ReturnFuture + Send + Sync + 'static>;
pub type PskCallback =
	Box<dyn Fn(ProcessManager, ProcessKey, Vec<u8>) -> ReturnFuture + Send + Sync + 'static>;
pub type StartedCallback =
	Box<dyn Fn(ProcessManager, ProcessKey, bool) -> ReturnFuture + Send + Sync + 'static>;
pub type ExitedCallback = Box<
	dyn Fn(ProcessManager, ProcessKey, Option<i32>, bool) -> ReturnFuture + Send + Sync + 'static,
>;

#[derive(Default)]
pub(crate) struct ProcessCallbacks {
	pub(crate) on_stdout: Option<StringCallback>,
	pub(crate) on_stderr: Option<StringCallback>,
	pub(crate) on_start: Option<StartedCallback>,
	pub(crate) on_exit: Option<ExitedCallback>,
	pub(crate) on_psk: Option<PskCallback>,
}

#[derive(Default)]
pub struct Process {
	pub(crate) executable: String,
	pub(crate) args: Vec<String>,
	pub(crate) env: Vec<(String, String)>,
	pub(crate) callbacks: ProcessCallbacks,
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
		F: Fn(ProcessManager, ProcessKey, String) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Send + Sync + 'static,
	{
		self.callbacks.on_stdout = Some(Box::new(move |p, k, s| Box::pin(on_stdout(p, k, s))));
		self
	}

	/// Sets the callback to run when the process writes to stderr.
	pub fn with_on_stderr<F, A>(mut self, on_stderr: F) -> Self
	where
		F: Fn(ProcessManager, ProcessKey, String) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Send + Sync + 'static,
	{
		self.callbacks.on_stderr = Some(Box::new(move |p, k, s| Box::pin(on_stderr(p, k, s))));
		self
	}

	/// Sets the callback to run when the process starts.
	/// This is called before the process is started.
	///
	/// It passes a single argument: a bool indicating whether the process was
	/// restarted or if it was started for the first time.
	pub fn with_on_start<F, A>(mut self, on_start: F) -> Self
	where
		F: Fn(ProcessManager, ProcessKey, bool) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Send + Sync + 'static,
	{
		self.callbacks.on_start = Some(Box::new(move |p, k, r| Box::pin(on_start(p, k, r))));
		self
	}

	/// Sets the callback to run when the process exits.
	/// This is called after the process exits, or before it restarts.
	///
	/// It passes two arguments: an optional exit code, and a bool indicating
	/// whether the process is going to be restarted or not.
	pub fn with_on_exit<F, A>(mut self, on_exit: F) -> Self
	where
		F: Fn(ProcessManager, ProcessKey, Option<i32>, bool) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Send + Sync + 'static,
	{
		self.callbacks.on_exit = Some(Box::new(move |p, k, code, restarting| {
			Box::pin(on_exit(p, k, code, restarting))
		}));
		self
	}

	/// Sets the callback to run when a new pre-shared key (PSK) is generated.
	/// This optionally happens when the process is started, and restarted.
	/// The PSK is a [u8; 64] and is piped to the child process via stdin.
	/// Though only the SHA256 has of the key is passed back here.
	/// You can use this hash for authentication assuming:
	///		1. The child handles the PSK safely
	///		2. The authentication server sha256 hashes the PSK when sent to it
	///		3. The PSK is used only once. The child process should restart
	///		   and generate a new key if needed. The server shouldn't
	///		   authenticate the same key twice. Ever.
	///		4. The child should authenticate and drop the PSK as fast as possible.
	///
	/// It passes one argument: A Vec<u8> of the bytes of the hash of the PSK.
	pub fn with_on_auth_key<F, A>(mut self, on_stream: F) -> Self
	where
		F: Fn(ProcessManager, ProcessKey, Vec<u8>) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Send + Sync + 'static,
	{
		self.callbacks.on_psk = Some(Box::new(move |p, k, byte| Box::pin(on_stream(p, k, byte))));
		self
	}

	/// Returns a human readable, escaped string of the executable.
	/// Used for logging.
	pub(crate) fn exe_text(&self) -> Cow<'_, str> {
		if self.executable.contains(' ') {
			Cow::Owned(format!("\"{}\"", self.executable))
		} else {
			Cow::Borrowed(&self.executable)
		}
	}

	/// Returns a human readable, escaped string of the environment variables.
	/// Used for logging.
	pub(crate) fn env_text(&self) -> Cow<'static, str> {
		if self.env.is_empty() {
			Cow::Borrowed("")
		} else {
			Cow::Owned(self.env.iter().fold(String::new(), |acc, (k, v)| {
				if v.contains(' ') {
					format!("{} {}=\"{}\"", acc, k, v)
				} else {
					format!("{} {}={}", acc, k, v)
				}
			}))
		}
	}

	/// Returns a human readable, escaped string of the arguments.
	/// Used for logging.
	pub(crate) fn args_text(&self) -> Cow<'static, str> {
		if self.args.is_empty() {
			Cow::Borrowed("")
		} else {
			Cow::Owned(self.args.iter().fold(String::new(), |acc, arg| {
				if arg.contains(' ') {
					format!("{} \"{}\"", acc, arg)
				} else {
					format!("{} {}", acc, arg)
				}
			}))
		}
	}
}
