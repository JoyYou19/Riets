use std::sync::Arc;
use std::sync::atomic::{ AtomicU8, AtomicU64, Ordering::Relaxed };

const MIB: f64 = 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BackupPhase {
    Idle = 0,
    Running = 1,
    Complete = 2,
    Failed = 3,
}

impl BackupPhase {
    #[allow(clippy::as_conversions, reason = "fieldless #[repr(u8)] enum; lossless")]
    const fn to_u8(self) -> u8 {
        self as u8
    }
     /// Inverse of `to_u8`; `None` for a value that is not a phase.
    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Idle),
            1 => Some(Self::Running),
            2 => Some(Self::Complete),
            3 => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BackupProgressSnapshot {
    pub phase: BackupPhase,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub elapsed_ms: Option<u64>,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

impl BackupProgressSnapshot {
    #[must_use]
    pub fn percent(&self) -> Option<f64> {
        if self.bytes_total == 0 {
            return None;
        }
        let done = u128::from(self.bytes_done.min(self.bytes_total));
        let total = u128::from(self.bytes_total);
        let hundredths = (done * 10_000 + total / 2) / total; // rounded to the nearest
        let hundredths = u32::try_from(hundredths).unwrap_or(10_000);
        Some(f64::from(hundredths) / 100.0)
    }

     #[must_use]
     pub fn mb_done(&self) -> f64 {
        round2(mib(self.bytes_done))
    }

    #[must_use]
    pub fn mb_total(&self) -> f64 {
        round2(mib(self.bytes_total))
    }

    
    #[must_use]
    pub fn eta_seconds(&self) -> Option<u64> {
        let elapsed_ms = self.elapsed_ms?;
        if elapsed_ms == 0 || self.bytes_done == 0 {
            return None;
        }
        // remaining / (done per second) = remaining * elapsed_ms / (done * 1000), rounded
        let remaining = u128::from(self.bytes_total.saturating_sub(self.bytes_done));
        let per = u128::from(self.bytes_done) * 1000;
        let eta = (remaining * u128::from(elapsed_ms) + per / 2) / per;
        Some(u64::try_from(eta).unwrap_or(u64::MAX))
    }
}
/// Bytes as MiB for display; the caller rounds it to two decimals.
   #[allow(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "a display value: rounding a large byte count is harmless"
)]
fn mib(bytes: u64) -> f64 {
    (bytes as f64) / MIB
}
#[derive(Debug, Clone, PartialEq)]
pub struct BackupStats {
    pub status: String,
    pub progress_percent: Option<f64>,
    pub mb_done: f64,
    pub mb_total: f64,
    pub eta_seconds: Option<u64>,
}

impl From<BackupProgressSnapshot> for BackupStats {
    fn from(s: BackupProgressSnapshot) -> Self {
        Self {
            status: (
                match s.phase {
                    BackupPhase::Idle => "idle",
                    BackupPhase::Running => "running",
                    BackupPhase::Complete => "complete",
                    BackupPhase::Failed => "failed",
                }
            ).to_owned(),
            progress_percent: s.percent(),
            mb_done: s.mb_done(),
            mb_total: s.mb_total(),
            eta_seconds: s.eta_seconds(),
        }
    }
}

#[derive(Debug)]
pub struct BackupProgress {
    phase: AtomicU8,
    bytes_total: AtomicU64,
    bytes_done: AtomicU64,
    started_at_ms: AtomicU64,
}

impl BackupProgress {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: AtomicU8::new(BackupPhase::Idle.to_u8()),
            bytes_total: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            started_at_ms: AtomicU64::new(0),
        })
    }

    pub fn try_begin(&self) -> bool {
        let won = self.phase
            .try_update(Relaxed, Relaxed, |current| {
                if current == (BackupPhase::Running.to_u8()) {
                    None
                } else {
                    Some(BackupPhase::Running.to_u8())
                }
            })
            .is_ok();
        if won {
            self.bytes_total.store(0, Relaxed);
            self.bytes_done.store(0, Relaxed);
            self.started_at_ms.store(chrono::Utc::now().timestamp_millis().cast_unsigned(), Relaxed);
        }
        won
    }

    pub fn grow_total(&self, bytes: u64) {
        self.bytes_total.fetch_add(bytes, Relaxed);
    }

    pub fn add(&self, bytes: u64) {
        self.bytes_done.fetch_add(bytes, Relaxed);
    }

    pub fn set_phase(&self, phase: BackupPhase) {
        self.phase.store(phase.to_u8(), Relaxed);
    }
    pub fn is_running(&self) -> bool {
        self.phase.load(Relaxed) == (BackupPhase::Running.to_u8())
    }

    pub fn snapshot(&self) -> BackupProgressSnapshot {
       let phase = BackupPhase::from_u8(self.phase.load(Relaxed)).unwrap_or(BackupPhase::Failed);
        let elapsed_ms = (phase == BackupPhase::Running).then(|| {
            let started = self.started_at_ms.load(Relaxed);
            let now = chrono::Utc::now().timestamp_millis().cast_unsigned();
            now.saturating_sub(started)
        });
        BackupProgressSnapshot {
            phase,
            bytes_total: self.bytes_total.load(Relaxed),
            bytes_done: self.bytes_done.load(Relaxed),
            elapsed_ms,
        }
    }
}
