//! `relay-vopr`: run or sweep deterministic sync simulations.

use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;

use clap::{Parser, Subcommand};
use relay_vopr::{Failure, RunReport, Scenario};

#[derive(Parser, Debug)]
#[command(name = "relay-vopr", about = "Deterministic sync simulator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List scenarios
    List,
    /// Run one scenario with one seed
    Run {
        #[arg(long)]
        scenario: String,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Override the scenario's step count
        #[arg(long)]
        steps: Option<u32>,
        /// Print every simulated event to stderr
        #[arg(long)]
        trace: bool,
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run many seeds of one or all scenarios
    Sweep {
        /// Scenario name, or every scenario when omitted
        #[arg(long)]
        scenario: Option<String>,
        #[arg(long, default_value_t = 0)]
        start: u64,
        #[arg(long, default_value_t = 20)]
        seeds: u64,
        /// Worker threads (default: available parallelism)
        #[arg(long)]
        jobs: Option<usize>,
        /// Override every scenario's step count
        #[arg(long)]
        steps: Option<u32>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::List => {
            for s in Scenario::all() {
                println!(
                    "{:<20} {:>2} nodes {:?} {} steps  {}",
                    s.name, s.nodes, s.topology, s.steps, s.description
                );
            }
            ExitCode::SUCCESS
        }
        Command::Run {
            scenario,
            seed,
            steps,
            trace,
            json,
        } => {
            let Some(mut scenario) = Scenario::by_name(&scenario) else {
                eprintln!("unknown scenario {scenario}; see `relay-vopr list`");
                return ExitCode::from(2);
            };
            if let Some(steps) = steps {
                scenario.steps = steps;
            }
            match relay_vopr::run(&scenario, seed, trace) {
                Ok(report) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&report).expect("serialize")
                        );
                    } else {
                        print_report(&report);
                    }
                    ExitCode::SUCCESS
                }
                Err(failure) => {
                    eprintln!("{failure}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Sweep {
            scenario,
            start,
            seeds,
            jobs,
            steps,
        } => {
            let mut scenarios = match scenario {
                Some(name) => match Scenario::by_name(&name) {
                    Some(s) => vec![s],
                    None => {
                        eprintln!("unknown scenario {name}; see `relay-vopr list`");
                        return ExitCode::from(2);
                    }
                },
                None => Scenario::all(),
            };
            if let Some(steps) = steps {
                for s in &mut scenarios {
                    s.steps = steps;
                }
            }
            let jobs = jobs
                .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
                .unwrap_or(1)
                .max(1);
            let started = Instant::now();
            let failures = sweep(&scenarios, start, seeds, jobs);
            println!(
                "{} runs in {:.1?} on {jobs} threads",
                scenarios.len() as u64 * seeds,
                started.elapsed()
            );
            if failures.is_empty() {
                ExitCode::SUCCESS
            } else {
                for f in &failures {
                    eprintln!("{f}");
                }
                eprintln!("{} failing runs", failures.len());
                ExitCode::FAILURE
            }
        }
    }
}

fn print_report(r: &RunReport) {
    let s = &r.stats;
    println!(
        "{} seed {}: ok in {} ms wall, {} ms virtual; {} steps, {} ops, {} scans, {} frames, {} fetches ({} faulted), {} cuts, {} crashes, {} write crashes, {} io faults, {} quiesces, {} conflict copies, {} holds applied, {} warnings; trace {}",
        r.scenario,
        r.seed,
        r.wall_ms,
        s.virtual_ms,
        s.steps,
        s.ops,
        s.scans,
        s.frames,
        s.fetches,
        s.fetch_faults,
        s.cuts,
        s.crashes,
        s.write_crashes,
        s.io_faults,
        s.quiesces,
        s.conflict_copies,
        s.holds_applied,
        s.warnings,
        &r.trace_digest[..12]
    );
}

fn sweep(scenarios: &[Scenario], start: u64, seeds: u64, jobs: usize) -> Vec<Failure> {
    let work: Vec<(usize, u64)> = scenarios
        .iter()
        .enumerate()
        .flat_map(|(i, _)| (start..start + seeds).map(move |seed| (i, seed)))
        .collect();
    let queue = Mutex::new(work.into_iter());
    let failures = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().expect("queue").next();
                    let Some((i, seed)) = next else { break };
                    match relay_vopr::run(&scenarios[i], seed, false) {
                        Ok(report) => print_report(&report),
                        Err(failure) => {
                            eprintln!(
                                "FAIL {} seed {}: {}",
                                failure.scenario,
                                failure.seed,
                                failure.message.lines().next().unwrap_or("")
                            );
                            failures.lock().expect("failures").push(failure);
                        }
                    }
                }
            });
        }
    });
    failures.into_inner().expect("failures")
}
