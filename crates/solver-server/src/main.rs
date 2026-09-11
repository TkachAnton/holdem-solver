use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use holdem_solver_core::{
    MultiwayBatchJobConfig, MultiwayBatchJobStore, MultiwayHoldemSpotJob,
    MultiwayHoldemSpotTreeConfig,
};
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_BIND: &str = "0.0.0.0:8080";
const DEFAULT_DATA_DIR: &str = "holdem-server-data";
const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_TREE_NODES: usize = 100_000;
const DEFAULT_MAX_ACTIVE_JOBS: usize = 2;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const FRONTEND_HTML: &str = include_str!("../../../frontend/index.html");
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct ServerOptions {
    bind: String,
    data_dir: PathBuf,
    max_body_bytes: usize,
    max_tree_nodes: usize,
    max_active_jobs: usize,
}

#[derive(Debug)]
struct ServerState {
    options: ServerOptions,
    active_jobs: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(400, code, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "internal_error", message)
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    target: String,
    body: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct SolveRequest {
    job: MultiwayHoldemSpotJob,
    job_id: Option<String>,
    target_iterations: Option<u64>,
    utility_samples: Option<usize>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let options = parse_args()?;
    fs::create_dir_all(options.data_dir.join("jobs"))
        .map_err(|error| format!("create jobs data directory: {error}"))?;
    fs::create_dir_all(options.data_dir.join("results"))
        .map_err(|error| format!("create results data directory: {error}"))?;

    let listener = TcpListener::bind(&options.bind)
        .map_err(|error| format!("bind {}: {error}", options.bind))?;
    eprintln!(
        "holdem-solver-server listening on {} (data_dir={}, max_body_bytes={}, max_tree_nodes={}, max_active_jobs={})",
        options.bind,
        options.data_dir.display(),
        options.max_body_bytes,
        options.max_tree_nodes,
        options.max_active_jobs
    );
    let state = Arc::new(ServerState {
        options,
        active_jobs: Mutex::new(HashMap::new()),
    });

    for incoming in listener.incoming() {
        match incoming {
            Ok(mut stream) => {
                if let Err(error) = handle_connection(&mut stream, &state) {
                    let _ = write_json_response(&mut stream, error.status, error_json(&error));
                }
            }
            Err(error) => eprintln!("accept connection: {error}"),
        }
    }
    Ok(())
}

fn parse_args() -> Result<ServerOptions, String> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let mut bind = DEFAULT_BIND.to_string();
    let mut data_dir = PathBuf::from(DEFAULT_DATA_DIR);
    let mut max_body_bytes = DEFAULT_MAX_BODY_BYTES;
    let mut max_tree_nodes = DEFAULT_MAX_TREE_NODES;
    let mut max_active_jobs = DEFAULT_MAX_ACTIVE_JOBS;
    let mut index = 0;

    while index < arguments.len() {
        let flag = &arguments[index];
        let value = |index: &mut usize| -> Result<String, String> {
            *index += 1;
            arguments
                .get(*index)
                .cloned()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--bind" => bind = value(&mut index)?,
            "--data-dir" => data_dir = PathBuf::from(value(&mut index)?),
            "--max-body-bytes" => {
                max_body_bytes = parse_positive_usize("--max-body-bytes", &value(&mut index)?)?
            }
            "--max-tree-nodes" => {
                max_tree_nodes = parse_positive_usize("--max-tree-nodes", &value(&mut index)?)?
            }
            "--max-active-jobs" => {
                max_active_jobs = parse_positive_usize("--max-active-jobs", &value(&mut index)?)?
            }
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }

    Ok(ServerOptions {
        bind,
        data_dir,
        max_body_bytes,
        max_tree_nodes,
        max_active_jobs,
    })
}

fn parse_positive_usize(flag: &str, value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid {flag}: {error}"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be positive"));
    }
    Ok(parsed)
}

fn usage() -> &'static str {
    "Usage:\n  holdem-solver-server [--bind ADDR] [--data-dir DIR] [--max-body-bytes N] [--max-tree-nodes N] [--max-active-jobs N]\n\nEndpoints:\n  GET  /healthz\n  POST /v1/spot/validate\n  POST /v1/spot/solve        synchronous solve\n  POST /v1/jobs               queued background solve\n  GET  /v1/jobs/JOB_ID       status/progress\n  GET  /v1/jobs/JOB_ID/result\n  POST /v1/jobs/JOB_ID/cancel\n\nPOST /v1/spot/validate accepts a MultiwayHoldemSpotJob JSON document.\nPOST /v1/spot/solve and POST /v1/jobs accept {\"job\": JOB, \"job_id\": ID, \"target_iterations\": N, \"utility_samples\": N}."
}

fn handle_connection(stream: &mut TcpStream, state: &Arc<ServerState>) -> Result<(), ApiError> {
    let request = read_http_request(stream, state.options.max_body_bytes)?;
    let path = request
        .target
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(&request.target);
    if request.method == "GET" && (path == "/" || path == "/index.html") {
        return write_html_response(stream, 200, FRONTEND_HTML);
    }
    let response = route_request(&request, state)?;
    write_json_response(stream, response.0, response.1)
}

fn route_request(
    request: &HttpRequest,
    state: &Arc<ServerState>,
) -> Result<(u16, Value), ApiError> {
    let path = request
        .target
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(&request.target);

    match (request.method.as_str(), path) {
        ("GET", "/healthz") => Ok((
            200,
            json!({
                "ok": true,
                "service": "holdem-solver-server",
                "api_version": 1,
                "execution": "native",
            }),
        )),
        ("POST", "/v1/spot/validate") => {
            let job = parse_job_body(&request.body)?;
            Ok((200, validate_job(job, state.options.max_tree_nodes)?))
        }
        ("POST", "/v1/spot/solve") => {
            let solve_request: SolveRequest = serde_json::from_slice(&request.body)
                .map_err(|error| ApiError::bad_request("invalid_json", error.to_string()))?;
            Ok((200, solve_job(solve_request, &state.options, None)?))
        }
        ("POST", "/v1/jobs") => {
            let solve_request: SolveRequest = serde_json::from_slice(&request.body)
                .map_err(|error| ApiError::bad_request("invalid_json", error.to_string()))?;
            enqueue_job(solve_request, state)
        }
        ("GET", path) if path.starts_with("/v1/jobs/") && path.ends_with("/result") => {
            let job_id = path
                .trim_start_matches("/v1/jobs/")
                .trim_end_matches("/result")
                .trim_end_matches('/');
            if job_id.is_empty() || job_id.contains('/') {
                return Err(ApiError::bad_request(
                    "invalid_job_id",
                    "job id must be one path segment",
                ));
            }
            validate_job_id(job_id)?;
            load_job_result(job_id, state)
        }
        ("POST", path) if path.starts_with("/v1/jobs/") && path.ends_with("/cancel") => {
            let job_id = path
                .trim_start_matches("/v1/jobs/")
                .trim_end_matches("/cancel");
            let job_id = job_id.trim_end_matches('/');
            if job_id.is_empty() || job_id.contains('/') {
                return Err(ApiError::bad_request(
                    "invalid_job_id",
                    "job id must be one path segment",
                ));
            }
            validate_job_id(job_id)?;
            cancel_job(job_id, state)
        }
        ("GET", path) if path.starts_with("/v1/jobs/") => {
            let job_id = path.trim_start_matches("/v1/jobs/");
            if job_id.is_empty() || job_id.contains('/') {
                return Err(ApiError::bad_request(
                    "invalid_job_id",
                    "job id must be one path segment",
                ));
            }
            validate_job_id(job_id)?;
            load_job_status(job_id, state)
        }
        _ => Err(ApiError::new(
            404,
            "not_found",
            format!("no route for {} {}", request.method, path),
        )),
    }
}

fn parse_job_body(body: &[u8]) -> Result<MultiwayHoldemSpotJob, ApiError> {
    let text = std::str::from_utf8(body)
        .map_err(|error| ApiError::bad_request("invalid_utf8", error.to_string()))?;
    MultiwayHoldemSpotJob::from_json(text)
        .map_err(|error| ApiError::bad_request("invalid_job", error))
}

fn validate_job(job: MultiwayHoldemSpotJob, max_tree_nodes: usize) -> Result<Value, ApiError> {
    let config = job
        .into_config()
        .map_err(|error| ApiError::bad_request("invalid_job", error))?;
    enforce_tree_limit(&config.tree, max_tree_nodes)?;
    let state = config
        .state_after_history()
        .map_err(|error| ApiError::bad_request("invalid_history", error))?;
    let tree = config
        .build_tree()
        .map_err(|error| ApiError::bad_request("tree_build_failed", error))?;
    let tree_fingerprint = holdem_solver_core::multiway_holdem_tree_fingerprint(&tree);
    Ok(json!({
        "ok": true,
        "data": {
            "table_size": config.table.table_size,
            "hero_player": config.hero_player,
            "hero_hands": config.hero_hands.len(),
            "actor_after_history": state.actor,
            "tree_nodes": tree.nodes.len(),
            "tree_fingerprint": tree_fingerprint,
        }
    }))
}

fn solve_job(
    request: SolveRequest,
    options: &ServerOptions,
    cancellation: Option<Arc<AtomicBool>>,
) -> Result<Value, ApiError> {
    let job_id = request
        .job_id
        .ok_or_else(|| ApiError::bad_request("missing_job_id", "solve requires job_id"))?;
    validate_job_id(&job_id)?;

    let job = request.job;
    let config = job
        .clone()
        .into_config()
        .map_err(|error| ApiError::bad_request("invalid_job", error))?;
    enforce_tree_limit(&config.tree, options.max_tree_nodes)?;
    let target_iterations = request
        .target_iterations
        .or_else(|| nonzero(job.execution.target_iterations))
        .ok_or_else(|| {
            ApiError::bad_request(
                "missing_target_iterations",
                "provide target_iterations or job.execution.target_iterations",
            )
        })?;
    if target_iterations == 0 {
        return Err(ApiError::bad_request(
            "invalid_target_iterations",
            "target_iterations must be positive",
        ));
    }
    let utility_samples = request.utility_samples.unwrap_or(256);
    if utility_samples == 0 {
        return Err(ApiError::bad_request(
            "invalid_utility_samples",
            "utility_samples must be positive",
        ));
    }

    let job_directory = options.data_dir.join("jobs").join(&job_id);
    let result_path = options
        .data_dir
        .join("results")
        .join(format!("{job_id}.json"));
    let store = MultiwayBatchJobStore::new(&job_directory).map_err(ApiError::internal)?;
    let job_json = job.to_json().map_err(ApiError::internal)?;
    write_text_atomically(&job_directory.join("job.json"), &job_json)
        .map_err(ApiError::internal)?;

    let (tree, fresh_solver) = config
        .build_solver()
        .map_err(|error| ApiError::new(422, "solver_build_failed", error))?;
    let fresh_config_fingerprint = fresh_solver.checkpoint().config_fingerprint;
    let mut solver = if store.manifest_path().exists() {
        store
            .resume_solver(
                tree.clone(),
                config.ranges.clone(),
                config.dead_cards,
                fresh_config_fingerprint,
            )
            .map_err(|error| ApiError::new(409, "resume_rejected", error))?
    } else {
        fresh_solver
    };

    let execution = &job.execution;
    let job_config = MultiwayBatchJobConfig {
        target_iterations,
        worker_count: execution.worker_count,
        reduction_batch_size: execution.reduction_batch_size,
        checkpoint_interval: execution.checkpoint_interval,
        max_private_attempts: execution.max_private_attempts,
        keep_checkpoints: execution.keep_checkpoints,
    };
    let manifest = match cancellation {
        Some(cancel_flag) => store
            .run_to_target_with_control(&job_id, &mut solver, &job_config, || {
                !cancel_flag.load(Ordering::Relaxed)
            })
            .map_err(|error| ApiError::new(422, "solver_execution_failed", error))?,
        None => store
            .run_to_target(&job_id, &mut solver, &job_config)
            .map_err(|error| ApiError::new(422, "solver_execution_failed", error))?,
    };
    if manifest.status == holdem_solver_core::MultiwayBatchJobStatus::Cancelled {
        return Ok(json!({
            "ok": true,
            "data": {
                "job_id": manifest.job_id,
                "status": "Cancelled",
                "completed_iterations": manifest.completed_iterations,
                "checkpoint_sequence": manifest.checkpoint_sequence,
                "latest_checkpoint": manifest.latest_checkpoint,
                "result": Value::Null,
            }
        }));
    }
    let result = config
        .result_from_solver(&tree, &solver, utility_samples)
        .map_err(|error| ApiError::new(422, "result_build_failed", error))?;
    let result_json = result.to_json().map_err(ApiError::internal)?;
    write_text_atomically(&result_path, &result_json).map_err(ApiError::internal)?;
    let result_value: Value = serde_json::from_str(&result_json)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let status = format!("{:?}", manifest.status);

    Ok(json!({
        "ok": true,
        "data": {
            "job_id": manifest.job_id,
            "status": status,
            "completed_iterations": manifest.completed_iterations,
            "checkpoint_sequence": manifest.checkpoint_sequence,
            "latest_checkpoint": manifest.latest_checkpoint,
            "result": result_value,
        }
    }))
}

fn enqueue_job(request: SolveRequest, state: &Arc<ServerState>) -> Result<(u16, Value), ApiError> {
    let job_id = request
        .job_id
        .clone()
        .ok_or_else(|| ApiError::bad_request("missing_job_id", "async solve requires job_id"))?;
    validate_job_id(&job_id)?;
    let config = request
        .job
        .clone()
        .into_config()
        .map_err(|error| ApiError::bad_request("invalid_job", error))?;
    enforce_tree_limit(&config.tree, state.options.max_tree_nodes)?;
    validate_target_and_samples(&request)?;

    let cancel_flag = Arc::new(AtomicBool::new(false));
    {
        let mut active = state
            .active_jobs
            .lock()
            .map_err(|_| ApiError::internal("active job registry is poisoned"))?;
        if active.contains_key(&job_id) {
            return Err(ApiError::new(
                409,
                "job_already_active",
                format!("job {job_id} is already queued or running"),
            ));
        }
        if active.len() >= state.options.max_active_jobs {
            return Err(ApiError::new(
                429,
                "job_capacity_reached",
                format!(
                    "server already has {} active jobs",
                    state.options.max_active_jobs
                ),
            ));
        }
        active.insert(job_id.to_string(), Arc::clone(&cancel_flag));
    }

    let worker_state = Arc::clone(state);
    let worker_job_id = job_id.to_string();
    std::thread::spawn(move || {
        let result = solve_job(
            request,
            &worker_state.options,
            Some(Arc::clone(&cancel_flag)),
        );
        if let Err(error) = result {
            let _ = write_job_error(&worker_state.options, &worker_job_id, &error);
            eprintln!("async job {} failed: {}", worker_job_id, error.message);
        }
        if let Ok(mut active) = worker_state.active_jobs.lock() {
            active.remove(&worker_job_id);
        }
    });

    Ok((
        202,
        json!({
            "ok": true,
            "data": {
                "job_id": job_id,
                "status": "Queued",
                "status_url": format!("/v1/jobs/{job_id}"),
                "cancel_url": format!("/v1/jobs/{job_id}/cancel"),
            }
        }),
    ))
}

fn cancel_job(job_id: &str, state: &Arc<ServerState>) -> Result<(u16, Value), ApiError> {
    let active_flag = state
        .active_jobs
        .lock()
        .map_err(|_| ApiError::internal("active job registry is poisoned"))?
        .get(job_id)
        .cloned();
    if let Some(flag) = active_flag {
        flag.store(true, Ordering::Relaxed);
        return Ok((
            202,
            json!({
                "ok": true,
                "data": { "job_id": job_id, "status": "CancellationRequested" }
            }),
        ));
    }

    let job_directory = state.options.data_dir.join("jobs").join(job_id);
    let manifest_path = job_directory.join("manifest.json");
    if !manifest_path.exists() {
        return Err(ApiError::new(
            404,
            "job_not_found",
            "job is not queued, running or persisted",
        ));
    }
    let store = MultiwayBatchJobStore::new(&job_directory).map_err(ApiError::internal)?;
    let manifest = store
        .load_manifest()
        .map_err(|error| ApiError::new(500, "invalid_manifest", error))?;
    Ok((
        200,
        json!({
            "ok": true,
            "data": {
                "job_id": job_id,
                "status": format!("{:?}", manifest.status),
                "message": "job is no longer active; no cancellation was needed"
            }
        }),
    ))
}

fn load_job_status(job_id: &str, state: &Arc<ServerState>) -> Result<(u16, Value), ApiError> {
    let job_directory = state.options.data_dir.join("jobs").join(job_id);
    let manifest_path = job_directory.join("manifest.json");
    if !manifest_path.exists() {
        let active = state
            .active_jobs
            .lock()
            .map_err(|_| ApiError::internal("active job registry is poisoned"))?
            .contains_key(job_id);
        if active {
            return Ok((
                200,
                json!({
                    "ok": true,
                    "data": { "job_id": job_id, "status": "Queued" }
                }),
            ));
        }
        let error_path = job_directory.join("error.json");
        if error_path.exists() {
            let error = fs::read_to_string(error_path)
                .map_err(|error| ApiError::internal(error.to_string()))?;
            let error_value: Value = serde_json::from_str(&error)
                .map_err(|error| ApiError::new(500, "invalid_job_error", error.to_string()))?;
            return Ok((200, json!({ "ok": true, "data": error_value })));
        }
        return Err(ApiError::new(
            404,
            "job_not_found",
            "job manifest does not exist",
        ));
    }
    let store = MultiwayBatchJobStore::new(&job_directory).map_err(ApiError::internal)?;
    let manifest = store
        .load_manifest()
        .map_err(|error| ApiError::new(500, "invalid_manifest", error))?;
    let value =
        serde_json::to_value(manifest).map_err(|error| ApiError::internal(error.to_string()))?;
    Ok((200, json!({ "ok": true, "data": { "manifest": value } })))
}

fn load_job_result(job_id: &str, state: &Arc<ServerState>) -> Result<(u16, Value), ApiError> {
    let result_path = state
        .options
        .data_dir
        .join("results")
        .join(format!("{job_id}.json"));
    if !result_path.exists() {
        return Err(ApiError::new(
            404,
            "result_not_found",
            "job result is not available yet",
        ));
    }
    let text =
        fs::read_to_string(result_path).map_err(|error| ApiError::internal(error.to_string()))?;
    let result: Value = serde_json::from_str(&text)
        .map_err(|error| ApiError::new(500, "invalid_result", error.to_string()))?;
    Ok((
        200,
        json!({
            "ok": true,
            "data": { "job_id": job_id, "result": result }
        }),
    ))
}

fn validate_target_and_samples(request: &SolveRequest) -> Result<(), ApiError> {
    let target_iterations = request
        .target_iterations
        .or_else(|| nonzero(request.job.execution.target_iterations))
        .ok_or_else(|| {
            ApiError::bad_request(
                "missing_target_iterations",
                "provide target_iterations or job.execution.target_iterations",
            )
        })?;
    if target_iterations == 0 {
        return Err(ApiError::bad_request(
            "invalid_target_iterations",
            "target_iterations must be positive",
        ));
    }
    if request.utility_samples.unwrap_or(256) == 0 {
        return Err(ApiError::bad_request(
            "invalid_utility_samples",
            "utility_samples must be positive",
        ));
    }
    Ok(())
}

fn write_job_error(options: &ServerOptions, job_id: &str, error: &ApiError) -> Result<(), String> {
    let job_directory = options.data_dir.join("jobs").join(job_id);
    let value = error_json(error);
    let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    write_text_atomically(&job_directory.join("error.json"), &text)
}

fn enforce_tree_limit(
    tree: &MultiwayHoldemSpotTreeConfig,
    max_tree_nodes: usize,
) -> Result<(), ApiError> {
    let configured = match tree {
        MultiwayHoldemSpotTreeConfig::Round(config) => config.max_nodes,
        MultiwayHoldemSpotTreeConfig::Full(config) => config.round.max_nodes,
    };
    if configured > max_tree_nodes {
        return Err(ApiError::bad_request(
            "tree_limit_exceeded",
            format!("configured max_nodes {configured} exceeds server limit {max_tree_nodes}"),
        ));
    }
    Ok(())
}

fn validate_job_id(job_id: &str) -> Result<(), ApiError> {
    if job_id.is_empty() || job_id.len() > 128 || job_id == "." || job_id == ".." {
        return Err(ApiError::bad_request(
            "invalid_job_id",
            "job id must be 1-128 characters",
        ));
    }
    if !job_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ApiError::bad_request(
            "invalid_job_id",
            "job id may contain only ASCII letters, digits, '-', '_' and '.'",
        ));
    }
    Ok(())
}

fn nonzero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

fn error_json(error: &ApiError) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": error.code,
            "message": error.message,
        }
    })
}

fn read_http_request(
    stream: &mut TcpStream,
    max_body_bytes: usize,
) -> Result<HttpRequest, ApiError> {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 4096];
        let read = stream
            .read(&mut chunk)
            .map_err(|error| ApiError::new(400, "read_error", error.to_string()))?;
        if read == 0 {
            return Err(ApiError::new(
                400,
                "empty_request",
                "connection closed before request",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(ApiError::new(
                413,
                "headers_too_large",
                "HTTP headers exceed the limit",
            ));
        }
    };

    let header_text = std::str::from_utf8(&buffer[..header_end])
        .map_err(|error| ApiError::bad_request("invalid_headers", error.to_string()))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| ApiError::bad_request("invalid_request_line", "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| ApiError::bad_request("invalid_request_line", "missing HTTP method"))?;
    let target = request_parts
        .next()
        .ok_or_else(|| ApiError::bad_request("invalid_request_line", "missing request target"))?;
    let version = request_parts
        .next()
        .ok_or_else(|| ApiError::bad_request("invalid_request_line", "missing HTTP version"))?;
    if version != "HTTP/1.0" && version != "HTTP/1.1" {
        return Err(ApiError::bad_request(
            "unsupported_http_version",
            "only HTTP/1.0 and HTTP/1.1 are supported",
        ));
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| ApiError::bad_request("invalid_header", "header has no colon"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    if headers.contains_key("transfer-encoding") {
        return Err(ApiError::bad_request(
            "unsupported_transfer_encoding",
            "chunked transfer encoding is not supported; send Content-Length",
        ));
    }
    let content_length = headers
        .get("content-length")
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|error| ApiError::bad_request("invalid_content_length", error.to_string()))
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > max_body_bytes {
        return Err(ApiError::new(
            413,
            "body_too_large",
            format!("request body exceeds {max_body_bytes} bytes"),
        ));
    }

    let mut body = buffer[header_end..].to_vec();
    if body.len() > content_length {
        body.truncate(content_length);
    }
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let mut chunk = vec![0u8; remaining.min(64 * 1024)];
        let read = stream
            .read(&mut chunk)
            .map_err(|error| ApiError::new(400, "read_error", error.to_string()))?;
        if read == 0 {
            return Err(ApiError::bad_request(
                "truncated_body",
                "connection closed before Content-Length bytes were received",
            ));
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(HttpRequest {
        method: method.to_string(),
        target: target.to_string(),
        body,
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_html_response(stream: &mut TcpStream, status: u16, body: &str) -> Result<(), ApiError> {
    let bytes = body.as_bytes();
    let reason = status_reason(status);
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|_| stream.write_all(bytes))
        .map_err(|error| ApiError::new(500, "write_error", error.to_string()))
}

fn write_json_response(stream: &mut TcpStream, status: u16, body: Value) -> Result<(), ApiError> {
    let bytes = serde_json::to_vec(&body).map_err(|error| ApiError::internal(error.to_string()))?;
    let reason = status_reason(status);
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|_| stream.write_all(&bytes))
        .map_err(|error| ApiError::new(500, "write_error", error.to_string()))
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn write_text_atomically(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create output directory: {error}"))?;
    }
    let temporary = unique_temporary_path(path);
    fs::write(&temporary, text).map_err(|error| format!("write temporary file: {error}"))?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_first_error) if path.exists() => {
            fs::remove_file(path).map_err(|error| format!("replace existing file: {error}"))?;
            fs::rename(&temporary, path)
                .map_err(|error| format!("commit file after replacement: {error}"))
        }
        Err(error) => Err(format!("commit file: {error}")),
    }
}

fn unique_temporary_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
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
