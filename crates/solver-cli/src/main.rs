use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use holdem_solver_core::{MultiwayBatchJobConfig, MultiwayBatchJobStore, MultiwayHoldemSpotJob};
use holdem_solver_icm::{all_in_bubble_factor, icm_equity, marginal_bubble_factor};
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
    stacks: Option<Vec<i64>>,
    payouts: Option<Vec<i64>>,
    hero: Option<usize>,
    villain: Option<usize>,
    delta: Option<i64>,
}

struct CommandResult {
    human_lines: Vec<String>,
    json: Value,
}

fn usage() -> &'static str {
    "Usage:\n  holdem-solver validate --job JOB.json [--json]\n  holdem-solver solve --job JOB.json --output RESULT.json [--job-dir DIR] [--iterations N] [--utility-samples N] [--job-id ID] [--json]\n  holdem-solver icm --stacks STACKS --payouts PAYOUTS [--hero INDEX] [--villain INDEX] [--delta N] [--json]\n\nCommands:\n  validate  Parse the JSON job, validate history, and build the configured tree.\n  solve     Run or resume a persistent arena job and write a JSON spot result.\n  icm       Compute ICM equity and bubble factors for given stacks and payouts.\n\nOutput:\n  --json    Emit one machine-readable JSON success or error envelope.\n\nStacks and payouts are comma-separated integers, e.g. --stacks 1000,2000,3000 --payouts 100,50,25"
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
    if options.command == "icm" {
        return icm_command(options);
    }
    let json = fs::read_to_string(&options.job_path)
        .map_err(|error| format!("read job {}: {error}", options.job_path.display()))?;
    let job = MultiwayHoldemSpotJob::from_json(&json)?;
    let config = job.clone().into_config()?;
    match options.command.as_str() {
        "validate" => {
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
        "solve" => solve_job(options, job, config),
        _ => Err(format!("unknown command: {}", options.command)),
    }
}

fn solve_job(
    options: CliOptions,
    job: MultiwayHoldemSpotJob,
    config: holdem_solver_core::MultiwayHoldemSpotConfig,
) -> Result<CommandResult, String> {
    let output_path = options
        .output_path
        .ok_or_else(|| "solve requires --output RESULT.json".to_string())?;
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

fn parse_args() -> Result<CliOptions, String> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.is_empty() || arguments[0] == "--help" || arguments[0] == "-h" {
        println!("{}", usage());
        std::process::exit(0);
    }
    let command = arguments[0].clone();
    if command != "validate" && command != "solve" && command != "icm" {
        return Err(format!("unknown command: {command}"));
    }
    let mut job_path = None;
    let mut output_path = None;
    let mut job_directory = None;
    let mut iterations = None;
    let mut utility_samples = 256usize;
    let mut job_id = None;
    let mut stacks: Option<Vec<i64>> = None;
    let mut payouts: Option<Vec<i64>> = None;
    let mut hero: Option<usize> = None;
    let mut villain: Option<usize> = None;
    let mut delta: Option<i64> = None;
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
            "--stacks" => {
                let stacks_str = value(&mut index)?;
                stacks = Some(parse_i64_list(&stacks_str)?);
            }
            "--payouts" => {
                let payouts_str = value(&mut index)?;
                payouts = Some(parse_i64_list(&payouts_str)?);
            }
            "--hero" => {
                hero = Some(
                    value(&mut index)?
                        .parse::<usize>()
                        .map_err(|error| format!("invalid --hero: {error}"))?,
                );
            }
            "--villain" => {
                villain = Some(
                    value(&mut index)?
                        .parse::<usize>()
                        .map_err(|error| format!("invalid --villain: {error}"))?,
                );
            }
            "--delta" => {
                delta = Some(
                    value(&mut index)?
                        .parse::<i64>()
                        .map_err(|error| format!("invalid --delta: {error}"))?,
                );
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
    if command == "icm" {
        let stacks = stacks.ok_or_else(|| "missing --stacks".to_string())?;
        let payouts = payouts.ok_or_else(|| "missing --payouts".to_string())?;
        if hero.is_some() && villain.is_none() {
            return Err("--villain is required when --hero is provided".to_string());
        }
        if hero.is_none() && villain.is_some() {
            return Err("--hero is required when --villain is provided".to_string());
        }
        Ok(CliOptions {
            command,
            job_path: PathBuf::new(),
            output_path: None,
            job_directory: None,
            iterations: None,
            utility_samples: 0,
            job_id: None,
            stacks: Some(stacks),
            payouts: Some(payouts),
            hero,
            villain,
            delta,
        })
    } else {
        let job_path = job_path.ok_or_else(|| "missing --job JOB.json".to_string())?;
        if command == "validate" && output_path.is_some() {
            return Err("--output is only valid for solve".to_string());
        }
        Ok(CliOptions {
            command,
            job_path,
            output_path,
            job_directory,
            iterations,
            utility_samples,
            job_id,
            stacks: None,
            payouts: None,
            hero: None,
            villain: None,
            delta: None,
        })
    }
}

fn icm_command(options: CliOptions) -> Result<CommandResult, String> {
    let stacks = options
        .stacks
        .ok_or_else(|| "missing --stacks".to_string())?;
    let payouts = options
        .payouts
        .ok_or_else(|| "missing --payouts".to_string())?;

    let result = icm_equity(&stacks, &payouts).map_err(|e| e.to_string())?;

    let mut human_lines = vec![
        format!("players={}", stacks.len()),
        format!("prize_pool={}", payouts.iter().sum::<i64>()),
    ];

    for (i, ev) in result.ev.iter().enumerate() {
        human_lines.push(format!("player_{}=${:.2}", i, ev));
    }

    let mut data = json!({
        "players": stacks.len(),
        "prize_pool": payouts.iter().sum::<i64>(),
        "ev": result.ev,
        "place_probs": result.place_probs,
    });

    if let (Some(hero), Some(villain)) = (options.hero, options.villain) {
        let bubble =
            all_in_bubble_factor(&stacks, &payouts, hero, villain).map_err(|e| e.to_string())?;

        human_lines.push(format!(
            "all_in_bubble_factor_hero_{}_vs_{}={:.4}",
            hero, villain, bubble.bubble_factor
        ));
        human_lines.push(format!("effective_stack={}", bubble.effective_stack));
        human_lines.push(format!("current_ev={:.2}", bubble.current_ev));
        human_lines.push(format!("win_ev={:.2}", bubble.win_ev));
        human_lines.push(format!("lose_ev={:.2}", bubble.lose_ev));

        data["all_in_bubble_factor"] = json!({
            "hero": hero,
            "villain": villain,
            "effective_stack": bubble.effective_stack,
            "current_ev": bubble.current_ev,
            "win_ev": bubble.win_ev,
            "lose_ev": bubble.lose_ev,
            "bubble_factor": bubble.bubble_factor,
        });
    }

    if let (Some(hero), Some(villain), Some(delta)) = (options.hero, options.villain, options.delta)
    {
        let marginal = marginal_bubble_factor(&stacks, &payouts, hero, villain, delta)
            .map_err(|e| e.to_string())?;

        human_lines.push(format!(
            "marginal_bubble_factor_hero_{}_vs_{}_delta_{}={:.4}",
            hero, villain, delta, marginal
        ));

        data["marginal_bubble_factor"] = json!({
            "hero": hero,
            "villain": villain,
            "delta": delta,
            "bubble_factor": marginal,
        });
    }

    Ok(CommandResult {
        human_lines,
        json: json!({
            "ok": true,
            "command": "icm",
            "data": data,
        }),
    })
}

fn parse_i64_list(s: &str) -> Result<Vec<i64>, String> {
    s.split(',')
        .map(|s| s.trim().parse::<i64>())
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|e| format!("invalid list: {e}"))
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
