use chrono::{ Duration as ChronoDuration, Local, NaiveDate };
use slog::{Drain, Duplicate, Logger, o};
use std::{
    collections::VecDeque,
    fs::{ self, File, OpenOptions },
    io::{ self, BufRead, BufReader, Write },
    path::{ Path, PathBuf },
};


const MAX_LOG_FILE_BYTES:u64 =64*1024*1024;
const KEEP_LOG_DAYS:i64=14;
pub struct DailyLog {
    dir: PathBuf,
    name: String,
    date: NaiveDate,
    part: u32,
    written: u64,
    file: File,
    at_line_start: bool,
}
impl DailyLog {
    pub fn new(dir: impl Into<PathBuf>, name: &str) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let date = Local::now().date_naive();
        let (part, written, file) = Self::open_current(&dir, name, date)?;
        prune_old_logs(&dir, name, date);
        Ok(Self { dir, name: name.to_owned(), date, part, written, file, at_line_start: true })
    }

    fn path(dir: &Path, name: &str, date: NaiveDate, part: u32) -> PathBuf {
        let day = date.format("%Y-%m-%d");
        if part == 0 {
            dir.join(format!("{name}-{day}.log"))
        } else {
            dir.join(format!("{name}-{day}.{part}.log"))
        }
    }
    fn open_current(dir: &Path, name: &str, date: NaiveDate) -> io::Result<(u32, u64, File)> {
        let mut part = 0;
        loop {
            let path = Self::path(dir, name, date, part);
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size < MAX_LOG_FILE_BYTES {
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                return Ok((part, size, file));
            }
            part += 1;
        }
    }
}
impl Write for DailyLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.at_line_start {
            let today = Local::now().date_naive();
            let next = if today != self.date {
                Some((today, 0))
            } else if self.written >= MAX_LOG_FILE_BYTES {
                Some((today, self.part + 1))
            } else {
                None
            };
            if let Some((date, part)) = next {
                let opened = if part == 0 {
                    Self::open_current(&self.dir, &self.name, date)
                } else {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(Self::path(&self.dir, &self.name, date, part))
                        .map(|f| (part, 0, f))
                };
                match opened {
                    Ok((part, written, file)) => {
                        let _ = self.file.flush();
                        self.file = file;
                        self.part = part;
                        self.written = written;
                    }
                    Err(e) => eprintln!("cannot open the next log file: {e}; still writing to the previous one"),
                }
                if date != self.date {
                    prune_old_logs(&self.dir, &self.name, date);
                }
                self.date = date;
            }
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        if n > 0 {
            self.at_line_start = buf[..n].ends_with(b"\n");
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
fn timestamp(w: &mut dyn Write) -> io::Result<()> {
    write!(w, "{}", Local::now().format("%Y-%m-%d %H:%M:%S%.3f%:z"))
}
pub fn shard_logger(root: &Path, name: &str) -> Logger {
    match DailyLog::new(root.join("logs"), name) {
        Ok(file) => {
            let drain = slog_term::CompactFormat
                ::new(slog_term::PlainDecorator::new(file))
                .use_custom_timestamp(timestamp)
                .build();
            Logger::root(std::sync::Mutex::new(drain).ignore_res(), o!())
        }
        Err(e) => {
            eprintln!("cannot open log file in {}: {e}; logging to stderr", root.display());
            let drain = slog_term::CompactFormat
                ::new(slog_term::PlainDecorator::new(io::stderr()))
                .use_custom_timestamp(timestamp)
                .build();
            Logger::root(std::sync::Mutex::new(drain).ignore_res(), o!("shard" => name.to_owned()))
        }
    }
}
pub fn program_logger(root: &Path) -> (Logger, slog_async::AsyncGuard) {
    let term_drain = slog_term::CompactFormat
        ::new(slog_term::TermDecorator::new().build())
        .build()
        .ignore_res();

    // a log file that cannot be opened must not stop the server: log to the terminal only
    let drain: Box<dyn Drain<Ok = (), Err = slog::Never> + Send> =
        match DailyLog::new(root.join("logs"), "program") {
            Ok(file) => {
                let file_drain = slog_term::CompactFormat
                    ::new(slog_term::PlainDecorator::new(file))
                    .use_custom_timestamp(timestamp)
                    .build()
                    .ignore_res();
                Box::new(Duplicate::new(file_drain, term_drain).ignore_res())
            }
            Err(e) => {
                eprintln!("cannot open program log in {}: {e}; logging to the terminal only",
                    root.display());
                Box::new(term_drain)
            }
        };

    // a slow disk must not stall request threads: drop lines when the queue is full
    let (drain, guard) = slog_async::Async
        ::new(drain)
        .chan_size(65_536)
        .overflow_strategy(slog_async::OverflowStrategy::DropAndReport)
        .build_with_guard();
    (Logger::root(drain.ignore_res(), o!("component" => "main")), guard)
}
fn prune_old_logs(dir: &Path, name: &str, today: NaiveDate) {
    let cutoff = today - ChronoDuration::days(KEEP_LOG_DAYS);
    let prefix = format!("{name}-");
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else { continue };
        let Some(rest) = file_name.strip_prefix(&prefix) else { continue };
        if !file_name.ends_with(".log") {
            continue;
        }
        let day = rest.get(..10).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
        if day.is_some_and(|d| d < cutoff) {
            let _ = fs::remove_file(entry.path());
        }
    }
}
pub fn read_log_tail(dir: &Path, date: Option<&str>, max_lines: usize) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let is_log = path.is_file() && path.extension().is_some_and(|e| e == "log");
        let wanted = date.is_none_or(|d| {
            path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(d))
        });
        if is_log && wanted {
            files.push(path);
        }
    }
    files.sort();

    let mut tail: VecDeque<String> = VecDeque::new();
    for path in files {
        for line in BufReader::new(File::open(path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if tail.len() == max_lines {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
    Ok(tail.into())
}

pub fn remove_logs(dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "log") {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}