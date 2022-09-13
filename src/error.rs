// SPDX-License-Identifier: MPL-2.0
use crate::ProcessKey;
use thiserror::Error as ThisError;
use tokio::sync::oneshot::error::RecvError;

#[derive(Debug, ThisError)]
pub enum Error {
	#[error("invalid process {:?}", .0)]
	InvalidProcess(ProcessKey),
	#[error("failed to receive return message: {0}")]
	ReturnMessage(#[from] RecvError),
	#[error("failed to start process: {0}")]
	Process(std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
