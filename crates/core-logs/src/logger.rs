use chrono::{Local, NaiveDate};
use slog::{Drain, Duplicate, Logger, o};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

pub struct DailyLog {
    dir: PathBuf,
    name: String,
    date: NaiveDate,
    file: File,
    at_line_start: bool,
}
impl DailyLog {
    pub fn new(dir: impl Into<PathBuf>, name: &str) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let date = Local::now().date_naive();
        let file = Self::open(&dir, name, date)?;
        Ok(Self {
            dir,
            name: name.to_owned(),
            date,
            file,
            at_line_start: true,
        })
    }
    pub fn open(dir: &Path, name: &str, date: NaiveDate) -> io::Result<File> {
        let path = dir.join(format!("{name}-{}.log", date.format("%Y-%m-%d")));
        OpenOptions::new().create(true).append(true).open(path)
    }
}
impl Write for DailyLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.at_line_start {
            let today = Local::now().date_naive();
            if today != self.date {
                self.file.flush()?;
                self.file = Self::open(&self.dir, &self.name, today)?;
                self.date = today;
            }
        }
        let n = self.file.write(buf)?;
        if n > 0 {
            self.at_line_start = buf[..n].ends_with(b"\n");
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
fn timestamp(w: &mut dyn io::Write) -> io::Result<()> {
    write!(w, "{}", Local::now().format("%Y-%m-%d %H:%M:%S%.3f%:z"))
}
pub fn shard_logger(root: &Path, name: &str) -> Logger {
    let file = DailyLog::new(root.join("logs"), name)
        .unwrap_or_else(|e| panic!("Failed to open log file:{e}"));
    let drain = slog_term::CompactFormat::new(slog_term::PlainDecorator::new(file))
        .use_custom_timestamp(timestamp)
        .build();
    let drain = std::sync::Mutex::new(drain).fuse();
    Logger::root(drain, o!())
}

pub fn program_logger(root: &Path) -> (Logger, slog_async::AsyncGuard) {
    let file =
        DailyLog::new(root.join("logs"), "program").unwrap_or_else(|e| panic!("Failed to open log file: {e}"));
    let file_drain = slog_term::CompactFormat::new(slog_term::PlainDecorator::new(file))
        .use_custom_timestamp(timestamp)
        .build()
        .fuse();
    let term_drain = slog_term::CompactFormat::new(slog_term::TermDecorator::new().build())
        .build()
        .fuse();
    let both = Duplicate::new(file_drain, term_drain).fuse();
    let (drain, guard) = slog_async::Async::new(both)
        .chan_size(65_536)
        .overflow_strategy(slog_async::OverflowStrategy::Block)
        .build_with_guard();
    (Logger::root(drain.fuse(), o!("component" => "main")), guard)
}

