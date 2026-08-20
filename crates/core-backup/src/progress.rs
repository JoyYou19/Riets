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
    fn from_u8(v: u8) -> Self {
        match v {
            1 => BackupPhase::Running,
            2 => BackupPhase::Complete,
            3 => BackupPhase::Failed,
            _ => BackupPhase::Idle,
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
    pub fn percent(&self) -> Option<f64> {
        if self.bytes_total == 0 {
            return None;
        }
        Some(round2(((self.bytes_done as f64) / (self.bytes_total as f64)) * 100.0))
    }

    pub fn mb_done(&self) -> f64 {
        round2((self.bytes_done as f64) / MIB)
    }

    pub fn mb_total(&self) -> f64 {
        round2((self.bytes_total as f64) / MIB)
    }

    pub fn eta_seconds(&self) -> Option<u64> {
        let elapsed_ms = self.elapsed_ms?;
        if elapsed_ms == 0 || self.bytes_done == 0 {
            return None;
        }
        let rate = (self.bytes_done as f64) / ((elapsed_ms as f64) / 1000.0);
        if rate <= 0.0 {
            return None;
        }
        let remaining = self.bytes_total.saturating_sub(self.bytes_done) as f64;
        Some((remaining / rate).round() as u64)
    }
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
        BackupStats {
            status: (
                match s.phase {
                    BackupPhase::Idle => "idle",
                    BackupPhase::Running => "running",
                    BackupPhase::Complete => "complete",
                    BackupPhase::Failed => "failed",
                }
            ).to_string(),
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
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: AtomicU8::new(BackupPhase::Idle as u8),
            bytes_total: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            started_at_ms: AtomicU64::new(0),
        })
    }

    pub fn try_begin(&self) -> bool {
        let won = self.phase
            .fetch_update(Relaxed, Relaxed, |current| {
                if current == (BackupPhase::Running as u8) {
                    None
                } else {
                    Some(BackupPhase::Running as u8)
                }
            })
            .is_ok();
        if won {
            self.bytes_total.store(0, Relaxed);
            self.bytes_done.store(0, Relaxed);
            self.started_at_ms.store(chrono::Utc::now().timestamp_millis() as u64, Relaxed);
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
        self.phase.store(phase as u8, Relaxed);
    }
    pub fn is_running(&self) -> bool {
        self.phase.load(Relaxed) == (BackupPhase::Running as u8)
    }

    pub fn snapshot(&self) -> BackupProgressSnapshot {
        let phase = BackupPhase::from_u8(self.phase.load(Relaxed));
        let elapsed_ms = if phase == BackupPhase::Running {
            let started = self.started_at_ms.load(Relaxed);
            let now = chrono::Utc::now().timestamp_millis() as u64;
            Some(now.saturating_sub(started))
        } else {
            None
        };
        BackupProgressSnapshot {
            phase,
            bytes_total: self.bytes_total.load(Relaxed),
            bytes_done: self.bytes_done.load(Relaxed),
            elapsed_ms,
        }
    }
}
