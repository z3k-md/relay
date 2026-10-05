//! Deterministic simulation testing for Relay sync, in the style of
//! TigerBeetle's VOPR.
//!
//! Every node runs the real `relay-engine` and its sans-I/O `Syncer` in one
//! process on one thread. Time is virtual, randomness comes from one seeded
//! generator, and the network is a priority queue of packets with configurable
//! latency, partitions and fetch failures. Working-tree writes, renames and
//! object installs can be made to fail or to "crash" the node between the
//! temp file and its rename. After every step the simulator checks that no
//! partial content is visible; periodically it heals everything, drains the
//! network and checks that every replica converged on one tree and index,
//! that every object verifies, and that nothing any device wrote was lost.
//!
//! A failing run prints its seed; `relay-vopr run --scenario NAME --seed N`
//! replays it exactly.

pub mod model;
pub mod scenario;
pub mod sim;
pub mod tree;

use std::time::Instant;

pub use scenario::Scenario;
pub use sim::{Failure, RunReport, Stats};

/// Run one scenario with one seed. Engine panics are reported as failures
/// with the seed, like invariant violations.
pub fn run(scenario: &Scenario, seed: u64, verbose: bool) -> Result<RunReport, Failure> {
    let start = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut sim = sim::Simulator::new(scenario, seed, verbose)?;
        sim.run()?;
        Ok(sim.report(start.elapsed()))
    }));
    match outcome {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "panic".into());
            Err(Failure {
                scenario: scenario.name.into(),
                seed,
                step: 0,
                message: format!("panic: {message}"),
                trace_tail: Vec::new(),
            })
        }
    }
}
