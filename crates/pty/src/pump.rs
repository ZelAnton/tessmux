//! The one shared PTY-output read loop.
//!
//! Every consumer of a session reader needs the same skeleton: fixed buffer,
//! retry on EINTR, stop on EOF/error. Hand-rolling it per consumer is how the
//! copies drift (the EINTR fix originally had to land in three places).
//! Deliberately a plain callback, not a channel: each consumer decides
//! whether to write inline, send over a channel, or feed a terminal model —
//! pump ownership and back-pressure policy stay open until L3 needs them.

use std::io::{ErrorKind, Read};

/// Reads `reader` to EOF, invoking `on_chunk` for every chunk. Retries
/// transient interruptions (EINTR); returns on EOF or any other read error.
///
/// Pair with [`crate::PtySession::close`]: the reader reaches EOF only once
/// the session is closed, so close before joining a thread running this.
pub fn pump_reader<R: Read, F: FnMut(&[u8])>(mut reader: R, mut on_chunk: F) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF — session closed
            Ok(n) => on_chunk(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue, // EINTR — retry
            Err(_) => break,
        }
    }
}
