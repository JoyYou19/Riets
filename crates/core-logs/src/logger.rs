use slog::{Drain, Duplicate, Logger, o};


pub fn program_logger() -> Logger {
    std::fs::create_dir_all("logs").unwrap();

    // file drain
    let file = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open("logs/program.log").unwrap();
    let file_drain = slog_term::FullFormat::new(slog_term::PlainDecorator::new(file))
        .build().fuse();

    // terminal drain with colors
    let term_drain = slog_term::FullFormat::new(slog_term::TermDecorator::new().build())
        .build().fuse();

    // merge both into one async drain
    let both = Duplicate::new(file_drain, term_drain).fuse();
    let drain = slog_async::Async::new(both)
        .chan_size(65_536)
        .overflow_strategy(slog_async::OverflowStrategy::Block)
        .build().fuse();

    Logger::root(drain, o!("component" => "main"))
}

pub fn db_logger(name: &str) -> Logger {
    std::fs::create_dir_all("logs").unwrap();
    let file = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open(format!("logs/{name}.log")).unwrap();
    let drain = slog_async::Async::new(
        slog_term::FullFormat::new(slog_term::PlainDecorator::new(file))
            .build().fuse()
    ).build().fuse();
    Logger::root(drain, o!("db" => name.to_string()))
}