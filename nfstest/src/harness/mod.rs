mod runner;
mod types;

pub use runner::TestManager;
pub use types::*;

use colored::Colorize;

/// Parse version filter string into enum set.
pub fn parse_version_filter(s: &str) -> Vec<NfsVersion> {
    match s.to_lowercase().as_str() {
        "all" => vec![
            NfsVersion::V3,
            NfsVersion::V4_0,
            NfsVersion::V4_1,
            NfsVersion::V4_2,
        ],
        "3" | "v3" | "nfs3" => vec![NfsVersion::V3],
        "4.0" | "v4.0" | "nfs4.0" | "4" | "v4" => vec![NfsVersion::V4_0],
        "4.1" | "v4.1" | "nfs4.1" => vec![NfsVersion::V4_1],
        "4.2" | "v4.2" | "nfs4.2" => vec![NfsVersion::V4_2],
        _ => {
            eprintln!("Unknown version filter: {s}, using all");
            vec![
                NfsVersion::V3,
                NfsVersion::V4_0,
                NfsVersion::V4_1,
                NfsVersion::V4_2,
            ]
        }
    }
}

/// Parse layer filter string into enum set.
pub fn parse_layer_filter(s: &str) -> Vec<TestLayer> {
    match s.to_lowercase().as_str() {
        "all" => vec![
            TestLayer::Wire,
            TestLayer::Functional,
            TestLayer::Interop,
            TestLayer::Stress,
            TestLayer::Perf,
        ],
        "wire" => vec![TestLayer::Wire],
        "functional" => vec![TestLayer::Functional],
        "interop" => vec![TestLayer::Interop],
        "stress" => vec![TestLayer::Stress],
        "perf" => vec![TestLayer::Perf],
        _ => {
            eprintln!("Unknown layer filter: {s}, using all");
            vec![
                TestLayer::Wire,
                TestLayer::Functional,
                TestLayer::Interop,
                TestLayer::Stress,
                TestLayer::Perf,
            ]
        }
    }
}

/// List all available tests with filtering.
pub fn list_tests(
    version_filter: &[NfsVersion],
    layer_filter: &[TestLayer],
    tag_filter: &Option<String>,
) {
    let registry = crate::wire::registry();

    let mut count = 0;
    for test in &registry {
        if !version_filter.contains(&test.version) {
            continue;
        }
        if !layer_filter.contains(&test.layer) {
            continue;
        }
        if let Some(ref tag) = tag_filter {
            if !test.tags.iter().any(|t| t == tag) {
                continue;
            }
        }

        let version_str = format!("{:?}", test.version);
        let layer_str = format!("{:?}", test.layer);
        let tags_str = test.tags.join(", ");

        println!(
            "  {} {:>6} {:>12}  {}  [{}]",
            test.id.bold(),
            version_str.cyan(),
            layer_str.yellow(),
            test.description,
            tags_str.dimmed(),
        );
        count += 1;
    }

    println!("\n{} tests listed", count.to_string().bold());
}
