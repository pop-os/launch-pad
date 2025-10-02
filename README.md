# launch-pad

Asynchronously spawn processes with automatic restarts and exponentional backoffs.
Pass file descriptors to child processes and configure callbacks for stdout, stderr, and exit handling.

## Examples

Spawning cosmic-settings-daemon as a session service

```rust
let process_manager = ProcessManager::new().await;
_ = process_manager.set_max_restarts(usize::MAX).await;
_ = process_manager
	.set_restart_mode(launch_pad::RestartMode::ExponentialBackoff(
		Duration::from_millis(10),
	))
	.await;

let stdout_span = info_span!(parent: None, "cosmic-settings-daemon");
let stderr_span = stdout_span.clone();
let (settings_exit_tx, settings_exit_rx) = oneshot::channel();
let settings_exit_tx = Arc::new(std::sync::Mutex::new(Some(settings_exit_tx)));
let settings_daemon = process_manager
	.start(
		Process::new()
			.with_executable("cosmic-settings-daemon")
			.with_on_stdout(move |_, _, line| {
				let stdout_span = stdout_span.clone();
				async move {
					info!("{}", line);
				}
				.instrument(stdout_span)
			})
			.with_on_stderr(move |_, _, line| {
				let stderr_span = stderr_span.clone();
				async move {
					warn!("{}", line);
				}
				.instrument(stderr_span)
			})
			.with_on_exit(move |_, _, _, will_restart| {
				if !will_restart {
					if let Some(tx) = settings_exit_tx.lock().unwrap().take() {
						_ = tx.send(());
					}
				}
				async {}
			}),
	)
	.await
	.expect("failed to start settings daemon");
```

Spawning a cosmic desktop applet

```rust
async fn start_component(
	cmd: impl Into<Cow<'static, str>>,
	span: tracing::Span,
	process_manager: &ProcessManager,
	env_vars: &[(String, String)],
	socket_tx: &mpsc::UnboundedSender<Vec<UnixStream>>,
	extra_fds: Vec<(OwnedFd, (String, String), UnixStream)>,
) {
	let mut sockets = Vec::with_capacity(2);
	let (mut env_vars, fd) = create_privileged_socket(&mut sockets, &env_vars).unwrap();

	let socket_tx_clone = socket_tx.clone();
	let stdout_span = span.clone();
	let stderr_span = span.clone();
	let stderr_span_clone = stderr_span.clone();
	let cmd = cmd.into();
	let cmd_clone = cmd.clone();

	let (mut fds, extra_fd_env, mut streams): (Vec<_>, Vec<_>, Vec<_>) =
		itertools::multiunzip(extra_fds);
	for kv in &extra_fd_env {
		env_vars.push(kv.clone());
	}

	sockets.append(&mut streams);
	if let Err(why) = socket_tx.send(sockets) {
		error!(?why, "Failed to send the privileged socket");
	}
	let (extra_fd_env, _): (Vec<_>, Vec<_>) = extra_fd_env.into_iter().unzip();
	fds.push(fd);
	if let Err(err) = process_manager
		.start(
			Process::new()
				.with_executable(cmd.clone())
				.with_env(env_vars.iter().cloned())
				.with_on_stdout(move |_, _, line| {
					let stdout_span = stdout_span.clone();
					async move {
						info!("{}", line);
					}
					.instrument(stdout_span)
				})
				.with_on_stderr(move |_, _, line| {
					let stderr_span = stderr_span.clone();
					async move {
						warn!("{}", line);
					}
					.instrument(stderr_span)
				})
				.with_on_start(move |pman, pkey, _will_restart| async move {
					#[cfg(feature = "systemd")]
					if *is_systemd_used() {
						if let Ok((innr_cmd, Some(pid))) = pman.get_exe_and_pid(pkey).await {
							if let Err(err) = spawn_scope(innr_cmd.clone(), vec![pid]).await {
								warn!(
									"Failed to spawn scope for {}. Creating transient unit failed \
									 with {}",
									innr_cmd, err
								);
							};
						}
					}
				})
				.with_on_exit(move |mut pman, key, err_code, will_restart| {
					if let Some(err) = err_code {
						error!("{cmd_clone} exited with error {}", err.to_string());
					}
					let extra_fd_env = extra_fd_env.clone();
					let socket_tx_clone = socket_tx_clone.clone();
					async move {
						if !will_restart {
							return;
						}

						let mut sockets = Vec::with_capacity(1 + extra_fd_env.len());
						let mut fds = Vec::with_capacity(1 + extra_fd_env.len());
						let (mut env_vars, fd) =
							create_privileged_socket(&mut sockets, &[]).unwrap();
						fds.push(fd);
						for k in extra_fd_env {
							let (mut fd_env_vars, fd) =
								create_privileged_socket(&mut sockets, &[]).unwrap();
							fd_env_vars.last_mut().unwrap().0 = k;
							env_vars.append(&mut fd_env_vars);
							fds.push(fd)
						}

						if let Err(why) = socket_tx_clone.send(sockets) {
							error!(?why, "Failed to send the privileged socket");
						}
						if let Err(why) = pman.update_process_env(&key, env_vars).await {
							error!(?why, "Failed to update environment variables");
						}
						if let Err(why) = pman.update_process_fds(&key, move || fds).await {
							error!(?why, "Failed to update fds");
						}
					}
				})
				.with_fds(move || fds),
		)
		.await
	{
		let _enter = stderr_span_clone.enter();
		error!("failed to start {}: {}", cmd, err);
	}
}
```
