//! PoC 0 — PTY wiring milestone (see `.claude/roadmap/plan.md` §8).
//!
//! Proves ConPTY works on Windows: spawns a shell (`pwsh` by default) via the
//! [`tessmux_pty::PtyBackend`] trait, forwards raw stdin/stdout, propagates
//! console resizes, and kills the child cleanly on Ctrl+].
//!
//! Usage: `poc0-pty [program [args...]]` — run from a real terminal.
//! Ctrl+C is forwarded to the child (cancels its command); Ctrl+] kills it.

use std::io::{ErrorKind, Read, Write};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::terminal;

use tessmux_pty::{PortablePtyBackend, PtyBackend, PtySize, SpawnCommand, pump_reader};

/// Ctrl+] — telnet-style escape byte: kill the child instead of forwarding.
const KILL_BYTE: u8 = 0x1d;

/// How often the watcher polls the console for size changes. Polling (rather
/// than crossterm's event stream) keeps stdin bytes untouched for the
/// forwarder thread.
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(200);

fn main() -> Result<()> {
    let cmd = match parse_args(std::env::args().skip(1)) {
        Cli::Help => {
            print_usage();
            return Ok(());
        }
        Cli::Version => {
            println!("poc0-pty {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Cli::Run(cmd) => cmd,
    };

    let (cols, rows) = terminal::size().context(
        "failed to query terminal size — run poc0-pty from a real terminal, \
         not a pipe or an IDE debug console",
    )?;
    // Degenerate sizes happen on some headless ptys; fall back to a sane
    // default rather than handing the child a 0-sized terminal.
    let size = if rows == 0 || cols == 0 {
        PtySize { rows: 24, cols: 80 }
    } else {
        PtySize { rows, cols }
    };

    let mut session = PortablePtyBackend.spawn(&cmd, size)?;

    // Acquire every session handle before spawning any thread: a `?` exit
    // past this point would otherwise abandon live threads mid-teardown.
    let reader = session.reader()?;
    let mut writer = session.take_writer()?;
    let mut killer = session.killer();
    let resizer = session.resizer();

    // Raw mode: bytes flow through unmodified — Ctrl+C arrives as 0x03 and is
    // forwarded to the child instead of killing us. The guard restores the
    // console on every exit path, panics included.
    let raw_guard = RawModeGuard::enable()?;

    // The hint goes to stderr (not the passthrough stream) and only after raw
    // mode is on, so its "Ctrl+C goes to the child" promise is already true.
    // Explicit \r\n: raw mode may have disabled output post-processing.
    eprint!(
        "[poc0-pty] spawned '{}' — Ctrl+C goes to the child, Ctrl+] kills it\r\n",
        cmd.program
    );

    // PTY -> our stdout. Joined after close() — the trait's EOF postcondition
    // makes that join deterministic.
    let reader_thread = thread::spawn(move || {
        // Lock stdout once for the thread's lifetime instead of re-acquiring
        // the global lock twice (write + flush) per chunk.
        let mut out = std::io::stdout().lock();
        let mut out_alive = true;
        pump_reader(reader, |chunk| {
            // Keep draining even after our stdout breaks (e.g. a pipe
            // consumer died): an undrained pipe blocks conhost, the child
            // wedges on its next write, and wait() never returns.
            if out_alive {
                out_alive = out.write_all(chunk).and_then(|()| out.flush()).is_ok();
            }
        });
    });

    // Our stdin -> PTY, watching for the kill byte. Not joined: it blocks in
    // stdin.read() until our own process exits.
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                // Our stdin closed: stop forwarding and drop the writer (a
                // true input-pipe EOF for the child on Windows; on Unix only
                // a VEOF byte — see PtySession::writer docs).
                Ok(0) => break,
                Ok(n) => {
                    let (forward, kill) = split_at_kill_byte(&buf[..n]);
                    // Kill must fire even if the forward write fails (a dead
                    // child breaks the pipe but the user still asked to kill).
                    let write_failed = !forward.is_empty()
                        && writer
                            .write_all(forward)
                            .and_then(|()| writer.flush())
                            .is_err();
                    if kill {
                        let _ = killer.kill();
                        break;
                    }
                    if write_failed {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue, // EINTR — retry
                Err(_) => break,
            }
        }
    });

    // Console size -> PTY size. Not joined: sleeps forever; dies with us.
    thread::spawn(move || {
        let mut last = (cols, rows);
        loop {
            thread::sleep(RESIZE_POLL_INTERVAL);
            let Ok(now) = terminal::size() else { continue };
            if now != last {
                last = now;
                let _ = resizer.resize(PtySize {
                    rows: now.1,
                    cols: now.0,
                });
            }
        }
    });

    // Run teardown before propagating a wait() error so the reader thread is
    // never abandoned and the console is always restored in order. close()
    // guarantees the reader reaches EOF, so the join cannot hang. Ordering is
    // load-bearing: close() must run BEFORE the join and never on the reader
    // thread itself — pre-24H2 Windows ClosePseudoConsole blocks until
    // clients disconnect, which needs the reader still draining elsewhere.
    let wait_result = session.wait();
    let _ = session.close();
    let _ = reader_thread.join();
    drop(raw_guard); // restore the console before printing our own line

    let status = wait_result?;
    let verdict = if status.success {
        "success"
    } else {
        "abnormal"
    };
    println!(
        "[poc0-pty] child exited with code {} ({verdict})",
        status.code
    );
    Ok(())
}

/// Splits an input chunk at the first [`KILL_BYTE`]: returns the bytes to
/// forward to the child and whether the kill byte was present. Bytes after
/// the kill byte are deliberately discarded — the session is going down.
fn split_at_kill_byte(chunk: &[u8]) -> (&[u8], bool) {
    match chunk.iter().position(|&b| b == KILL_BYTE) {
        Some(pos) => (&chunk[..pos], true),
        None => (chunk, false),
    }
}

enum Cli {
    Run(SpawnCommand),
    Help,
    Version,
}

/// `poc0-pty [program [args...]]`; defaults to an interactive `pwsh`.
/// `--help`/`--version` are intercepted — everything else is a program name,
/// so they must never reach the spawn path.
fn parse_args(mut args: impl Iterator<Item = String>) -> Cli {
    let first = args.next();
    match first.as_deref() {
        Some("--help" | "-h") => Cli::Help,
        Some("--version" | "-V") => Cli::Version,
        Some(program) => Cli::Run(SpawnCommand {
            program: program.into(),
            args: args.collect(),
            ..Default::default()
        }),
        None => Cli::Run(SpawnCommand {
            program: "pwsh".into(),
            ..Default::default()
        }),
    }
}

fn print_usage() {
    println!(
        "poc0-pty [program [args...]]   (default: pwsh)\n\
         \n\
         PoC 0 of tessmux: runs the program in a fresh ConPTY/PTY session and\n\
         wires it to the current terminal (raw I/O, resize propagation).\n\
         \n\
         Keys:\n\
         \x20 Ctrl+C  forwarded to the child (cancels its command)\n\
         \x20 Ctrl+]  kills the child\n\
         \n\
         If the child survives Ctrl+] (a Unix child ignoring SIGHUP), terminate\n\
         poc0-pty from another terminal."
    );
}

/// RAII wrapper around crossterm raw mode so the console is restored even if
/// we panic mid-session.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode().context("failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, parse_args, split_at_kill_byte};
    use tessmux_pty::SpawnCommand;

    fn run_command(args: &[&str]) -> SpawnCommand {
        match parse_args(args.iter().map(ToString::to_string)) {
            Cli::Run(cmd) => cmd,
            other => panic!(
                "expected Cli::Run, got Help/Version variant: {}",
                matches!(other, Cli::Help) as u8
            ),
        }
    }

    #[test]
    fn no_args_defaults_to_pwsh() {
        let cmd = run_command(&[]);
        assert_eq!(cmd.program, "pwsh");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn args_override_program_and_args() {
        let cmd = run_command(&["cmd", "/c", "dir"]);
        assert_eq!(cmd.program, "cmd");
        assert_eq!(cmd.args, vec!["/c", "dir"]);
    }

    #[test]
    fn help_and_version_are_intercepted() {
        assert!(matches!(
            parse_args(["--help".to_string()].into_iter()),
            Cli::Help
        ));
        assert!(matches!(
            parse_args(["-h".to_string()].into_iter()),
            Cli::Help
        ));
        assert!(matches!(
            parse_args(["--version".to_string()].into_iter()),
            Cli::Version
        ));
        assert!(matches!(
            parse_args(["-V".to_string()].into_iter()),
            Cli::Version
        ));
    }

    #[test]
    fn plain_chunk_forwards_everything() {
        assert_eq!(split_at_kill_byte(b"hello"), (&b"hello"[..], false));
    }

    #[test]
    fn kill_byte_forwards_prefix_and_signals_kill() {
        assert_eq!(split_at_kill_byte(b"abc\x1ddef"), (&b"abc"[..], true));
    }

    #[test]
    fn leading_kill_byte_forwards_nothing() {
        assert_eq!(split_at_kill_byte(b"\x1d"), (&b""[..], true));
    }
}
