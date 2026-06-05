//! Backend-generic contract tests: the SAME assertions run against the real
//! `PortablePtyBackend` and the scripted `FakePtyBackend`. This is the
//! explicit roadmap-§10 proof that the L0 boundary is swappable — and that
//! the fake faithfully mirrors the contracts upper layers will rely on.

mod common;

use std::sync::mpsc::RecvTimeoutError;
use std::time::Instant;

use common::{TEST_TIMEOUT, shell_command, spawn_chunk_pump};
use tessmux_pty::{
    FakeExit, FakePtyBackend, FakeScript, PortablePtyBackend, PtyBackend, PtyError, PtyExitStatus,
    PtySize, SpawnCommand,
};

/// The shared session lifecycle every backend must support: spawn → resize →
/// stream output until a marker → wait (clean exit) → close → reader EOF.
fn exercise_session(backend: &dyn PtyBackend, cmd: &SpawnCommand, marker: &[u8]) {
    let mut session = backend
        .spawn(cmd, PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");

    session
        .resize(PtySize {
            rows: 30,
            cols: 100,
        })
        .expect("resize failed");

    let rx = spawn_chunk_pump(session.as_ref());

    // Stream until the marker shows up (scan only the new chunk plus a
    // marker-length carry-over — not the whole buffer, quadratic otherwise).
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut output = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let chunk = rx.recv_timeout(remaining).unwrap_or_else(|_| {
            panic!(
                "marker not seen within {TEST_TIMEOUT:?}; output so far: {:?}",
                String::from_utf8_lossy(&output)
            )
        });
        let start = output.len().saturating_sub(marker.len() - 1);
        output.extend_from_slice(&chunk);
        if output[start..].windows(marker.len()).any(|w| w == marker) {
            break;
        }
    }

    // reader() is documented multi-call: a second clone must succeed while
    // the session is open.
    drop(session.reader().expect("second reader clone failed"));

    let status = session.wait().expect("wait failed");
    assert!(status.success, "child should exit cleanly, got {status:?}");
    assert_eq!(status.code, 0);

    // Exited-but-not-Closed: the PTY itself must still accept a resize even
    // though the child is gone (lifecycle table row "Exited | resize applies").
    session
        .resize(PtySize { rows: 25, cols: 90 })
        .expect("resize after child exit must succeed");

    // close() postcondition: the pump reaches EOF and its channel
    // disconnects — it must not block on the session staying open.
    session.close().expect("close failed");
    let eof_deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        let remaining = eof_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(_) => continue,                            // draining the tail
            Err(RecvTimeoutError::Disconnected) => break, // EOF — pump ended
            Err(RecvTimeoutError::Timeout) => {
                panic!("pump did not reach EOF within {TEST_TIMEOUT:?} after close()")
            }
        }
    }
}

#[test]
fn portable_backend_honors_the_contract() {
    exercise_session(
        &PortablePtyBackend,
        &shell_command("echo poc0-marker"),
        b"poc0-marker",
    );
}

#[test]
fn fake_backend_honors_the_contract() {
    // Marker split across chunks on purpose: the carry-over scan and the
    // fake's chunk-boundary preservation are both exercised.
    let (backend, probe) = FakePtyBackend::new(FakeScript {
        output_chunks: vec![b"noise poc0-".to_vec(), b"marker tail".to_vec()],
        ..Default::default()
    });
    let cmd = SpawnCommand {
        program: "fake-agent".into(),
        ..Default::default()
    };
    exercise_session(&backend, &cmd, b"poc0-marker");

    // The fake also lets us assert what the session *received*.
    let (spawned_cmd, spawned_size) = probe.spawned().expect("spawn not recorded");
    assert_eq!(spawned_cmd.program, "fake-agent");
    assert_eq!(spawned_size, PtySize { rows: 24, cols: 80 });
    assert_eq!(
        probe.resizes(),
        vec![
            PtySize {
                rows: 30,
                cols: 100
            },
            PtySize { rows: 25, cols: 90 },
        ]
    );
}

#[test]
fn fake_captures_input_and_kill_lifecycle() {
    let (backend, probe) = FakePtyBackend::new(FakeScript {
        exit: PtyExitStatus {
            code: 1,
            success: false,
        },
        exit_mode: FakeExit::OnKill,
        ..Default::default()
    });
    let mut session = backend
        .spawn(&SpawnCommand::default(), PtySize { rows: 24, cols: 80 })
        .expect("spawn failed");

    // Input capture + single-use writer contract.
    let mut writer = session.take_writer().expect("take_writer failed");
    assert!(matches!(session.take_writer(), Err(PtyError::WriterTaken)));
    use std::io::Write;
    writer.write_all(b"do-something\r\n").expect("write failed");
    assert_eq!(probe.written(), b"do-something\r\n");

    // Still running (OnKill): try_wait is None; after kill it reports the
    // scripted status and a late kill is refused.
    assert!(session.try_wait().expect("try_wait failed").is_none());
    let mut killer = session.killer();
    killer.kill().expect("kill failed");
    assert!(probe.was_killed());
    let status = session.wait().expect("wait failed");
    assert_eq!(status.code, 1);
    assert!(!status.success);
    assert!(matches!(killer.kill(), Err(PtyError::AlreadyReaped)));
}
