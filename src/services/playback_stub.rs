//! Stub of `services::playback` for builds without the `audio_playback`
//! feature. Mirrors the real module's public surface so callers compile
//! unchanged; every entry point either returns `None` (probe) or a friendly
//! error explaining how to get a build with playback (`cargo install
//! --git ... --features audio_playback`).
//!
//! Used by published `aivo-linux-*` artifacts so the binary doesn't
//! hard-link `libasound.so.2` and can run on minimal / headless servers.

use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct PlaybackOutcome {
    pub completed: bool,
    pub last_pos: Duration,
}

pub fn probe_duration(_path: &Path) -> Option<Duration> {
    None
}

pub fn play_interactive(_path: &Path, _start_at: Duration) -> Result<PlaybackOutcome> {
    bail!(unsupported_message());
}

pub fn run_streaming_playback(chunk_rx: Receiver<Vec<u8>>) -> Result<PlaybackOutcome> {
    // Drain the producer so the upstream task doesn't block forever on a
    // full channel — but we have no audio device to play to.
    while chunk_rx.recv().is_ok() {}
    bail!(unsupported_message());
}

fn unsupported_message() -> &'static str {
    "audio playback is not built into this binary. Rebuild from source with \
     `cargo install --git https://github.com/yuanchuan/aivo --features audio_playback` \
     to enable in-process playback. The published `aivo-linux-*` artifacts \
     ship without it so the binary doesn't depend on libasound at runtime."
}
