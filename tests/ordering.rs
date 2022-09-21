use launch_pad::{process::Process, ProcessKey, ProcessManager};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

static LAST_NUMBER: AtomicUsize = AtomicUsize::new(0);
static EXITED: AtomicBool = AtomicBool::new(false);
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

async fn on_stdout(_: ProcessManager, _: ProcessKey, line: String) {
	let last_number = LAST_NUMBER.load(Ordering::SeqCst);
	let number = line.parse::<usize>().expect("unexpected output from test");
	if number != last_number + 1 {
		panic!("expected {} but got {}", last_number + 1, number);
	}
	LAST_NUMBER.store(number, Ordering::SeqCst);
}

async fn on_exit(pm: ProcessManager, key: ProcessKey, exit_code: Option<i32>, _: bool) {
	if EXITED.load(Ordering::SeqCst) {
		return;
	}
	pm.stop_process(key).await.expect("failed to stop process");
	EXIT_CODE.store(exit_code.unwrap_or(0), Ordering::SeqCst);
	EXITED.store(true, Ordering::SeqCst);
}

#[tokio::test]
async fn ensure_ordering() {
	let pm = ProcessManager::new().await;
	pm.start_process(
		Process::new()
			.with_executable("target/debug/ordering")
			.with_on_stdout(on_stdout)
			.with_on_exit(on_exit),
	)
	.await
	.expect("failed to start process");
	loop {
		if !EXITED.load(Ordering::SeqCst) {
			tokio::task::yield_now().await;
		}
		pm.stop();
		std::process::exit(EXIT_CODE.load(Ordering::SeqCst));
	}
}
