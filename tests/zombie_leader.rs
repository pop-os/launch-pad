// SPDX-License-Identifier: MPL-2.0

//! End-to-end proof that supervision survives a child whose thread-group leader
//! dies while its sibling threads keep running.
//!
//! This is the state a kernel oops in a driver leaves behind: `do_exit()` runs
//! on the faulting task alone, so a leader that faults becomes an unreapable
//! zombie and `waitpid` blocks forever. Before the liveness check, `on_exit`
//! never fired and the supervised process was never restarted.
//!
//! Linux-only: it depends on procfs and on `do_exit` semantics.
#![cfg(target_os = "linux")]

use launch_pad::{ProcessManager, process::Process};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Builds a helper that reproduces the failure, and returns its path.
///
/// The helper spawns a thread, then calls `pthread_exit` on its main thread.
/// That terminates the leader alone — exactly what the kernel does to the
/// faulting task on an oops — leaving a zombie leader with a live sibling.
fn build_wedging_helper() -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("launch-pad-zombie-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let src = dir.join("wedge.c");
    let bin = dir.join("wedge");

    std::fs::write(
        &src,
        r#"
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>
static void *worker(void *a) { (void)a; for (;;) pause(); return 0; }
int main(void) {
    pthread_t t;
    pthread_create(&t, 0, worker, 0);
    printf("wedged\n");
    fflush(stdout);
    pthread_exit(0);   /* leader exits alone; worker lives on */
}
"#,
    )
    .ok()?;

    let ok = std::process::Command::new("cc")
        .arg("-O0")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .arg("-lpthread")
        .status()
        .ok()
        .is_some_and(|s| s.success());

    ok.then_some(bin)
}

#[tokio::test(flavor = "multi_thread")]
async fn wedged_child_is_detected_and_supervision_continues() {
    let Some(helper) = build_wedging_helper() else {
        eprintln!("skipping: no working C compiler to build the helper");
        return;
    };

    let manager = ProcessManager::new().await;
    // Poll briskly so the test does not have to wait on the 5s default.
    manager
        .set_liveness_interval(Some(Duration::from_millis(200)))
        .await;
    // One restart is enough to prove the exit was noticed and acted on.
    manager.set_max_restarts(1).await;

    let exited = Arc::new(AtomicBool::new(false));
    let exited_tx = exited.clone();

    manager
        .start(
            Process::new()
                .with_executable(helper.to_string_lossy().to_string())
                .with_on_exit(move |_, _, _, _| {
                    let exited_tx = exited_tx.clone();
                    async move {
                        exited_tx.store(true, Ordering::SeqCst);
                    }
                }),
        )
        .await
        .expect("failed to start helper");

    // Generous relative to the 200ms poll; fails fast on regression because the
    // flag is checked continuously rather than after a fixed sleep.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !exited.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        exited.load(Ordering::SeqCst),
        "on_exit never fired: a zombie thread-group leader went undetected, so waitpid \
         blocked forever and supervision was lost"
    );

    manager.stop();
    let _ = std::fs::remove_dir_all(helper.parent().unwrap());
}

/// The check must not disturb a well-behaved process.
#[tokio::test(flavor = "multi_thread")]
async fn healthy_process_is_left_alone() {
    let manager = ProcessManager::new().await;
    manager
        .set_liveness_interval(Some(Duration::from_millis(100)))
        .await;
    manager.set_max_restarts(0).await;

    let exited = Arc::new(AtomicBool::new(false));
    let exited_tx = exited.clone();

    manager
        .start(
            Process::new()
                .with_executable("sleep".to_string())
                .with_args(["3".to_string()])
                .with_on_exit(move |_, _, _, _| {
                    let exited_tx = exited_tx.clone();
                    async move {
                        exited_tx.store(true, Ordering::SeqCst);
                    }
                }),
        )
        .await
        .expect("failed to start sleep");

    // Many liveness ticks elapse here; none of them should kill it.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !exited.load(Ordering::SeqCst),
        "the liveness check killed a healthy process"
    );

    manager.stop();
}
