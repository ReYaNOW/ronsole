//! Shared helpers for the crate's unit tests.

use crate::color::{Rgb, TermPalette};
use crate::term::{Term, TermConfig, TermIdentity, TermSink};
use std::time::Instant;

/// Records everything a [`Term`] sends to its host during a test.
pub(crate) struct RecordingSink {
    /// One entry per `reply` call.
    pub(crate) calls: Vec<Vec<u8>>,
    /// All `reply` bytes concatenated in call order.
    pub(crate) bytes: Vec<u8>,
    /// Every `title` call, in order.
    pub(crate) titles: Vec<String>,
    /// Fixed time passed to `advance` by [`feed`].
    pub(crate) now: Instant,
}

impl RecordingSink {
    pub(crate) fn new() -> Self {
        Self {
            calls: Vec::new(),
            bytes: Vec::new(),
            titles: Vec::new(),
            now: Instant::now(),
        }
    }
}

impl TermSink for RecordingSink {
    fn reply(&mut self, bytes: &[u8]) {
        self.calls.push(bytes.to_vec());
        self.bytes.extend_from_slice(bytes);
    }

    fn title(&mut self, title: &str) {
        self.titles.push(title.to_owned());
    }

    fn clipboard_set(&mut self, _text: String) {}

    fn working_directory(&mut self, _host: &str, _path: &[u8]) {}
}

/// Feeds `bytes` into `term` with the sink's fixed time.
pub(crate) fn feed(term: &mut Term, sink: &mut RecordingSink, bytes: &[u8]) {
    term.advance(bytes, sink.now, sink);
}

/// Configuration with 10 000 scrollback lines and a fixed dark palette.
pub(crate) fn test_config(cols: usize, rows: usize) -> TermConfig {
    let light = Rgb {
        r: 0xe5,
        g: 0xe5,
        b: 0xe5,
    };
    let dark = Rgb { r: 0, g: 0, b: 0 };
    TermConfig {
        cols,
        rows,
        scrollback_lines: 10_000,
        identity: TermIdentity {
            name: "ronsole-test",
            version: "0",
        },
        palette: TermPalette {
            foreground: light,
            background: dark,
            cursor: light,
            ansi: [dark; 16],
        },
    }
}
