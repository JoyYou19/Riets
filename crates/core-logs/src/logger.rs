use std::path::Path;
use slog::{Drain, Duplicate, Logger, error, o};

pub fn db_logger(root: &Path, name: &str) -> Logger {
    let log_dir = root.join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let file = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open(log_dir.join(format!("{name}.log"))).unwrap();
    let drain = slog_term::FullFormat::new(slog_term::PlainDecorator::new(file))
        .build();
    let drain = std::sync::Mutex::new(drain).fuse();
    Logger::root(drain, o!("db" => name.to_string()))
}

pub fn program_logger() -> (Logger, slog_async::AsyncGuard) {
    std::fs::create_dir_all("logs").unwrap();
    let file = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open("logs/program.log").unwrap();
    let file_drain = slog_term::FullFormat::new(slog_term::PlainDecorator::new(file))
        .build().fuse();
    let term_drain = slog_term::FullFormat::new(slog_term::TermDecorator::new().build())
        .build().fuse();
    let both = Duplicate::new(file_drain, term_drain).fuse();
    let (drain, guard) = slog_async::Async::new(both)
        .chan_size(65_536)
        .overflow_strategy(slog_async::OverflowStrategy::Block)
        .build_with_guard();
    (Logger::root(drain.fuse(), o!("component" => "main")), guard)
}