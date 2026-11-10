use ahash::HashSet;
use core_timing::timed;
use std::fs::File;
use std::io::Write;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const MANIFEST_FILE: &str = "manifest.txt";

pub(super) fn manifest_path(root: &Path) -> PathBuf {
    root.join(MANIFEST_FILE)
}
const SEGMENT_PREFIX: &str = "segment-";

/// `segment-17.idx`: the file name of index segment `id`.
pub(super) fn segment_file_name(id: u64) -> String {
    format!("{SEGMENT_PREFIX}{id}.idx")
}

/// The segment a file belongs to: 17 for `segment-17.idx`, `segment-17.3.vec`,
/// `segment-17.3.hnsw` and `segment-17.idx.tmp`. None for anything else.
pub(super) fn segment_id_of(file_name: &str) -> Option<u64> {
    file_name
        .split('.')
        .next()?
        .strip_prefix(SEGMENT_PREFIX)?
        .parse()
        .ok()
}
pub(super) fn remove_leftovers(root: &Path, live: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let live_ids: HashSet<u64> = live
        .iter()
        .filter_map(|path| segment_id_of(path.file_name()?.to_str()?))
        .collect();
    let mut removed = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let orphan = segment_id_of(name).is_some_and(|id| !live_ids.contains(&id));
        if name.ends_with(".tmp") || orphan {
            fs::remove_file(entry.path())?;
            removed.push(entry.path());
        }
    }
    Ok(removed)
}

#[timed(database_lifecycle)]
pub(super) fn read_manifest(root: &Path) -> io::Result<Vec<PathBuf>> {
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
pub(super) fn compact_manifest(root: &Path, live: &[PathBuf]) -> io::Result<()> {
    let mut text = String::new();
    for path in live {
        if let Some(name) = path.file_name() {
            text.push_str(&name.to_string_lossy());
            text.push('\n');
        }
    }
    let path = manifest_path(root);
    let tmp = path.with_extension("txt.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    // the rename must be durable before the merged inputs are deleted
    File::open(root)?.sync_all()
}
#[timed(writing_files)]
pub(super) fn append_segment(root: &Path, segment: &Path) -> io::Result<()> {
    let file_name = segment.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "segment path has no file name")
    })?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(manifest_path(root))?;
    writeln!(file, "{}", file_name.to_string_lossy())?;
    file.sync_data()?;
    // Persists both the new segment's and (first time) the manifest's directory entry.
    File::open(root)?.sync_all()
}
