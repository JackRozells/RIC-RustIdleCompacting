//! Small file helpers: JSON in and out, and writes that never leave a file half
//! written (a temporary file beside the target, then a rename over it).

use serde_json::Value;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let tmp = path.with_file_name(format!(".{name}.{}.{nonce}.tmp", std::process::id()));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all().ok();
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

pub fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    let mut text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

/// `Ok(None)` when the file is absent; an error when it exists but is not JSON,
/// so a caller never mistakes a file it cannot read for an empty one.
pub fn read_json(path: &Path) -> io::Result<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let trimmed = text.trim_start_matches('\u{feff}');
            if trimmed.trim().is_empty() {
                return Ok(Some(Value::Object(Default::default())));
            }
            serde_json::from_str(trimmed)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Best effort: whatever is there and parses, else nothing.
pub fn read_json_lossy(path: &Path) -> Option<Value> {
    read_json(path).ok().flatten()
}

/// Appends one line to a log file, rolling it to `.1` past `max_bytes`.
pub fn append_log(path: &Path, line: &str, max_bytes: u64) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > max_bytes {
            let _ = fs::rename(path, path.with_extension("log.1"));
        }
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}
