use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use holdem_solver_core::{MultiwayBatchJobConfig, MultiwayBatchJobStore, MultiwayHoldemSpotJob};
use holdem_solver_pushfold::{all_classes, solve_hu, EquityMatrix, PushFoldResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct CliOptions {
    command: String,
    job_path: PathBuf,
    output_path: Option<PathBuf>,
    job_directory: Option<PathBuf>,
    iterations: Option<u64>,
    utility_samples: usize,
    job_id: Option<String>,
    stack_bb: Option<f64>,
    matrix_boards: Option<usize>,
}

struct CommandResult {
    human_lines: Vec<String>,
    json: Value,
}

fn usage() -> &'static str {
    "Usage:\n  holdem-solver validate --job JOB.json [--json]\n  holdem-solver solve --job JOB.json --output RESULT.json [--job-dir DIR] [--iterations N] [--utility-samples N] [--job-id ID] [--json]\n  holdem-solver pushfold [--stack N] [--matrix-boards N] [--output RESULT.json] [--json]\n\nCommands:\n  validate  Parse the JSON job, validate history, and build the configured tree.\n  solve     Run or resume a persistent arena job and write a JSON spot result.\n  pushfold  Solve heads-up push/fold for a given effective stack.\n\nOutput:\n  --json    Emit one machine-readable JSON success or error envelope."
}

fn main() {
    let json_output = env::args().any(|argument| argument == "--json");
    match run() {
        Ok(result) => {
            if json_output {
                println!("{}", result.json);
            } else {
                for line in result.human_lines {
                    println!("{line}");
                }
            }
        }
        Err(error) => {
            if json_output {
                println!(
                    "{}",
                    json!({
                        "ok": false,
                        "error": {
                            "code": "cli_error",
                            "message": error,
                        }
                    })
                );
            } else {
                eprintln!("error: {error}");
                eprintln!("\n{}", usage());
            }
            std::process::exit(1);
        }
    }
}

fn run() -> Result<CommandResult, String> {
    let options = parse_args()?;
    match options.command.as_str() {
        "validate" => validate_command(options),
        "solve" => solve_command(options),
        "pushfold" => pushfold_command(options),
        _ => Err(format!("unknown command: {}", options.command)),
    }
}

fn validate_command(options: CliOptions) -> Result<CommandResult, String> {
    let json = fs::read_to_string(&options.job_path)
        .map_err(|error| format!("read job {}: {error}", options.job_path.display()))?;
    let job = MultiwayHoldemSpotJob::from_json(&json)?;
    let config = job.clone().into_config()?;
    let state = config.state_after_history()?;
    let tree = config.build_tree()?;
    let tree_fingerprint = holdem_solver_core::multiway_holdem_tree_fingerprint(&tree);
    Ok(CommandResult {
        human_lines: vec![
            "valid spot job".to_string(),
            format!("table_size={}", config.table.table_size),
            format!("hero_player={}", config.hero_player),
            format!("hero_hands={}", config.hero_hands.len()),
            format!("actor_after_history={:?}", state.actor),
            format!("tree_nodes={}", tree.nodes.len()),
            format!("tree_fingerprint={tree_fingerprint}"),
        ],
        json: json!({
            "ok": true,
            "command": "validate",
            "data": {
                "table_size": config.table.table_size,
                "hero_player": config.hero_player,
                "hero_hands": config.hero_hands.len(),
                "actor_after_history": state.actor,
                "tree_nodes": tree.nodes.len(),
                "tree_fingerprint": tree_fingerprint,
            }
        }),
    })
}

fn solve_command(options: CliOptions) -> Result<CommandResult, String> {
    let output_path = options
        .output_path
        .clone()
        .ok_or_else(|| "solve requires --output RESULT.json".to_string())?;
    let json = fs::read_to_string(&options.job_path)
        .map_err(|error| format!("read job {}: {error}", options.job_path.display()))?;
    let job = MultiwayHoldemSpotJob::from_json(&json)?;
    let config = job.clone().into_config()?;
    let target_iterations = options
        .iterations
        .or_else(|| nonzero(job.execution.target_iterations))
        .ok_or_else(|| {
            "no target iterations configured; use execution.target_iterations or --iterations"
                .to_string()
        })?;
    let job_directory = options
        .job_directory
        .unwrap_or_else(|| output_path.with_extension("job"));
    let job_id = options.job_id.unwrap_or_else(|| {
        options
            .job_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("multiway-spot")
            .to_string()
    });
    let utility_samples = options.utility_samples;
    let store = MultiwayBatchJobStore::new(&job_directory)?;
    let (tree, fresh_solver) = config.build_solver()?;
    let fresh_config_fingerprint = fresh_solver.checkpoint().config_fingerprint;
    let mut solver = if store.manifest_path().exists() {
        store.resume_solver(
            tree.clone(),
            config.ranges.clone(),
            config.dead_cards,
            fresh_config_fingerprint,
        )?
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
    let manifest = store.run_to_target(&job_id, &mut solver, &job_config)?;
    let result = config.result_from_solver(&tree, &solver, utility_samples)?;
    write_text_atomically(&output_path, &result.to_json()?)?;
    let status = format!("{:?}", manifest.status);
    Ok(CommandResult {
        human_lines: vec![
            format!("job_id={}", manifest.job_id),
            format!("status={status}"),
            format!("completed_iterations={}", manifest.completed_iterations),
            format!("result={}", output_path.display()),
            format!("job_directory={}", job_directory.display()),
        ],
        json: json!({
            "ok": true,
            "command": "solve",
            "data": {
                "job_id": manifest.job_id,
                "status": status,
                "completed_iterations": manifest.completed_iterations,
                "result": output_path,
                "job_directory": job_directory,
                "checkpoint_sequence": manifest.checkpoint_sequence,
                "latest_checkpoint": manifest.latest_checkpoint,
            }
        }),
    })
}

// ---------------------------------------------------------------------------
// Push/fold (T2.3)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct MatrixCache {
    n: usize,
    boards: usize,
    e: Vec<f64>,
}

fn matrix_cache_path(boards: usize) -> PathBuf {
    PathBuf::from(format!(".pushfold-matrix-{}.json", boards))
}

fn load_or_compute_matrix(boards: usize) -> Result<EquityMatrix, String> {
    let cache_path = matrix_cache_path(boards);
    if cache_path.exists() {
        if let Ok(text) = fs::read_to_string(&cache_path) {
            if let Ok(cache) = serde_json::from_str::<MatrixCache>(&text) {
                if cache.boards == boards && cache.n == 169 {
                    return Ok(EquityMatrix {
                        n: cache.n,
                        e: cache.e,
                    });
                }
            }
        }
    }
    let classes = all_classes();
    let matrix = EquityMatrix::compute(&classes, boards, 0x5EED_0001).map_err(|e| e.to_string())?;
    let cache = MatrixCache {
        n: matrix.n,
        boards,
        e: matrix.e.clone(),
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = write_text_atomically(&cache_path, &json);
    }
    Ok(matrix)
}

fn grid_to_class(i: usize, j: usize) -> usize {
    if i == j {
        i
    } else if i < j {
        let hi = 12 - i;
        let lo = 12 - j;
        13 + hi * (hi - 1) / 2 + lo
    } else {
        let hi = 12 - j;
        let lo = 12 - i;
        91 + hi * (hi - 1) / 2 + lo
    }
}

fn pushfold_grid_lines(result: &PushFoldResult, which: &str) -> Vec<String> {
    let ranks = [
        'A', 'K', 'Q', 'J', 'T', '9', '8', '7', '6', '5', '4', '3', '2',
    ];
    let mut lines = Vec::new();
    lines.push(format!("{}:", which));
    let mut header = String::from("    ");
    for &r in ranks.iter() {
        header.push(r);
        header.push(' ');
    }
    lines.push(header);
    for i in 0..13 {
        let mut line = String::new();
        line.push(ranks[i]);
        line.push_str("  ");
        for j in 0..13 {
            let idx = grid_to_class(i, j);
            let action = if which.contains("Button") {
                if result.button_push[idx] {
                    "P"
                } else {
                    "."
                }
            } else {
                if result.bb_call[idx] {
                    "C"
                } else {
                    "."
                }
            };
            line.push_str(action);
            line.push(' ');
        }
        lines.push(line);
    }
    lines
}

fn pushfold_command(options: CliOptions) -> Result<CommandResult, String> {
    let stack_bb = options.stack_bb.unwrap_or(10.0);
    let matrix_boards = options.matrix_boards.unwrap_or(200);
    let matrix = load_or_compute_matrix(matrix_boards)?;
    let classes = all_classes();
    let result = solve_hu(stack_bb, &matrix, &classes, 60).map_err(|e| e.to_string())?;

    let mut human_lines = Vec::new();
    human_lines.push(format!("pushfold stack={stack_bb}bb"));
    human_lines.push(format!("exploitability={:.4}bb", result.exploitability_bb));
    human_lines.push(format!("push_combos={}", result.push_combos));
    human_lines.push(format!("call_combos={}", result.call_combos));
    human_lines.push(format!("iterations={}", result.iterations));
    human_lines.push(format!("stable={}", result.stable));
    human_lines.push(String::new());
    human_lines.extend(pushfold_grid_lines(
        &result,
        "Button push grid (P = push, . = fold)",
    ));
    human_lines.push(String::new());
    human_lines.extend(pushfold_grid_lines(
        &result,
        "BB call grid (C = call, . = fold)",
    ));

    let button_data: Vec<Value> = (0..classes.len())
        .map(|i| {
            json!({
                "class": classes[i].label,
                "push": result.button_push[i],
                "ev": result.button_ev[i],
                "freq": result.button_freq[i],
            })
        })
        .collect();
    let bb_data: Vec<Value> = (0..classes.len())
        .map(|j| {
            json!({
                "class": classes[j].label,
                "call": result.bb_call[j],
                "ev": result.bb_call_ev[j],
                "freq": result.bb_freq[j],
            })
        })
        .collect();

    let json = json!({
        "ok": true,
        "command": "pushfold",
        "data": {
            "stack_bb": stack_bb,
            "matrix_boards": matrix_boards,
            "exploitability_bb": result.exploitability_bb,
            "push_combos": result.push_combos,
            "call_combos": result.call_combos,
            "iterations": result.iterations,
            "stable": result.stable,
            "button": button_data,
            "bb": bb_data,
        }
    });

    if let Some(output_path) = &options.output_path {
        let json_text = serde_json::to_string_pretty(&json)
            .map_err(|e| format!("serialize pushfold json: {e}"))?;
        write_text_atomically(output_path, &json_text)?;
        human_lines.push(format!("result={}", output_path.display()));
    }

    Ok(CommandResult { human_lines, json })
}

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

fn parse_args() -> Result<CliOptions, String> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.is_empty() || arguments[0] == "--help" || arguments[0] == "-h" {
        println!("{}", usage());
        std::process::exit(0);
    }
    let command = arguments[0].clone();
    match command.as_str() {
        "validate" | "solve" => parse_job_args(&arguments, &command),
        "pushfold" => parse_pushfold_args(&arguments),
        _ => Err(format!("unknown command: {command}")),
    }
}

fn parse_job_args(arguments: &[String], command: &str) -> Result<CliOptions, String> {
    let mut job_path = None;
    let mut output_path = None;
    let mut job_directory = None;
    let mut iterations = None;
    let mut utility_samples = 256usize;
    let mut job_id = None;
    let mut index = 1;
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
            "--job" => job_path = Some(PathBuf::from(value(&mut index)?)),
            "--output" => output_path = Some(PathBuf::from(value(&mut index)?)),
            "--job-dir" => job_directory = Some(PathBuf::from(value(&mut index)?)),
            "--job-id" => job_id = Some(value(&mut index)?),
            "--iterations" => {
                iterations = Some(
                    value(&mut index)?
                        .parse::<u64>()
                        .map_err(|error| format!("invalid --iterations: {error}"))?,
                )
            }
            "--utility-samples" => {
                utility_samples = value(&mut index)?
                    .parse::<usize>()
                    .map_err(|error| format!("invalid --utility-samples: {error}"))?;
                if utility_samples == 0 {
                    return Err("--utility-samples must be positive".to_string());
                }
            }
            "--json" => {}
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }
    let job_path = job_path.ok_or_else(|| "missing --job JOB.json".to_string())?;
    if command == "validate" && output_path.is_some() {
        return Err("--output is only valid for solve".to_string());
    }
    Ok(CliOptions {
        command: command.to_string(),
        job_path,
        output_path,
        job_directory,
        iterations,
        utility_samples,
        job_id,
        stack_bb: None,
        matrix_boards: None,
    })
}

fn parse_pushfold_args(arguments: &[String]) -> Result<CliOptions, String> {
    let mut stack_bb = 10.0f64;
    let mut matrix_boards = 200usize;
    let mut output_path = None;
    let mut index = 1;
    while index < arguments.len() {
        let flag = &arguments[index];
        match flag.as_str() {
            "--stack" => {
                index += 1;
                stack_bb = arguments
                    .get(index)
                    .and_then(|v| v.parse::<f64>().ok())
                    .ok_or_else(|| "invalid --stack".to_string())?;
                if !(2.0..=50.0).contains(&stack_bb) {
                    return Err(format!("stack {stack_bb} outside 2.0..=50.0"));
                }
            }
            "--matrix-boards" => {
                index += 1;
                matrix_boards = arguments
                    .get(index)
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(|| "invalid --matrix-boards".to_string())?;
                if matrix_boards == 0 {
                    return Err("--matrix-boards must be positive".to_string());
                }
            }
            "--output" => {
                index += 1;
                output_path = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .ok_or_else(|| "missing value for --output".to_string())?,
                ));
            }
            "--json" => {}
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }
    Ok(CliOptions {
        command: "pushfold".to_string(),
        job_path: PathBuf::new(),
        output_path,
        job_directory: None,
        iterations: None,
        utility_samples: 0,
        job_id: None,
        stack_bb: Some(stack_bb),
        matrix_boards: Some(matrix_boards),
    })
}

fn nonzero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

fn write_text_atomically(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create output directory: {error}"))?;
        }
    }
    let temporary = unique_temporary_path(path);
    fs::write(&temporary, text).map_err(|error| format!("write temporary result: {error}"))?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_first_error) if path.exists() => {
            fs::remove_file(path).map_err(|error| format!("replace existing result: {error}"))?;
            fs::rename(&temporary, path)
                .map_err(|error| format!("commit result after replacement: {error}"))
        }
        Err(error) => Err(format!("commit result: {error}")),
    }
}

fn unique_temporary_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("result");
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
