//! Named scenarios: node count, topology, fault profile and workload mix.

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Topology {
    /// Every node is paired with every other node.
    Mesh,
    /// Node 0 is paired with every other node; spokes never pair with each
    /// other, so their changes travel through the hub.
    Star,
    /// Node i is paired only with i-1 and i+1.
    Chain,
    /// The last node is a home server paired with every other node. The
    /// others never pair with each other, so every change goes through the
    /// server. The server joins and attaches by itself (`relay server`).
    Server,
}

#[derive(Clone, Debug, Serialize)]
pub struct NetProfile {
    /// One-way frame latency, drawn uniformly per packet.
    pub latency_ms: (u64, u64),
    /// Object transfer rate, added on top of the latency.
    pub bytes_per_ms: u64,
    /// Per-step chance that one live link is cut (both sides see a disconnect).
    pub cut_rate: f64,
    /// Per-step chance that one cut link is healed.
    pub heal_rate: f64,
    /// Chance that an object fetch fails with a transient error.
    pub fetch_fail_rate: f64,
    /// Chance that an object fetch is answered "not found" although the
    /// serving store has the bytes.
    pub fetch_not_found_rate: f64,
    /// How long a surviving side keeps believing in a session after the
    /// link was cut or the peer died (QUIC idle timeout, missed pings).
    /// Frames it sends meanwhile are lost and its object fetches fail.
    pub notice_delay_ms: (u64, u64),
}

impl NetProfile {
    pub const fn reliable() -> Self {
        Self {
            latency_ms: (1, 20),
            bytes_per_ms: 1 << 20,
            cut_rate: 0.0,
            heal_rate: 0.0,
            fetch_fail_rate: 0.0,
            fetch_not_found_rate: 0.0,
            notice_delay_ms: (0, 0),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FaultProfile {
    /// Per-step chance that one running node crashes (process dies; disk stays).
    pub crash_rate: f64,
    /// Per-step chance that one crashed node restarts.
    pub restart_rate: f64,
    /// Per engine call chance that a working-tree write, rename or object
    /// install fails with an I/O error.
    pub io_error_rate: f64,
    /// Per engine call chance that the node dies between a finished temp
    /// file and its rename into place.
    pub crash_on_write_rate: f64,
}

impl FaultProfile {
    pub const fn none() -> Self {
        Self {
            crash_rate: 0.0,
            restart_rate: 0.0,
            io_error_rate: 0.0,
            crash_on_write_rate: 0.0,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkloadProfile {
    /// Relative weights of what one step does.
    pub op_weight: u32,
    pub scan_weight: u32,
    pub deliver_weight: u32,
    pub tick_weight: u32,
    /// Relative weights of file operations.
    pub create: u32,
    pub modify: u32,
    pub delete: u32,
    pub mkdir: u32,
    pub rename: u32,
    pub delete_dir: u32,
    /// Share of created files that are line-based text (merge candidates).
    pub text_fraction: f64,
    pub max_files: usize,
    pub max_bytes: usize,
    /// Every this many steps the simulator heals, restarts and drains
    /// everything, then checks convergence.
    pub quiesce_every: u32,
}

impl WorkloadProfile {
    pub const fn standard() -> Self {
        Self {
            op_weight: 30,
            scan_weight: 20,
            deliver_weight: 40,
            tick_weight: 10,
            create: 30,
            modify: 40,
            delete: 10,
            mkdir: 5,
            rename: 10,
            delete_dir: 3,
            text_fraction: 0.6,
            max_files: 40,
            max_bytes: 4096,
            quiesce_every: 150,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Scenario {
    pub name: &'static str,
    pub description: &'static str,
    pub nodes: usize,
    pub topology: Topology,
    pub steps: u32,
    pub net: NetProfile,
    pub faults: FaultProfile,
    pub workload: WorkloadProfile,
    /// Smaller index batches so few files still span several batches.
    pub index_batch_entries: Option<usize>,
    /// Maximum wall-clock skew between nodes.
    pub clock_skew_ms: i64,
}

impl Scenario {
    const fn base(name: &'static str, description: &'static str, nodes: usize) -> Self {
        Self {
            name,
            description,
            nodes,
            topology: Topology::Mesh,
            steps: 600,
            net: NetProfile::reliable(),
            faults: FaultProfile::none(),
            workload: WorkloadProfile::standard(),
            index_batch_entries: None,
            clock_skew_ms: 0,
        }
    }

    pub fn all() -> Vec<Scenario> {
        vec![
            Scenario::base(
                "two_node_lan",
                "Two paired devices on a reliable low-latency link.",
                2,
            ),
            Scenario {
                topology: Topology::Mesh,
                clock_skew_ms: 90_000,
                ..Scenario::base(
                    "three_node_mesh",
                    "Three fully paired devices with skewed clocks.",
                    3,
                )
            },
            Scenario {
                topology: Topology::Star,
                ..Scenario::base(
                    "hub_and_spokes",
                    "Four devices where spokes reach each other only through a hub.",
                    4,
                )
            },
            Scenario {
                topology: Topology::Chain,
                ..Scenario::base(
                    "chain",
                    "Four devices in a line; changes hop node to node.",
                    4,
                )
            },
            Scenario {
                net: NetProfile {
                    latency_ms: (5, 400),
                    cut_rate: 0.03,
                    heal_rate: 0.05,
                    notice_delay_ms: (0, 20_000),
                    ..NetProfile::reliable()
                },
                workload: WorkloadProfile {
                    modify: 60,
                    ..WorkloadProfile::standard()
                },
                ..Scenario::base(
                    "partitions",
                    "Links drop and heal while every device keeps editing; concurrent edits must merge or leave both versions.",
                    3,
                )
            },
            Scenario {
                net: NetProfile {
                    latency_ms: (20, 800),
                    bytes_per_ms: 64,
                    cut_rate: 0.12,
                    heal_rate: 0.25,
                    notice_delay_ms: (1_000, 30_000),
                    ..NetProfile::reliable()
                },
                index_batch_entries: Some(4),
                ..Scenario::base(
                    "flapping_links",
                    "Slow links that drop every few steps, often mid-batch or mid-transfer, while each side notices late.",
                    3,
                )
            },
            Scenario {
                net: NetProfile {
                    fetch_fail_rate: 0.15,
                    fetch_not_found_rate: 0.05,
                    ..NetProfile::reliable()
                },
                ..Scenario::base(
                    "flaky_fetches",
                    "Object fetches fail or come back not-found; entries must be re-requested, not dropped.",
                    3,
                )
            },
            Scenario {
                net: NetProfile {
                    notice_delay_ms: (0, 30_000),
                    ..NetProfile::reliable()
                },
                faults: FaultProfile {
                    crash_rate: 0.02,
                    restart_rate: 0.06,
                    io_error_rate: 0.0,
                    crash_on_write_rate: 0.02,
                },
                ..Scenario::base(
                    "crashes",
                    "Devices die and come back, sometimes with a finished temp file that never got renamed.",
                    3,
                )
            },
            Scenario {
                faults: FaultProfile {
                    crash_rate: 0.0,
                    restart_rate: 0.0,
                    io_error_rate: 0.08,
                    crash_on_write_rate: 0.0,
                },
                ..Scenario::base(
                    "disk_errors",
                    "Working-tree writes, renames and object installs fail at random.",
                    2,
                )
            },
            Scenario {
                index_batch_entries: Some(3),
                workload: WorkloadProfile {
                    max_files: 80,
                    max_bytes: 512,
                    ..WorkloadProfile::standard()
                },
                ..Scenario::base(
                    "many_small_batches",
                    "Index batches of three entries so catch-up spans many batches.",
                    3,
                )
            },
            Scenario {
                workload: WorkloadProfile {
                    delete: 35,
                    delete_dir: 15,
                    rename: 25,
                    max_files: 60,
                    ..WorkloadProfile::standard()
                },
                ..Scenario::base(
                    "delete_heavy",
                    "Deletes, folder deletes and renames dominate; mass-delete holds are applied.",
                    3,
                )
            },
            Scenario {
                topology: Topology::Server,
                net: NetProfile {
                    latency_ms: (5, 200),
                    cut_rate: 0.05,
                    heal_rate: 0.08,
                    notice_delay_ms: (0, 20_000),
                    ..NetProfile::reliable()
                },
                faults: FaultProfile {
                    crash_rate: 0.02,
                    restart_rate: 0.06,
                    io_error_rate: 0.0,
                    crash_on_write_rate: 0.01,
                },
                ..Scenario::base(
                    "home_server",
                    "Three devices that never pair with each other sync through an always-on server that joined and attached the space by itself; laptops drop off and crash.",
                    4,
                )
            },
            Scenario {
                topology: Topology::Mesh,
                steps: 800,
                clock_skew_ms: 30_000,
                net: NetProfile {
                    latency_ms: (1, 300),
                    cut_rate: 0.03,
                    heal_rate: 0.05,
                    fetch_fail_rate: 0.08,
                    fetch_not_found_rate: 0.02,
                    notice_delay_ms: (0, 20_000),
                    ..NetProfile::reliable()
                },
                faults: FaultProfile {
                    crash_rate: 0.015,
                    restart_rate: 0.05,
                    io_error_rate: 0.03,
                    crash_on_write_rate: 0.01,
                },
                index_batch_entries: Some(5),
                ..Scenario::base("chaos", "Everything at once on four devices.", 4)
            },
        ]
    }

    pub fn by_name(name: &str) -> Option<Scenario> {
        Scenario::all().into_iter().find(|s| s.name == name)
    }
}
