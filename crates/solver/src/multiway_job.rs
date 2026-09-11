//! Persistent job orchestration for the sampled multiway Hold'em batch solver.
//!
//! The job store deliberately persists only JSON checkpoints and a small
//! manifest. The public tree and ranges remain caller-owned inputs and are
//! validated again by `MultiwayHoldemBatchSolver::from_checkpoint` on resume.
//! Checkpoint files are rotated deterministically, while manifest writes use a
//! temporary file followed by a rename so an interrupted write does not leave
//! a partially written JSON document at the canonical path.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use holdem_cards::DeckMask;
use holdem_ranges::WeightedRange;
use holdem_tree::GameTree;
use serde::{Deserialize, Serialize};

use crate::{MultiwayHoldemBatchCheckpoint, MultiwayHoldemBatchSolver};

pub const MULTIWAY_BATCH_JOB_FORMAT_VERSION: u32 = 1;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiwayBatchJobConfig {
    /// Total solver iterations desired, not additional iterations.
    pub target_iterations: u64,
    pub worker_count: usize,
    /// Number of stale-strategy iterations reduced before the next ordered
    /// worker reduction. A value of one is the smallest parallel batch.
    pub reduction_batch_size: usize,
    /// Checkpoint after this many solver iterations have completed.
    pub checkpoint_interval: u64,
    pub max_private_attempts: usize,
    /// Number of newest checkpoint files retained by the store.
    pub keep_checkpoints: usize,
}

impl MultiwayBatchJobConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.target_iterations == 0 {
            return Err("multiway job target_iterations must be positive".to_string());
        }
        if self.worker_count == 0 {
            return Err("multiway job worker_count must be positive".to_string());
        }
        if self.reduction_batch_size == 0 {
            return Err("multiway job reduction_batch_size must be positive".to_string());
        }
        if self.checkpoint_interval == 0 {
            return Err("multiway job checkpoint_interval must be positive".to_string());
        }
        if self.max_private_attempts == 0 {
            return Err("multiway job max_private_attempts must be positive".to_string());
        }
        if self.keep_checkpoints == 0 {
            return Err("multiway job keep_checkpoints must be positive".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MultiwayBatchJobStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiwayBatchJobManifest {
    pub format_version: u32,
    pub job_id: String,
    pub status: MultiwayBatchJobStatus,
    pub config: MultiwayBatchJobConfig,
    pub player_count: usize,
    pub tree_fingerprint: u64,
    pub range_fingerprint: u64,
    pub dead_cards: DeckMask,
    pub config_fingerprint: u64,
    pub completed_iterations: u64,
    pub checkpoint_sequence: u64,
    pub latest_checkpoint: Option<String>,
    pub error: Option<String>,
}

impl MultiwayBatchJobManifest {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != MULTIWAY_BATCH_JOB_FORMAT_VERSION {
            return Err(format!(
                "unsupported multiway batch job format version: {}",
                self.format_version
            ));
        }
        if self.job_id.trim().is_empty() {
            return Err("multiway batch job id cannot be empty".to_string());
        }
        self.config.validate()?;
        if !(3..=8).contains(&self.player_count) {
            return Err("multiway batch job player count must be 3-8".to_string());
        }
        if self.completed_iterations > self.config.target_iterations {
            return Err("multiway batch job completed iterations exceed target".to_string());
        }
        if self.checkpoint_sequence == 0 || self.latest_checkpoint.is_none() {
            return Err("multiway batch job has no latest checkpoint".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MultiwayBatchJobStore {
    directory: PathBuf,
}

impl MultiwayBatchJobStore {
    pub fn new<P: Into<PathBuf>>(directory: P) -> Result<Self, String> {
        let directory = directory.into();
        fs::create_dir_all(directory.join("checkpoints"))
            .map_err(|error| format!("create multiway job directory: {error}"))?;
        Ok(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.directory.join("manifest.json")
    }

    pub fn checkpoints_directory(&self) -> PathBuf {
        self.directory.join("checkpoints")
    }

    pub fn load_manifest(&self) -> Result<MultiwayBatchJobManifest, String> {
        let json = fs::read_to_string(self.manifest_path())
            .map_err(|error| format!("read multiway job manifest: {error}"))?;
        let manifest = MultiwayBatchJobManifest::from_json(&json)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load_latest_checkpoint(&self) -> Result<MultiwayHoldemBatchCheckpoint, String> {
        let manifest = self.load_manifest()?;
        let filename = manifest
            .latest_checkpoint
            .ok_or_else(|| "multiway job manifest has no checkpoint filename".to_string())?;
        let path = self.checkpoints_directory().join(filename);
        let json = fs::read_to_string(path)
            .map_err(|error| format!("read latest multiway job checkpoint: {error}"))?;
        let checkpoint = MultiwayHoldemBatchCheckpoint::from_json(&json)?;
        checkpoint.validate()?;
        if checkpoint.iterations != manifest.completed_iterations {
            return Err(
                "latest checkpoint iteration does not match multiway job manifest".to_string(),
            );
        }
        Ok(checkpoint)
    }

    /// Restores a solver from the newest durable checkpoint. The tree, ranges,
    /// dead cards and configuration fingerprint are deliberately supplied by
    /// the caller and revalidated against the checkpoint context.
    pub fn resume_solver(
        &self,
        tree: GameTree,
        ranges: Vec<WeightedRange>,
        dead_cards: DeckMask,
        config_fingerprint: u64,
    ) -> Result<MultiwayHoldemBatchSolver, String> {
        let manifest = self.load_manifest()?;
        let checkpoint = self.load_latest_checkpoint()?;
        MultiwayHoldemBatchSolver::from_checkpoint(
            tree,
            ranges,
            dead_cards,
            &checkpoint,
            config_fingerprint,
            manifest.config.max_private_attempts,
        )
    }

    /// Runs a job until its configured total target and checkpoints after each
    /// interval. Existing jobs must be resumed from their latest checkpoint;
    /// passing a fresh solver with a mismatching iteration count is rejected.
    pub fn run_to_target(
        &self,
        job_id: &str,
        solver: &mut MultiwayHoldemBatchSolver,
        config: &MultiwayBatchJobConfig,
    ) -> Result<MultiwayBatchJobManifest, String> {
        self.run_to_target_with_control(job_id, solver, config, || true)
    }

    /// Runs a job with a cooperative cancellation hook. The hook is checked
    /// before each solver batch and after each durable checkpoint, so a worker
    /// never has to interrupt a solver thread in the middle of a reduction.
    pub fn run_to_target_with_control<F>(
        &self,
        job_id: &str,
        solver: &mut MultiwayHoldemBatchSolver,
        config: &MultiwayBatchJobConfig,
        mut should_continue: F,
    ) -> Result<MultiwayBatchJobManifest, String>
    where
        F: FnMut() -> bool,
    {
        config.validate()?;
        if job_id.trim().is_empty() {
            return Err("multiway batch job id cannot be empty".to_string());
        }
        if solver.iterations() > config.target_iterations {
            return Err("solver is already past the multiway job target".to_string());
        }

        let mut manifest = if self.manifest_path().exists() {
            let mut existing = self.load_manifest()?;
            validate_existing_job(&existing, job_id, config, solver)?;
            // The target is a resumable total, so extending it is allowed. The
            // other execution settings remain immutable for this job.
            existing.config.target_iterations = config.target_iterations;
            existing
        } else {
            let checkpoint = solver.checkpoint();
            if checkpoint.max_private_attempts != config.max_private_attempts {
                return Err(
                    "solver max_private_attempts does not match multiway job config".to_string(),
                );
            }
            let mut created = manifest_from_checkpoint(job_id, config.clone(), &checkpoint);
            created.status = MultiwayBatchJobStatus::Running;
            let (sequence, filename) =
                self.persist_checkpoint(&checkpoint, 1, config.keep_checkpoints)?;
            created.checkpoint_sequence = sequence;
            created.latest_checkpoint = Some(filename);
            self.persist_manifest(&created)?;
            created
        };

        manifest.status = MultiwayBatchJobStatus::Running;
        manifest.error = None;
        self.persist_manifest(&manifest)?;

        if !should_continue() {
            manifest.status = MultiwayBatchJobStatus::Cancelled;
            self.persist_manifest(&manifest)?;
            return Ok(manifest);
        }

        while solver.iterations() < config.target_iterations {
            if !should_continue() {
                manifest.status = MultiwayBatchJobStatus::Cancelled;
                self.persist_manifest(&manifest)?;
                return Ok(manifest);
            }
            let remaining = config.target_iterations - solver.iterations();
            let step = remaining.min(config.checkpoint_interval);
            if let Err(error) =
                solver.run_parallel(step, config.worker_count, config.reduction_batch_size)
            {
                manifest.status = MultiwayBatchJobStatus::Failed;
                manifest.completed_iterations = solver.iterations();
                manifest.error = Some(error.clone());
                let _ = self.persist_manifest(&manifest);
                return Err(error);
            }

            let checkpoint = solver.checkpoint();
            let next_sequence = manifest
                .checkpoint_sequence
                .checked_add(1)
                .ok_or_else(|| "multiway job checkpoint sequence overflowed".to_string())?;
            let (sequence, filename) =
                self.persist_checkpoint(&checkpoint, next_sequence, config.keep_checkpoints)?;
            manifest.checkpoint_sequence = sequence;
            manifest.latest_checkpoint = Some(filename);
            manifest.completed_iterations = checkpoint.iterations;
            self.persist_manifest(&manifest)?;
            if !should_continue() {
                manifest.status = MultiwayBatchJobStatus::Cancelled;
                self.persist_manifest(&manifest)?;
                return Ok(manifest);
            }
        }

        manifest.status = MultiwayBatchJobStatus::Completed;
        manifest.completed_iterations = solver.iterations();
        manifest.error = None;
        self.persist_manifest(&manifest)?;
        Ok(manifest)
    }

    fn persist_checkpoint(
        &self,
        checkpoint: &MultiwayHoldemBatchCheckpoint,
        sequence: u64,
        keep_checkpoints: usize,
    ) -> Result<(u64, String), String> {
        checkpoint.validate()?;
        let filename = format!("checkpoint-{sequence:020}.json");
        let path = self.checkpoints_directory().join(&filename);
        let json = checkpoint.to_json()?;
        write_text_atomically(&path, &json)?;

        let mut files = checkpoint_files(&self.checkpoints_directory())?;
        while files.len() > keep_checkpoints {
            let oldest = files.remove(0);
            fs::remove_file(oldest)
                .map_err(|error| format!("rotate multiway job checkpoint: {error}"))?;
        }
        Ok((sequence, filename))
    }

    fn persist_manifest(&self, manifest: &MultiwayBatchJobManifest) -> Result<(), String> {
        manifest.validate()?;
        let json = manifest.to_json()?;
        write_text_atomically(&self.manifest_path(), &json)
    }
}

fn manifest_from_checkpoint(
    job_id: &str,
    config: MultiwayBatchJobConfig,
    checkpoint: &MultiwayHoldemBatchCheckpoint,
) -> MultiwayBatchJobManifest {
    MultiwayBatchJobManifest {
        format_version: MULTIWAY_BATCH_JOB_FORMAT_VERSION,
        job_id: job_id.to_string(),
        status: MultiwayBatchJobStatus::Running,
        config,
        player_count: checkpoint.player_count,
        tree_fingerprint: checkpoint.tree_fingerprint,
        range_fingerprint: checkpoint.range_fingerprint,
        dead_cards: checkpoint.dead_cards,
        config_fingerprint: checkpoint.config_fingerprint,
        completed_iterations: checkpoint.iterations,
        checkpoint_sequence: 0,
        latest_checkpoint: None,
        error: None,
    }
}

fn validate_existing_job(
    manifest: &MultiwayBatchJobManifest,
    job_id: &str,
    config: &MultiwayBatchJobConfig,
    solver: &MultiwayHoldemBatchSolver,
) -> Result<(), String> {
    if manifest.job_id != job_id {
        return Err("multiway job id does not match existing manifest".to_string());
    }
    let mut expected_config = manifest.config.clone();
    expected_config.target_iterations = config.target_iterations;
    if expected_config != *config {
        return Err("multiway job config does not match existing manifest (only target_iterations may change on resume)".to_string());
    }
    if manifest.completed_iterations != solver.iterations() {
        return Err(
            "solver iteration count does not match existing multiway job checkpoint".to_string(),
        );
    }
    let checkpoint = solver.checkpoint();
    if checkpoint.player_count != manifest.player_count
        || checkpoint.tree_fingerprint != manifest.tree_fingerprint
        || checkpoint.range_fingerprint != manifest.range_fingerprint
        || checkpoint.dead_cards != manifest.dead_cards
        || checkpoint.config_fingerprint != manifest.config_fingerprint
    {
        return Err("solver context does not match existing multiway job manifest".to_string());
    }
    Ok(())
}

fn checkpoint_files(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = fs::read_dir(directory)
        .map_err(|error| format!("list multiway job checkpoints: {error}"))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("checkpoint-") && name.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn write_text_atomically(path: &Path, text: &str) -> Result<(), String> {
    let temporary = unique_temporary_path(path);
    fs::write(&temporary, text).map_err(|error| format!("write temporary job file: {error}"))?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_first_error) if path.exists() => {
            fs::remove_file(path).map_err(|error| format!("replace existing job file: {error}"))?;
            fs::rename(&temporary, path)
                .map_err(|error| format!("commit job file after replacement: {error}"))
        }
        Err(error) => Err(format!("commit job file: {error}")),
    }
}

fn unique_temporary_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("job");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{name}.tmp-{}-{timestamp}-{counter}",
        std::process::id()
    ))
}
