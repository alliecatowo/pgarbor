//! JSON Lines transcript sink for `serve --transcript FILE` (WIRE-12 diagnostics). Fully implemented,
//! no lease. Appends one `pgarbor_wire::transcript::to_jsonl` line per recorded wire message; write
//! errors are ignored (diagnostics must never break serving).
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use pgarbor_wire::transcript::to_jsonl;
use pgarbor_wire::{TranscriptEntry, TranscriptSink};

use crate::ctx::CliError;

pub struct JsonlFileSink {
    file: Mutex<std::fs::File>,
}

impl JsonlFileSink {
    /// Open `path` for appending (created if missing).
    pub fn open(path: &Path) -> Result<JsonlFileSink, CliError> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| CliError::io(&format!("cannot open transcript file {}", path.display()), &e))?;
        Ok(JsonlFileSink { file: Mutex::new(file) })
    }
}

impl TranscriptSink for JsonlFileSink {
    fn record(&self, entry: TranscriptEntry) {
        if let Ok(line) = to_jsonl(&entry) {
            if let Ok(mut f) = self.file.lock() {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}
