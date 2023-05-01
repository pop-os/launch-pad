// SPDX-License-Identifier: MPL-2.0
use crate::ProcessKey;
use thiserror::Error as ThisError;
use tokio::sync::oneshot::error::RecvError;

#[derive(Debug, ThisError)]
pub enum Error {
	#[error("invalid process {:?}", .0)]
	InvalidProcess(ProcessKey),
	#[error("failed to send message")]
	SendMessage,
	#[error("failed to receive return message: {0}")]
	ReturnMessage(#[from] RecvError),
	#[error("failed to start process: {0}")]
	Process(std::io::Error),
	#[error("process does not exist")]
	NonExistantProcess,
	#[error("process manager has been shut down")]
	Stopped,
	#[error("Generic IO Error")]
	Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
