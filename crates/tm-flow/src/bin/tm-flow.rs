use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, bail};
use serde_json::Value;

fn usage() -> &'static str {
    "Usage:
  tm-flow build <dir>
  tm-flow seed <dir> <plan.json>
  tm-flow update <dir> <rel-path>...
  tm-flow verify <dir>
  tm-flow query <dir> <find|node|neighbors|source> [json-args] [--json]
  tm-flow unit-map <dir> <unit-id> [--owned <csv>] [--contracts <csv>] [--budget <n>] [--stats]
  tm-flow env <dir>"
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("tm-flow error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        println!("{}", usage());
        return Ok(ExitCode::SUCCESS);
    }

    match args[0].as_str() {
        "build" => {
            let dir = require_arg(&args, 1, "build <dir>")?;
            let report = tm_flow::build(Path::new(dir))?;
            println!(
                "built .flow at {}: {} files, {} nodes, {} edges (parsed {}, removed {}, unchanged {})",
                dir,
                report.total_files,
                report.total_nodes,
                report.total_edges,
                report.parsed.len(),
                report.removed.len(),
                report.unchanged,
            );
            Ok(ExitCode::SUCCESS)
        }
        "seed" => {
            let dir = require_arg(&args, 1, "seed <dir> <plan.json>")?;
            let plan_file = require_arg(&args, 2, "seed <dir> <plan.json>")?;
            let raw = std::fs::read_to_string(plan_file)
                .with_context(|| format!("cannot read plan JSON {plan_file}"))?;
            let plan: tm_core::Plan = serde_json::from_str(&raw)
                .with_context(|| format!("invalid Plan JSON in {plan_file}"))?;
            let report = tm_flow::seed_from_plan(Path::new(dir), &plan)?;
            println!(
                "seeded .flow plan at {}: {} contracts, {} units, {} planned files, {} planned nodes",
                dir, report.contracts, report.units, report.planned_files, report.planned_nodes
            );
            Ok(ExitCode::SUCCESS)
        }
        "update" => {
            let dir = require_arg(&args, 1, "update <dir> <rel-path>...")?;
            if args.len() < 3 {
                bail!("update requires at least one relative file path");
            }
            let paths: Vec<String> = args[2..].to_vec();
            let report = tm_flow::update_files(Path::new(dir), &paths)?;
            println!(
                "updated .flow at {}: {} updated, {} removed, {} flipped from planned",
                dir,
                report.updated.len(),
                report.removed.len(),
                report.flipped_from_planned.len(),
            );
            Ok(ExitCode::SUCCESS)
        }
        "verify" => {
            let dir = require_arg(&args, 1, "verify <dir>")?;
            let stale = tm_flow::verify_all(Path::new(dir))?;
            if stale.is_empty() {
                println!("fresh");
                Ok(ExitCode::SUCCESS)
            } else {
                for p in &stale {
                    println!("stale: {p}");
                }
                Ok(ExitCode::from(1))
            }
        }
        "query" => {
            let dir = require_arg(&args, 1, "query <dir> <op> [json-args]")?;
            let op = require_arg(&args, 2, "query <dir> <op> [json-args]")?;
            let mut as_json = false;
            let mut raw_args: Option<&str> = None;
            for arg in &args[3..] {
                if arg == "--json" {
                    as_json = true;
                } else if raw_args.is_none() {
                    raw_args = Some(arg.as_str());
                } else {
                    bail!("unexpected extra argument `{arg}`");
                }
            }
            let json_args: Value = match raw_args {
                Some(s) => serde_json::from_str(s)
                    .with_context(|| format!("invalid JSON args `{s}`"))?,
                None => serde_json::json!({}),
            };
            let out = tm_flow::execute_op(Path::new(dir), op, &json_args)?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&out.json)?);
            } else {
                print!("{}", out.text);
                if !out.text.ends_with('\n') {
                    println!();
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        "unit-map" => {
            let dir = require_arg(&args, 1, "unit-map <dir> <unit-id>")?;
            let unit_id = require_arg(&args, 2, "unit-map <dir> <unit-id>")?;
            let mut owned: Vec<String> = Vec::new();
            let mut contracts: Vec<String> = Vec::new();
            let mut budget = tm_flow::DEFAULT_UNIT_MAP_BUDGET;
            let mut show_stats = false;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--owned" => {
                        let val = require_arg(&args, i + 1, "--owned <csv>")?;
                        owned.extend(
                            val.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string),
                        );
                        i += 2;
                    }
                    "--contracts" => {
                        let val = require_arg(&args, i + 1, "--contracts <csv>")?;
                        contracts.extend(
                            val.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string),
                        );
                        i += 2;
                    }
                    "--budget" => {
                        let val = require_arg(&args, i + 1, "--budget <n>")?;
                        budget = val
                            .parse::<usize>()
                            .with_context(|| format!("invalid --budget `{val}`"))?;
                        i += 2;
                    }
                    "--stats" => {
                        show_stats = true;
                        i += 1;
                    }
                    other => bail!("unknown flag `{other}`"),
                }
            }
            if contracts.is_empty()
                && let Ok(Some(seed)) = tm_flow::store::read_plan_seed(Path::new(dir))
            {
                contracts = seed.contracts;
            }
            let rendered =
                tm_flow::unit_map(Path::new(dir), unit_id, &owned, &contracts, budget)?;
            print!("{rendered}");
            if !rendered.ends_with('\n') {
                println!();
            }
            if show_stats {
                let chars = rendered.chars().count();
                let approx_tokens = chars.div_ceil(4);
                println!(
                    "unit_map stats: unit={} chars={} approx_tokens={} budget={}",
                    unit_id, chars, approx_tokens, budget
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "env" => {
            let dir = require_arg(&args, 1, "env <dir>")?;
            match tm_flow::read_env(Path::new(dir))? {
                Some(facts) => {
                    println!("{}", serde_json::to_string_pretty(&facts)?);
                    println!("{}", facts.shared_prefix_line());
                }
                None => println!("no .flow/env.json"),
            }
            Ok(ExitCode::SUCCESS)
        }
        other => bail!("unknown subcommand `{other}`\n\n{}", usage()),
    }
}

fn require_arg<'a>(args: &'a [String], idx: usize, hint: &str) -> anyhow::Result<&'a str> {
    args.get(idx)
        .map(String::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing argument for `{hint}`"))
}
