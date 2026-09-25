use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use holdem_solver_abstraction::{
    FlopAbstraction, FlopEquityAbstraction, Granularity, Street, StreetAbstraction,
    StreetEquityAbstraction,
};
use holdem_solver_core::{MultiwayBatchJobConfig, MultiwayBatchJobStore, MultiwayHoldemSpotJob};
use holdem_solver_icm::{all_in_bubble_factor, icm_equity, marginal_bubble_factor};
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
    br_samples: Option<usize>,
    utility_samples: usize,
    job_id: Option<String>,
    stack_bb: Option<f64>,
    matrix_boards: Option<usize>,
    stacks: Option<Vec<i64>>,
    payouts: Option<Vec<i64>>,
    hero: Option<usize>,
    villain: Option<usize>,
    delta: Option<i64>,
    granularity: Option<String>,
    equity: bool,
    equity_groups: Option<usize>,
}

struct CommandResult {
    human_lines: Vec<String>,
    json: Value,
}

fn usage() -> &'static str {
    "Usage:\n  holdem-solver validate --job JOB.json [--json]\n  holdem-solver solve --job JOB.json --output RESULT.json [--job-dir DIR] [--iterations N] [--utility-samples N] [--br-samples N] [--job-id ID] [--json]\n  holdem-solver icm --stacks S1,S2,... --payouts P1,P2,... [--hero INDEX] [--villain INDEX] [--delta N] [--json]\n  holdem-solver pushfold [--stack N] [--matrix-boards N] [--output RESULT.json] [--json]\n\nCommands:\n  validate  Parse the JSON job, validate history, and build the configured tree.\n  solve     Run or resume a persistent arena job and write a JSON spot result.\n  icm       Compute exact ICM equity and bubble factors for stacks and payouts.\n  pushfold  Solve heads-up push/fold for a given effective stack.\n  flop-clusters  Deterministic flop clustering report (1755 classes); --equity adds exact equity refinement (full pass, minutes).\n  turn-clusters  Deterministic turn clustering report; --equity adds exact equity refinement (~1 min release).\n  river-clusters  Deterministic river clustering report; --equity adds exact equity refinement (~15 s release).\n\nOutput:\n  --json    Emit one machine-readable JSON success or error envelope."
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
        "icm" => icm_command(options),
        "pushfold" => pushfold_command(options),
        "flop-clusters" => flop_clusters_command(options),
        "turn-clusters" => turn_clusters_command(options),
        "river-clusters" => river_clusters_command(options),
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
    // T3.2/D-019: публичное дерево абстрагируется ДО солвера; оба
    // режима идут одним путём — чекпойнты/резюме работают с
    // трансформированным деревом автоматически.
    let (tree, fresh_solver, abstraction) = config.build_solver_with_abstraction()?;
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
    // is_fresh фиксируется ДО run_to_target: manifest создаётся внутри
    // прогона, проверка после него всегда видела бы существующий файл.
    let is_fresh = !store.manifest_path().exists();
    let manifest = store.run_to_target(&job_id, &mut solver, &job_config)?;
    // MC-оценка блокировки: только при свежем прогоне — на resume не
    // пересчитывается (зависит от дерева/спеки, не от итераций).
    let blocking = match (&abstraction, is_fresh) {
        (Some((_, estimate)), true) => estimate.clone(),
        _ => None,
    };
    // T4.2/D-021: override числа сэмплов BR поверх execution.
    let br_samples = options
        .br_samples
        .unwrap_or(execution.exploitability_samples);
    let mut config = config;
    config.exploitability_samples = br_samples;
    let result = config.result_from_solver_with_abstraction(
        &tree,
        &solver,
        utility_samples,
        abstraction.map(|(report, _)| report),
        blocking,
    )?;
    write_text_atomically(&output_path, &result.to_json()?)?;
    let status = format!("{:?}", manifest.status);
    let exploitability_line = match &result.exploitability {
        Some(report) => format!(
            "exploitability={:.3}% pot (SE {:.3}; upper {:.3}% pot; {} samples)",
            report.improvement_pot_percent,
            report.improvement_standard_error,
            report.upper_bound_pot_percent,
            report.samples
        ),
        None => "exploitability=not measured".to_string(),
    };
    Ok(CommandResult {
        human_lines: vec![
            format!("job_id={}", manifest.job_id),
            format!("status={status}"),
            format!("completed_iterations={}", manifest.completed_iterations),
            format!("result={}", output_path.display()),
            format!("job_directory={}", job_directory.display()),
            exploitability_line,
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
// ICM (T1.3; восстановлено в сессии 9 после регрессии f56f046)
// ---------------------------------------------------------------------------

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
        human_lines.push(format!("player_{i}=${ev:.2}"));
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
            "all_in_bubble_factor_hero_{hero}_vs_{villain}={:.4}",
            bubble.bubble_factor
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
            "marginal_bubble_factor_hero_{hero}_vs_{villain}_delta_{delta}={:.4}",
            marginal
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
        .map(|part| part.trim().parse::<i64>())
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|e| format!("invalid list: {e}"))
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
    PathBuf::from(format!(".pushfold-matrix-{boards}.json"))
}

fn load_or_compute_matrix(boards: usize) -> Result<(EquityMatrix, bool), String> {
    let cache_path = matrix_cache_path(boards);
    if cache_path.exists() {
        if let Ok(text) = fs::read_to_string(&cache_path) {
            if let Ok(cache) = serde_json::from_str::<MatrixCache>(&text) {
                if cache.boards == boards && cache.n == 169 {
                    return Ok((
                        EquityMatrix {
                            n: cache.n,
                            e: cache.e,
                        },
                        true,
                    ));
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
    Ok((matrix, false))
}

/// Сетка 13x13: i — строка (A сверху), j — столбец.
/// hi >= lo всегда; выше диагонали suited, ниже — offsuit, диагональ — пары.
fn grid_to_class(i: usize, j: usize) -> usize {
    let (hi, lo) = if i <= j {
        (12 - i, 12 - j)
    } else {
        (12 - j, 12 - i)
    };
    if hi == lo {
        12 - hi
    } else if i < j {
        13 + hi * (hi - 1) / 2 + lo
    } else {
        91 + hi * (hi - 1) / 2 + lo
    }
}

fn pushfold_grid_lines(result: &PushFoldResult, which: &str) -> Vec<String> {
    let ranks = [
        'A', 'K', 'Q', 'J', 'T', '9', '8', '7', '6', '5', '4', '3', '2',
    ];
    let mut lines = Vec::new();
    lines.push(format!("{which}:"));
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
            } else if result.bb_call[idx] {
                "C"
            } else {
                "."
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
    // Реальный дефолт — здесь (фикс сессии 9: раньше 200, «20000»
    // из сессии 7 меняло мёртвый код и не работало).
    let matrix_boards = options.matrix_boards.unwrap_or(20000);
    let (matrix, cached) = load_or_compute_matrix(matrix_boards)?;
    let classes = all_classes();
    let result = solve_hu(stack_bb, &matrix, &classes, 60).map_err(|e| e.to_string())?;

    let mut human_lines = Vec::new();
    human_lines.push(format!("pushfold stack={stack_bb}bb"));
    human_lines.push(if cached {
        "matrix=cached".to_string()
    } else {
        format!("matrix=computed(boards={matrix_boards})")
    });
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
// Разбор аргументов
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
        "icm" => parse_icm_args(&arguments),
        "pushfold" => parse_pushfold_args(&arguments),
        "flop-clusters" => parse_flop_clusters_args(&arguments),
        "turn-clusters" => parse_street_clusters_args(&arguments, "turn-clusters"),
        "river-clusters" => parse_street_clusters_args(&arguments, "river-clusters"),
        _ => Err(format!("unknown command: {command}")),
    }
}

fn parse_job_args(arguments: &[String], command: &str) -> Result<CliOptions, String> {
    let mut job_path = None;
    let mut output_path = None;
    let mut job_directory = None;
    let mut iterations = None;
    let mut utility_samples = 256usize;
    let mut br_samples: Option<usize> = None;
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
            "--br-samples" => {
                let parsed = value(&mut index)?
                    .parse::<usize>()
                    .map_err(|error| format!("invalid --br-samples: {error}"))?;
                if parsed == 0 {
                    return Err("--br-samples must be positive".to_string());
                }
                br_samples = Some(parsed);
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
        equity: false,
        equity_groups: None,
        job_path,
        output_path,
        job_directory,
        iterations,
        utility_samples,
        br_samples,
        job_id,
        stack_bb: None,
        matrix_boards: None,
        stacks: None,
        payouts: None,
        hero: None,
        villain: None,
        delta: None,
        granularity: None,
    })
}

fn parse_icm_args(arguments: &[String]) -> Result<CliOptions, String> {
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
            "--stacks" => {
                let text = value(&mut index)?;
                stacks = Some(parse_i64_list(&text)?);
            }
            "--payouts" => {
                let text = value(&mut index)?;
                payouts = Some(parse_i64_list(&text)?);
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
    if hero.is_some() && villain.is_none() {
        return Err("--villain is required when --hero is provided".to_string());
    }
    if hero.is_none() && villain.is_some() {
        return Err("--hero is required when --villain is provided".to_string());
    }
    let stacks = stacks.ok_or_else(|| "missing --stacks".to_string())?;
    let payouts = payouts.ok_or_else(|| "missing --payouts".to_string())?;
    Ok(CliOptions {
        command: "icm".to_string(),
        equity: false,
        equity_groups: None,
        job_path: PathBuf::new(),
        output_path: None,
        job_directory: None,
        iterations: None,
        utility_samples: 0,
        br_samples: None,
        job_id: None,
        granularity: None,
        stack_bb: None,
        matrix_boards: None,
        stacks: Some(stacks),
        payouts: Some(payouts),
        hero,
        villain,
        delta,
    })
}

fn parse_pushfold_args(arguments: &[String]) -> Result<CliOptions, String> {
    let mut stack_bb = 10.0f64;
    // Реальный дефолт — здесь, в parse (см. фикс сессии 9).
    let mut matrix_boards = 20000usize;
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
        equity: false,
        equity_groups: None,
        job_path: PathBuf::new(),
        output_path,
        job_directory: None,
        iterations: None,
        utility_samples: 0,
        br_samples: None,
        job_id: None,
        stack_bb: Some(stack_bb),
        matrix_boards: Some(matrix_boards),
        stacks: None,
        payouts: None,
        hero: None,
        villain: None,
        delta: None,
        granularity: None,
    })
}

// ---------------------------------------------------------------------------
// Flop abstraction report (T3.1 v1, сессия 10)
// ---------------------------------------------------------------------------

fn flop_clusters_command(options: CliOptions) -> Result<CommandResult, String> {
    let granularity = match options.granularity.as_deref() {
        None => Granularity::Medium,
        Some("coarse") => Granularity::Coarse,
        Some("medium") => Granularity::Medium,
        Some("fine") => Granularity::Fine,
        Some(other) => return Err(format!("unknown granularity: {other}")),
    };
    if options.equity_groups.is_some() && !options.equity {
        return Err("--equity-groups requires --equity".to_string());
    }
    let equity_groups = options.equity_groups.unwrap_or(4);
    if !options.equity {
        // v1 (сессия 10): детерминированные feature-бакеты, мгновенно.
        let abstraction = FlopAbstraction::new(granularity);
        let histogram = abstraction.histogram();
        let mut human_lines = vec![
            format!("flop_classes={}", abstraction.class_count()),
            format!("granularity={granularity}"),
            format!("used_buckets={}", abstraction.used_buckets()),
            format!("fingerprint={:#018x}", abstraction.fingerprint()),
            "top_buckets:".to_string(),
        ];
        for (bucket, count) in histogram.iter().take(10) {
            human_lines.push(format!("  bucket={bucket} flops={count}"));
        }
        let buckets: Vec<Value> = histogram
            .iter()
            .map(|(bucket, count)| json!({ "bucket": bucket, "flops": count }))
            .collect();
        return Ok(CommandResult {
            human_lines,
            json: json!({
                "ok": true,
                "command": "flop-clusters",
                "data": {
                    "flop_classes": abstraction.class_count(),
                    "granularity": format!("{granularity}"),
                    "used_buckets": abstraction.used_buckets(),
                    "fingerprint": abstraction.fingerprint(),
                    "buckets": buckets,
                }
            }),
        });
    }
    // v2 (T3.1, D-017): точный эквити-проход, 1755 классов x 6 якорей.
    // Замер сессии 11: ~123 с в release на reference-машине (AI_LOG).
    let started = std::time::Instant::now();
    let abstraction = FlopEquityAbstraction::new(granularity, equity_groups)?;
    let elapsed = started.elapsed().as_secs_f64();
    let histogram = abstraction.flop_histogram();
    let mut human_lines = vec![
        format!("flop_classes={}", abstraction.class_count()),
        format!("granularity={granularity}"),
        format!("equity_groups={}", abstraction.equity_groups()),
        format!("anchors={}", abstraction.anchor_count()),
        format!("used_buckets={}", abstraction.used_buckets()),
        format!("fingerprint={:#018x}", abstraction.fingerprint()),
        format!("base_fingerprint={:#018x}", abstraction.base_fingerprint()),
        format!("total_flops={}", abstraction.total_flops()),
        format!("elapsed_seconds={elapsed:.1}"),
        "anchors:".to_string(),
    ];
    for anchor in 0..abstraction.anchor_count() {
        human_lines.push(format!(
            "  {} available={} mean={:.4} std={:.4}",
            abstraction.anchor_name(anchor).unwrap_or("?"),
            abstraction.anchor_available_classes(anchor).unwrap_or(0),
            abstraction.anchor_mean(anchor).unwrap_or(0.0),
            abstraction.anchor_std(anchor).unwrap_or(0.0),
        ));
    }
    human_lines.push("top_buckets:".to_string());
    for (bucket, flops) in histogram.iter().take(10) {
        human_lines.push(format!("  bucket={bucket} flops={flops}"));
    }
    let buckets: Vec<Value> = histogram
        .iter()
        .map(|(bucket, flops)| json!({ "bucket": bucket, "flops": flops }))
        .collect();
    let anchors: Vec<Value> = (0..abstraction.anchor_count())
        .map(|anchor| {
            json!({
                "name": abstraction.anchor_name(anchor),
                "role": abstraction.anchor_role(anchor),
                "available_classes": abstraction.anchor_available_classes(anchor),
                "mean": abstraction.anchor_mean(anchor),
                "std": abstraction.anchor_std(anchor),
            })
        })
        .collect();
    Ok(CommandResult {
        human_lines,
        json: json!({
            "ok": true,
            "command": "flop-clusters",
            "data": {
                "flop_classes": abstraction.class_count(),
                "granularity": format!("{granularity}"),
                "equity": true,
                "equity_groups": abstraction.equity_groups(),
                "anchors": anchors,
                "used_buckets": abstraction.used_buckets(),
                "fingerprint": abstraction.fingerprint(),
                "base_fingerprint": abstraction.base_fingerprint(),
                "total_flops": abstraction.total_flops(),
                "elapsed_seconds": elapsed,
                "buckets": buckets,
            }
        }),
    })
}

fn parse_flop_clusters_args(arguments: &[String]) -> Result<CliOptions, String> {
    let mut granularity: Option<String> = None;
    let mut equity = false;
    let mut equity_groups: Option<usize> = None;
    let mut index = 1;
    while index < arguments.len() {
        let flag = &arguments[index];
        match flag.as_str() {
            "--granularity" => {
                index += 1;
                granularity = Some(
                    arguments
                        .get(index)
                        .ok_or_else(|| "missing value for --granularity".to_string())?
                        .clone(),
                );
            }
            "--equity" => {
                equity = true;
            }
            "--equity-groups" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "missing value for --equity-groups".to_string())?;
                let parsed: usize = value
                    .parse()
                    .map_err(|_| format!("invalid --equity-groups value: {value}"))?;
                equity_groups = Some(parsed);
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
        command: "flop-clusters".to_string(),
        job_path: PathBuf::new(),
        output_path: None,
        job_directory: None,
        iterations: None,
        utility_samples: 0,
        br_samples: None,
        job_id: None,
        stack_bb: None,
        matrix_boards: None,
        stacks: None,
        payouts: None,
        hero: None,
        villain: None,
        delta: None,
        granularity,
        equity,
        equity_groups,
    })
}

fn turn_clusters_command(options: CliOptions) -> Result<CommandResult, String> {
    street_clusters_command(options, Street::Turn)
}

fn river_clusters_command(options: CliOptions) -> Result<CommandResult, String> {
    street_clusters_command(options, Street::River)
}

// Street clustering report (T3.1, сессия 12, D-018): зеркало flop-clusters
// для тёрна (4 карты) и ривера (5 карт).
fn street_clusters_command(options: CliOptions, street: Street) -> Result<CommandResult, String> {
    let command_name = match street {
        Street::Turn => "turn-clusters",
        Street::River => "river-clusters",
    };
    let granularity = match options.granularity.as_deref() {
        None => Granularity::Medium,
        Some("coarse") => Granularity::Coarse,
        Some("medium") => Granularity::Medium,
        Some("fine") => Granularity::Fine,
        Some(other) => return Err(format!("unknown granularity: {other}")),
    };
    if options.equity_groups.is_some() && !options.equity {
        return Err("--equity-groups requires --equity".to_string());
    }
    let equity_groups = options.equity_groups.unwrap_or(4);
    if !options.equity {
        // v1 (D-018): структурные бакеты канонических классов улицы.
        let started = std::time::Instant::now();
        let abstraction = StreetAbstraction::new(street, granularity);
        let elapsed = started.elapsed().as_secs_f64();
        let histogram = abstraction.histogram();
        let mut human_lines = vec![
            format!("street={street}"),
            format!("board_classes={}", abstraction.class_count()),
            format!("granularity={granularity}"),
            format!("used_buckets={}", abstraction.used_buckets()),
            format!("fingerprint={:#018x}", abstraction.fingerprint()),
            format!("total_boards={}", abstraction.total_boards()),
            format!("elapsed_seconds={elapsed:.1}"),
            "top_buckets:".to_string(),
        ];
        for (bucket, boards) in histogram.iter().take(10) {
            human_lines.push(format!("  bucket={bucket} boards={boards}"));
        }
        let buckets: Vec<Value> = histogram
            .iter()
            .map(|(bucket, boards)| json!({ "bucket": bucket, "boards": boards }))
            .collect();
        return Ok(CommandResult {
            human_lines,
            json: json!({
                "ok": true,
                "command": command_name,
                "data": {
                    "street": format!("{street}"),
                    "board_classes": abstraction.class_count(),
                    "granularity": format!("{granularity}"),
                    "equity": false,
                    "used_buckets": abstraction.used_buckets(),
                    "fingerprint": abstraction.fingerprint(),
                    "total_boards": abstraction.total_boards(),
                    "elapsed_seconds": elapsed,
                    "buckets": buckets,
                }
            }),
        });
    }
    // v2 (D-018): точное эквити-уточнение, те же 6 якорей, что у флопа.
    // Ривер-эквити дискретно {0, 0.5, 1} — структурное свойство, не баг.
    let started = std::time::Instant::now();
    let abstraction = StreetEquityAbstraction::new(street, granularity, equity_groups)?;
    let elapsed = started.elapsed().as_secs_f64();
    let histogram = abstraction.board_histogram();
    let mut human_lines = vec![
        format!("street={street}"),
        format!("board_classes={}", abstraction.class_count()),
        format!("granularity={granularity}"),
        format!("equity_groups={}", abstraction.equity_groups()),
        format!("anchors={}", abstraction.anchor_count()),
        format!("used_buckets={}", abstraction.used_buckets()),
        format!("fingerprint={:#018x}", abstraction.fingerprint()),
        format!("base_fingerprint={:#018x}", abstraction.base_fingerprint()),
        format!("total_boards={}", abstraction.total_boards()),
        format!("elapsed_seconds={elapsed:.1}"),
        "anchors:".to_string(),
    ];
    for anchor in 0..abstraction.anchor_count() {
        human_lines.push(format!(
            "  {} available={} mean={:.4} std={:.4}",
            abstraction.anchor_name(anchor).unwrap_or("?"),
            abstraction.anchor_available_classes(anchor).unwrap_or(0),
            abstraction.anchor_mean(anchor).unwrap_or(0.0),
            abstraction.anchor_std(anchor).unwrap_or(0.0),
        ));
    }
    human_lines.push("top_buckets:".to_string());
    for (bucket, boards) in histogram.iter().take(10) {
        human_lines.push(format!("  bucket={bucket} boards={boards}"));
    }
    let buckets: Vec<Value> = histogram
        .iter()
        .map(|(bucket, boards)| json!({ "bucket": bucket, "boards": boards }))
        .collect();
    let anchors: Vec<Value> = (0..abstraction.anchor_count())
        .map(|anchor| {
            json!({
                "name": abstraction.anchor_name(anchor),
                "role": abstraction.anchor_role(anchor),
                "available_classes": abstraction.anchor_available_classes(anchor),
                "mean": abstraction.anchor_mean(anchor),
                "std": abstraction.anchor_std(anchor),
            })
        })
        .collect();
    Ok(CommandResult {
        human_lines,
        json: json!({
            "ok": true,
            "command": command_name,
            "data": {
                "street": format!("{street}"),
                "board_classes": abstraction.class_count(),
                "granularity": format!("{granularity}"),
                "equity": true,
                "equity_groups": abstraction.equity_groups(),
                "anchors": anchors,
                "used_buckets": abstraction.used_buckets(),
                "fingerprint": abstraction.fingerprint(),
                "base_fingerprint": abstraction.base_fingerprint(),
                "total_boards": abstraction.total_boards(),
                "elapsed_seconds": elapsed,
                "buckets": buckets,
            }
        }),
    })
}

fn parse_street_clusters_args(arguments: &[String], command: &str) -> Result<CliOptions, String> {
    let mut granularity: Option<String> = None;
    let mut equity = false;
    let mut equity_groups: Option<usize> = None;
    let mut index = 1;
    while index < arguments.len() {
        let flag = &arguments[index];
        match flag.as_str() {
            "--granularity" => {
                index += 1;
                granularity = Some(
                    arguments
                        .get(index)
                        .ok_or_else(|| "missing value for --granularity".to_string())?
                        .clone(),
                );
            }
            "--equity" => {
                equity = true;
            }
            "--equity-groups" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "missing value for --equity-groups".to_string())?;
                let parsed: usize = value
                    .parse()
                    .map_err(|_| format!("invalid --equity-groups value: {value}"))?;
                equity_groups = Some(parsed);
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
        command: command.to_string(),
        job_path: PathBuf::new(),
        output_path: None,
        job_directory: None,
        iterations: None,
        utility_samples: 0,
        br_samples: None,
        job_id: None,
        stack_bb: None,
        matrix_boards: None,
        stacks: None,
        payouts: None,
        hero: None,
        villain: None,
        delta: None,
        granularity,
        equity,
        equity_groups,
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
