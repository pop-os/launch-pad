// SPDX-License-Identifier: GPL-3.0-only
use nix::fcntl;
use std::os::unix::prelude::*;

pub(crate) fn mark_as_not_cloexec(raw_fd: RawFd) -> Result<(), tokio::io::Error> {
	let Some(fd_flags) = fcntl::FdFlag::from_bits(fcntl::fcntl(raw_fd, fcntl::FcntlArg::F_GETFD)?)
	else {
		return Err(tokio::io::Error::new(
			tokio::io::ErrorKind::Other,
			"failed to get fd flags from file",
		));
	};
	fcntl::fcntl(
		raw_fd,
		fcntl::FcntlArg::F_SETFD(fd_flags.difference(fcntl::FdFlag::FD_CLOEXEC)),
	)?;
	Ok(())
}
