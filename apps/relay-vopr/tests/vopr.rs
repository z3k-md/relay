//! Every scenario, a handful of seeds each. `RELAY_VOPR_SEEDS=50` widens
//! the sweep; `RELAY_VOPR_STEPS=2000` lengthens each run. A failure prints
//! the seed and the command that replays it.

use std::time::Instant;

use relay_vopr::Scenario;

fn seeds() -> u64 {
    std::env::var("RELAY_VOPR_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3)
}

fn steps() -> Option<u32> {
    std::env::var("RELAY_VOPR_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
}

fn run_scenario(name: &str) {
    let mut scenario = Scenario::by_name(name).expect("scenario exists");
    if let Some(steps) = steps() {
        scenario.steps = steps;
    }
    let started = Instant::now();
    for seed in 1..=seeds() {
        match relay_vopr::run(&scenario, seed, false) {
            Ok(report) => eprintln!(
                "{name} seed {seed}: ok in {} ms ({} ops, {} frames, {} conflict copies)",
                report.wall_ms, report.stats.ops, report.stats.frames, report.stats.conflict_copies
            ),
            Err(failure) => panic!("{failure}"),
        }
    }
    eprintln!("{name}: {} seeds in {:.1?}", seeds(), started.elapsed());
}

macro_rules! scenario_tests {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                run_scenario(stringify!($name));
            }
        )*
    };
}

scenario_tests!(
    two_node_lan,
    three_node_mesh,
    hub_and_spokes,
    chain,
    partitions,
    flaky_fetches,
    crashes,
    disk_errors,
    many_small_batches,
    delete_heavy,
    chaos,
);

#[test]
fn every_scenario_has_a_test() {
    let tested = [
        "two_node_lan",
        "three_node_mesh",
        "hub_and_spokes",
        "chain",
        "partitions",
        "flaky_fetches",
        "crashes",
        "disk_errors",
        "many_small_batches",
        "delete_heavy",
        "chaos",
    ];
    for s in Scenario::all() {
        assert!(tested.contains(&s.name), "scenario {} has no test", s.name);
    }
}

/// The same seed must replay the same events: that is what makes a failing
/// seed a reproduction.
#[test]
fn same_seed_same_trace() {
    let mut scenario = Scenario::by_name("chaos").expect("scenario exists");
    scenario.steps = 200;
    let first = relay_vopr::run(&scenario, 7, false).unwrap_or_else(|f| panic!("{f}"));
    let second = relay_vopr::run(&scenario, 7, false).unwrap_or_else(|f| panic!("{f}"));
    assert_eq!(
        first.trace_digest, second.trace_digest,
        "run is not deterministic"
    );
    let other = relay_vopr::run(&scenario, 8, false).unwrap_or_else(|f| panic!("{f}"));
    assert_ne!(
        first.trace_digest, other.trace_digest,
        "different seeds should differ"
    );
}
