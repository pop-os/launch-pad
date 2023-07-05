use std::borrow::Cow;

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
	#[error("process does not exist")]
	NonExistantProcess,
	#[error("process manager has been shut down")]
	Stopped,
	#[error("failed to send message to process stdin: {0}")]
	StdinMessage(#[from] tokio::sync::mpsc::error::SendError<Cow<'static, [u8]>>),
	#[error("recever for stdin messages is missing from process")]
	MissingStdinReceiver,
}

pub type Result<T> = std::result::Result<T, Error>;
