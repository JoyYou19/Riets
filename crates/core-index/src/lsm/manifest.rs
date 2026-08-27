use core_timing::timed;

use std::{
    fs, io,
    path::{Path, PathBuf},
};

const MANIFEST_FILE: &str = "manifest.txt";

pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(MANIFEST_FILE)
}

#[timed(database_lifecycle)]
pub fn read_manifest(root: &Path) -> io::Result<Vec<PathBuf>> {
    let path = manifest_path(root);

    if !path.exists() {
        return Ok(Vec::new());
    }

    let text = fs::read_to_string(path)?;

    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| root.join(line.trim()))
        .collect())
}

#[timed(writing_files)]
pub fn write_manifest(root: &Path, segments: &[PathBuf]) -> io::Result<()> {
    let mut text = String::new();

    for path in segments {
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "segment path has no file name")
        })?;

        text.push_str(&file_name.to_string_lossy());
        text.push('\n');
    }

    fs::write(manifest_path(root), text)
}
