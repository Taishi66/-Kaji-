//! SDD pass state (ADR 2026-08-07 architecture, ADR 2026-08-08 IPC):
//! pure state owned by the core, clients render and trigger transitions.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SddStage {
    Intent,
    Spec,
    Gate,
    Exec,
    Validate,
    DriftLock,
}

impl SddStage {
    pub const ALL: [SddStage; 6] = [
        SddStage::Intent,
        SddStage::Spec,
        SddStage::Gate,
        SddStage::Exec,
        SddStage::Validate,
        SddStage::DriftLock,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            SddStage::Intent => "Intent",
            SddStage::Spec => "SPEC",
            SddStage::Gate => "Gate",
            SddStage::Exec => "Exec",
            SddStage::Validate => "Validate",
            SddStage::DriftLock => "Drift lock",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct SpecDoc {
    pub path: PathBuf,
    pub title: String,
    pub body: String,
}

impl SpecDoc {
    pub fn parse(path: PathBuf, content: &str) -> Self {
        let title = content
            .lines()
            .find_map(|line| line.strip_prefix("# ").map(|t| t.trim().to_string()))
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "SPEC".to_string())
            });
        Self {
            path,
            title,
            body: content.to_string(),
        }
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::parse(path.to_path_buf(), &content))
    }

    pub fn is_empty(&self) -> bool {
        self.body.trim().is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct SddPass {
    statuses: [StageStatus; 6],
    current: Option<usize>,
}

impl SddPass {
    pub fn new() -> Self {
        Self {
            statuses: [StageStatus::Pending; 6],
            current: None,
        }
    }

    pub fn start(&mut self) {
        if self.current.is_none() && self.statuses.iter().all(|s| *s == StageStatus::Pending) {
            self.current = Some(0);
            self.statuses[0] = StageStatus::Running;
        }
    }

    pub fn advance(&mut self) {
        let Some(idx) = self.current else { return };
        self.statuses[idx] = StageStatus::Done;
        let next = idx + 1;
        if next < self.statuses.len() {
            self.current = Some(next);
            self.statuses[next] = StageStatus::Running;
        } else {
            self.current = None;
        }
    }

    pub fn fail_current(&mut self) {
        if let Some(idx) = self.current {
            self.statuses[idx] = StageStatus::Failed;
            self.current = None;
        }
    }

    pub fn current(&self) -> Option<SddStage> {
        self.current.map(|idx| SddStage::ALL[idx])
    }

    pub fn stages(&self) -> [(SddStage, StageStatus); 6] {
        let mut out = [(SddStage::Intent, StageStatus::Pending); 6];
        for (i, stage) in SddStage::ALL.iter().enumerate() {
            out[i] = (*stage, self.statuses[i]);
        }
        out
    }

    pub fn is_running(&self) -> bool {
        self.current.is_some()
    }

    pub fn is_complete(&self) -> bool {
        self.statuses.iter().all(|s| *s == StageStatus::Done)
    }

    pub fn drifted(&self) -> bool {
        self.statuses.contains(&StageStatus::Failed)
    }
}

impl Default for SddPass {
    fn default() -> Self {
        Self::new()
    }
}
