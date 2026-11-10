use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

pub mod analyzer;
pub mod array_rows;
pub mod background;
pub mod bkd;
pub mod disk;
pub mod document;
pub mod document_values;
pub mod fuzzy;
pub mod lsm;
pub mod mem;
pub mod numeric_values;
pub mod posting;
pub mod search;
pub mod segment;
pub mod term_dict;
pub mod types;
pub mod vector;
pub mod wildcard;

pub fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let written = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}
pub fn tmp_path_for(path: &Path) -> PathBuf {
    with_suffix(path, ".tmp")
}
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
pub mod hash;
