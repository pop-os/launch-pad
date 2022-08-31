// SPDX-License-Identifier: MPL-2.0

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProcessMessage {
	Started,
	Stdout(String),
	Stderr(String),
	Exited { code: Option<i32>, restarting: bool },
}
