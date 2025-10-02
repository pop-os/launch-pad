// SPDX-License-Identifier: MPL-2.0
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use std::os::fd::BorrowedFd;

pub(crate) fn mark_as_not_cloexec(raw_fd: BorrowedFd) -> Result<(), tokio::io::Error> {
    let fd_flags = fcntl_getfd(raw_fd)?;
    fcntl_setfd(raw_fd, fd_flags.difference(FdFlags::CLOEXEC))?;
    Ok(())
}
