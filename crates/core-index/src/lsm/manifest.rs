use core_timing::timed;
use std::io::Write;
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

    let text = fs::read_to_string(&path)?;
    let mut live = Vec::new();

    for line in text.lines() {
        if let Some(removed) = line.strip_suffix(".del") {
            live.retain(|name| name != removed);
        } else {
            live.push(line.to_string());
        }
    }


    Ok(live.into_iter().map(|name| root.join(name)).collect())
}
pub fn compact_manifest(root: &Path, live: &[PathBuf]) -> io::Result<()> {
    let mut text = String::new();
    for path in live {
        if let Some(name) = path.file_name() {
            text.push_str(&name.to_string_lossy());
            text.push('\n');
        }
    }
    fs::write(manifest_path(root), text)
}
#[timed(writing_files)]
pub fn append_segment(root: &Path, segment: &Path) -> io::Result<()> {
    let file_name = segment.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "segment path has no file name")
    })?;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(manifest_path(root))?;

    writeln!(file, "{}", file_name.to_string_lossy())
}
#[timed(writing_files)]
pub fn append_removal(root: &Path, segment: &Path) -> io::Result<()> {
    let file_name = segment.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "segment path has no file name")
    })?;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(manifest_path(root))?;

    writeln!(file, "{}.del", file_name.to_string_lossy())
}

// #[timed(writing_files)]
// pub fn write_manifest(root: &Path, segments: &[PathBuf]) -> io::Result<()> {
//     let mut text = String::new();

//     for path in segments {
//         let file_name = path.file_name().ok_or_else(|| {
//             io::Error::new(io::ErrorKind::InvalidData, "segment path has no file name")
//         })?;

//         text.push_str(&file_name.to_string_lossy());
//         text.push('\n');
//     }

//     fs::write(manifest_path(root), text)
// }
