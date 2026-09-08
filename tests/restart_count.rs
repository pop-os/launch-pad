use launch_pad::{ProcessKey, ProcessManager, RestartMode, process::Process};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static RESTARTS: AtomicUsize = AtomicUsize::new(0);
static CRASH_LOOP_RESTARTS: AtomicUsize = AtomicUsize::new(0);

const MAX_RESTARTS: usize = 2;
const RESET_AFTER: Duration = Duration::from_millis(50);

async fn on_start(_: ProcessManager, _: ProcessKey, restarted: bool) {
    if restarted {
        RESTARTS.fetch_add(1, Ordering::SeqCst);
    }
}

async fn on_crash_loop_start(_: ProcessManager, _: ProcessKey, restarted: bool) {
    if restarted {
        CRASH_LOOP_RESTARTS.fetch_add(1, Ordering::SeqCst);
    }
}

/// `max_restarts` is compared against a count that accumulated for the lifetime
/// of the manager, so a process failing occasionally over a long session
/// eventually stopped being restarted at all. Every run here outlives
/// `restart_count_reset_after`, so the count must return to zero and restarts
/// must continue past `max_restarts`.
#[tokio::test]
async fn restart_count_resets_after_uptime() {
    let pm = ProcessManager::new().await;
    pm.set_restart_mode(RestartMode::Instant).await;
    pm.set_max_restarts(MAX_RESTARTS).await;
    pm.set_restart_count_reset_after(RESET_AFTER).await;

    pm.start_process(
        Process::new()
            .with_executable("/bin/sh")
            .with_args(["-c", "sleep 0.2; exit 1"])
            .with_on_start(on_start),
    )
    .await
    .expect("failed to start process");

    let target = MAX_RESTARTS + 3;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while RESTARTS.load(Ordering::SeqCst) < target {
        assert!(
            tokio::time::Instant::now() < deadline,
            "stalled at {} restarts with max_restarts = {}; the count is not being cleared",
            RESTARTS.load(Ordering::SeqCst),
            MAX_RESTARTS
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    pm.stop();
}

/// A process failing faster than `restart_count_reset_after` is a genuine crash
/// loop, so the count must survive and `max_restarts` must still stop it.
#[tokio::test]
async fn restart_count_survives_a_crash_loop() {
    let pm = ProcessManager::new().await;
    pm.set_restart_mode(RestartMode::Instant).await;
    pm.set_max_restarts(MAX_RESTARTS).await;
    pm.set_restart_count_reset_after(Duration::from_secs(30))
        .await;

    pm.start_process(
        Process::new()
            .with_executable("/bin/sh")
            .with_args(["-c", "exit 1"])
            .with_on_start(on_crash_loop_start),
    )
    .await
    .expect("failed to start process");

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        CRASH_LOOP_RESTARTS.load(Ordering::SeqCst),
        MAX_RESTARTS,
        "a crash loop must still stop at max_restarts"
    );
    pm.stop();
}
