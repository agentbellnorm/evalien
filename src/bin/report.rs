//! The report's composition root: read a stored run and check its quality.
//!
//! usage: report [db] [--timeline] [--last]

use std::{collections::HashMap, path::PathBuf, process::ExitCode};

use evalien::{
    config,
    db::{self, Access, SqliteStore},
    domain::{Event, TRAJECTORY_TABLE, Trajectory},
    evaluation::SandboxHost,
    harness::context::render_trajectory,
    quality,
};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (flags, paths): (Vec<&String>, Vec<&String>) = args.iter().partition(|arg| arg.starts_with("--"));
    if flags.iter().any(|flag| *flag != "--timeline" && *flag != "--last") || paths.len() > 1 {
        eprintln!("usage: report [db] [--timeline] [--last]");
        return ExitCode::from(64);
    }
    let flag = |name: &str| flags.iter().any(|flag| *flag == name);
    let path = paths
        .first()
        .map_or_else(|| config::db_path(&config::environment()), PathBuf::from);
    match report(path, flag("--timeline"), flag("--last")).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("report: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Print the report. Ok(false) means a check failed.
async fn report(path: PathBuf, timeline: bool, last: bool) -> anyhow::Result<bool> {
    let store = SqliteStore::new(db::open(&path, Access::ReadOnly)?, TRAJECTORY_TABLE)?;
    let all = Trajectory::new(store)?.read_all()?;
    let entries = if last { quality::last_run(&all) } else { &all[..] };

    // Whether each distinct source compiles, checked in the same runtime the agent uses.
    let mut checker = SandboxHost::new()?.checker().await?;
    let mut parsed: HashMap<&str, bool> = HashMap::new();
    for entry in entries {
        if let Event::Eval { code } = &entry.event
            && !parsed.contains_key(code.as_str())
        {
            parsed.insert(code, checker.parses(code).await?);
        }
    }
    let report = quality::analyze(entries, &|code| parsed.get(code).copied().unwrap_or(false));

    // The timeline is the trajectory as the model sees it, calls included.
    if timeline {
        println!("{}", render_trajectory(entries));
    }
    print!("{}", quality::render(&report));
    Ok(report.passed())
}
