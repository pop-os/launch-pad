use tokio::sync::mpsc;

// SPDX-License-Identifier: MPL-2.0
use super::{ProcessKey, ProcessManager};
use std::{borrow::Cow, future::Future, os::fd::OwnedFd, pin::Pin, time::Duration};

pub type ReturnFuture =
	sync_wrapper::SyncFuture<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>;
pub type StringCallback =
	Box<dyn Fn(ProcessManager, ProcessKey, String) -> ReturnFuture + Send + Sync + 'static>;
pub type StartedCallback =
	Box<dyn Fn(ProcessManager, ProcessKey, bool) -> ReturnFuture + Send + Sync + 'static>;
/// Type for the callback that is called when a process exits. Arguments are the
/// process manager, the process key, the error code to return, and a
/// bool indicating if the process is going to be restarted.
pub type ExitedCallback = Box<
	dyn Fn(ProcessManager, ProcessKey, Option<i32>, bool) -> ReturnFuture + Send + Sync + 'static,
>;
pub type BlockingCallback = Box<dyn Fn(ProcessManager, ProcessKey, bool) + Send + Sync + 'static>;

#[derive(Default)]
pub(crate) struct ProcessCallbacks {
	pub(crate) on_stdout: Option<StringCallback>,
	pub(crate) on_stderr: Option<StringCallback>,
	pub(crate) on_start: Option<StartedCallback>,
	pub(crate) on_exit: Option<ExitedCallback>,
	pub(crate) fds: Option<Box<dyn FnOnce() -> Vec<OwnedFd> + Send + Sync + 'static>>,
}

pub struct Process {
	pub(crate) executable: String,
	pub(crate) args: Vec<String>,
	pub(crate) env: Vec<(String, String)>,
	pub(crate) callbacks: ProcessCallbacks,
	pub(crate) stdin_tx: mpsc::Sender<Cow<'static, [u8]>>,
	pub(crate) stdin_rx: Option<mpsc::Receiver<Cow<'static, [u8]>>>,
	pub(crate) cancel_timeout: Option<Duration>,
}

impl Process {
	pub fn new() -> Self {
		let (stdin_tx, stdin_rx) = mpsc::channel(10);
		Self {
			executable: String::new(),
			args: Vec::new(),
			env: Vec::new(),
			callbacks: ProcessCallbacks::default(),
			stdin_tx,
			stdin_rx: Some(stdin_rx),
			cancel_timeout: Some(Duration::from_secs(1)),
		}
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

	/// Sets the cancellation timeout before forcing the process to exit.
	pub fn with_cancel_timeout(mut self, t: impl Into<Option<Duration>>) -> Self {
		self.cancel_timeout = t.into();
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
		A: Future<Output = ()> + Send + 'static,
	{
		self.callbacks.on_stdout = Some(Box::new(move |p, k, s| {
			sync_wrapper::SyncFuture::new(Box::pin(on_stdout(p, k, s)))
		}));
		self
	}

	/// Sets the callback to run when the process writes to stderr.
	pub fn with_on_stderr<F, A>(mut self, on_stderr: F) -> Self
	where
		F: Fn(ProcessManager, ProcessKey, String) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Send + 'static,
	{
		self.callbacks.on_stderr = Some(Box::new(move |p, k, s| {
			sync_wrapper::SyncFuture::new(Box::pin(on_stderr(p, k, s)))
		}));
		self
	}

	/// Shares Fds with the child process
	/// Closure produces a vector of Fd to share with the child process
	pub fn with_fds<F>(mut self, fds: F) -> Self
	where
		F: FnOnce() -> Vec<OwnedFd> + Send + Sync + 'static,
	{
		self.callbacks.fds = Some(Box::new(fds));
		self
	}

	/// This is called when the process is started.
	///
	/// It passes a single argument: a bool indicating whether the process was
	/// restarted or if it was started for the first time.
	pub fn with_on_start<F, A>(mut self, on_start: F) -> Self
	where
		F: Fn(ProcessManager, ProcessKey, bool) -> A + Unpin + Send + Sync + 'static,
		A: Future<Output = ()> + Send + 'static,
	{
		self.callbacks.on_start = Some(Box::new(move |p, k, r| {
			sync_wrapper::SyncFuture::new(Box::pin(on_start(p, k, r)))
		}));
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
		A: Future<Output = ()> + Send + 'static,
	{
		self.callbacks.on_exit = Some(Box::new(move |p, k, code, restarting| {
			sync_wrapper::SyncFuture::new(Box::pin(on_exit(p, k, code, restarting)))
		}));
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
