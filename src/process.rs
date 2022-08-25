// SPDX-License-Identifier: MPL-2.0

pub struct Process {
	executable: String,
	args: Vec<String>,
	env: Vec<(String, String)>,
}
