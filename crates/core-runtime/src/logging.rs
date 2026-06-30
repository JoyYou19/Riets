use tracing_appender::non_blocking::WorkerGuard;

/// Hardcoded to ./logs.txt for now per your TODO — swap for rolling/structured later.
/// Caller must keep the returned guard alive for the program's lifetime (don't drop it).
pub fn init_tracing() -> WorkerGuard {
    let file_appender = tracing_appender::rolling::never(".", "logs.txt");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false)
        .init();

    guard
}
