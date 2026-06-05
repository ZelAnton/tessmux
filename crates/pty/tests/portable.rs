//! Integration tests for the `portable-pty` backend against real shells.
//! Hermetic: runs headless on all three CI OSes (ConPTY needs no visible
//! console). The backend-generic contract lives in `contract.rs`.

mod common;

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{
    TEST_TIMEOUT, interactive_shell, long_running_command, shell_command, spawn_chunk_pump,
    wait_with_timeout,
};
use tessmux_pty::{PortablePtyBackend, PtyBackend, PtyError, PtySize};

#[test]
fn nonzero_exit_is_reported() {
    let session = PortablePtyBackend
        .spawn(&shell_command("exit 7"), PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");
    let _chunks = spawn_chunk_pump(session.as_ref());

    let status = wait_with_timeout(session);
    assert_eq!(
        status.code, 7,
        "exit code must pass through, got {status:?}"
    );
    assert!(!status.success, "non-zero exit must not be success");
}

#[test]
fn writer_drives_interactive_shell() {
    // Proves the take_writer()->child-stdin path end to end: an interactive
    // shell receives `exit` on its stdin and terminates cleanly.
    let mut session = PortablePtyBackend
        .spawn(&interactive_shell(), PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");
    let _chunks = spawn_chunk_pump(session.as_ref());

    let mut writer = session.take_writer().expect("take_writer failed");
    assert!(
        matches!(session.take_writer(), Err(PtyError::WriterTaken)),
        "writer is single-use: the second take must report WriterTaken"
    );
    writer
        .write_all(b"exit\r\n")
        .and_then(|()| writer.flush())
        .expect("write to child stdin failed");

    let status = wait_with_timeout(session);
    assert!(
        status.success,
        "shell should exit cleanly after 'exit', got {status:?}"
    );
}

#[test]
fn kill_terminates_child() {
    let session = PortablePtyBackend
        .spawn(&long_running_command(), PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");

    // Drain the output pipe in the background: a full pipe blocks conhost,
    // which can wedge pseudo-console teardown.
    let _chunks = spawn_chunk_pump(session.as_ref());

    let mut killer = session.killer();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300)); // let the child boot
        let _ = killer.kill();
    });

    // Bounded wait: a child the kill failed to reap fails the test cleanly
    // instead of hanging the whole CI job.
    let _status = wait_with_timeout(session);
}

#[test]
fn try_wait_reports_running_then_exit_and_blocks_late_kill() {
    let mut session = PortablePtyBackend
        .spawn(&long_running_command(), PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");
    let _chunks = spawn_chunk_pump(session.as_ref());

    assert!(
        session.try_wait().expect("try_wait failed").is_none(),
        "child should still be running"
    );

    let mut killer = session.killer();
    killer.kill().expect("kill failed");

    // Termination is asynchronous; poll try_wait until the child is reaped.
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        if session.try_wait().expect("try_wait failed").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child not reaped within {TEST_TIMEOUT:?} after kill"
        );
        thread::sleep(Duration::from_millis(50));
    }

    // The child is reaped: a late kill must be refused, not signalled.
    assert!(
        matches!(killer.kill(), Err(PtyError::AlreadyReaped)),
        "kill after reap must report AlreadyReaped"
    );
}

#[test]
fn close_unblocks_reader_and_is_idempotent() {
    let mut session = PortablePtyBackend
        .spawn(
            &shell_command("echo poc0-marker"),
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn failed");

    // Reader cloned BEFORE close — it must reach EOF afterwards.
    let mut reader = session.reader().expect("reader failed");

    // Direct wait: `echo` exits immediately (same pattern the contract test
    // bounds with a pump; here the session must stay with this thread).
    let status = session.wait().expect("wait failed");
    assert!(status.success);

    session.close().expect("close failed");
    session.close().expect("close must be idempotent");

    // POSTCONDITION: read_to_end completes (EOF) instead of blocking on the
    // console staying open. Run on a thread purely as a CI hang-guard — the
    // contract says it cannot block.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut tail = Vec::new();
        let _ = reader.read_to_end(&mut tail);
        let _ = tx.send(());
    });
    rx.recv_timeout(TEST_TIMEOUT)
        .expect("reader did not reach EOF after close()");

    // Closed-state contract.
    assert!(matches!(session.reader(), Err(PtyError::Closed)));
    assert!(matches!(
        session.resize(PtySize {
            rows: 30,
            cols: 100
        }),
        Err(PtyError::Closed)
    ));
    assert!(matches!(
        session.resizer().resize(PtySize {
            rows: 30,
            cols: 100
        }),
        Err(PtyError::Closed)
    ));
}
