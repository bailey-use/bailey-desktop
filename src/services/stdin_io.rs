//! Shared stdin helpers for one-shot CLI commands.

use std::io::{self, IsTerminal, Read};

use anyhow::Result;

/// Reads all of stdin when it's piped (non-TTY) and returns the contents.
///
/// Returns `Ok(None)` when stdin is a TTY (interactive shell) or when the
/// pipe contained only whitespace — both of which mean the caller should
/// fall back to its non-stdin behavior (help, picker, etc.) rather than
/// firing off work for a misfired empty pipe.
pub fn read_stdin_if_piped() -> Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(buf))
    }
}
