//! PoC 0 — PTY wiring milestone (see `.claude/roadmap/plan.md` §8).
//!
//! Target: spawn `pwsh` via ConPTY (`portable-pty`), forward stdin/stdout,
//! handle resize, and kill the child cleanly. Placeholder until implemented.

fn main() {
    println!("poc0-pty: PTY wiring not implemented yet (PoC 0 placeholder).");
}

#[cfg(test)]
mod tests {
    #[test]
    fn arithmetic_holds() {
        assert_eq!(2 + 2, 4);
    }
}
