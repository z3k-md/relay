//! `relay log` and `relay diag` (D56).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;
use relay_daemon::{diag, logging};
use relay_ipc::{Client, LogLevel, LogLevelParams};

/// A temporary level returns to the default after this long unless `--for`
/// says otherwise.
const DEFAULT_FOR: Duration = Duration::from_secs(60 * 60);

#[derive(Subcommand, Debug)]
pub enum LogCmd {
    /// Print the end of the log
    Show {
        #[arg(short = 'n', default_value_t = 50)]
        n: usize,
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Print or set the running host's log level: `debug`, `info`, or a full
    /// filter such as `info,relay_net=trace`. Returns to the default after
    /// `--for` (1h; `0` keeps it until Relay restarts).
    Level {
        filter: Option<String>,
        #[arg(long = "for", value_parser = parse_duration)]
        for_: Option<Duration>,
        /// Back to the default now
        #[arg(long, conflicts_with = "filter")]
        reset: bool,
    },
    /// Print the log file's path
    Path,
}

#[derive(Subcommand, Debug)]
pub enum DiagCmd {
    /// Interleave bundles into one timeline on the first bundle's clock
    Merge {
        #[arg(required = true)]
        bundles: Vec<PathBuf>,
        /// Write the timeline here instead of stdout
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
}

pub fn cmd_log(home: &Path, cmd: LogCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        LogCmd::Show { n, follow } => crate::service::cmd_logs(home, n, follow),
        LogCmd::Path => {
            println!("{}", logging::log_path(home).display());
            Ok(ExitCode::SUCCESS)
        }
        LogCmd::Level {
            filter,
            for_,
            reset,
        } => {
            let Some(mut client) = Client::connect(home)? else {
                bail!(
                    "Relay is not running; set RELAY_LOG={} before starting it instead",
                    filter.as_deref().unwrap_or("debug")
                );
            };
            let for_secs = match for_.unwrap_or(DEFAULT_FOR).as_secs() {
                0 => None,
                secs => Some(secs),
            };
            let level = client.log_level(&LogLevelParams {
                for_secs: filter.as_ref().and(for_secs),
                filter,
                reset,
            })?;
            print_level(&level, json)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn print_level(level: &LogLevel, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(level)?);
        return Ok(());
    }
    let until = match level.until_ms {
        Some(ms) => format!(" until {}", logging::format_utc(ms / 1000)),
        None if level.filter != level.default => " until Relay restarts".to_owned(),
        None => String::new(),
    };
    println!("log level: {}{until}", level.filter);
    if level.filter != level.default {
        println!("default: {}", level.default);
    }
    Ok(())
}

/// `30s`, `15m`, `2h`, `1d`, or plain seconds.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (digits, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as 30m or 2h"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return Err(format!("{text:?} is not a duration such as 30m or 2h")),
    };
    Ok(Duration::from_secs(secs))
}

pub fn cmd_diag(
    home: &Path,
    cmd: Option<DiagCmd>,
    out: Option<PathBuf>,
    all: bool,
    peers: Vec<String>,
    json: bool,
) -> Result<ExitCode> {
    if let Some(DiagCmd::Merge { bundles, out }) = cmd {
        return cmd_merge(&bundles, out.as_deref());
    }
    let pull = if all {
        diag::Pull::All
    } else if peers.is_empty() {
        diag::Pull::None
    } else {
        diag::Pull::Peers(&peers)
    };
    // One bundle may be named outright; several go into a folder.
    let (dir, own_name) = match out {
        Some(path)
            if matches!(pull, diag::Pull::None) && path.extension().is_some_and(|e| e == "zip") =>
        {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            (
                path.parent().map(Path::to_path_buf).unwrap_or_default(),
                name,
            )
        }
        Some(dir) => (dir, None),
        None => (PathBuf::from("."), None),
    };
    let done = diag::collect(home, &dir, own_name.as_deref(), pull)?;
    if done.nobody_to_pull {
        eprintln!(
            "no connected device lets this one manage it, so only this device's bundle was saved"
        );
    }
    for failure in &done.failures {
        eprintln!("{failure}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&done)?);
    } else {
        for path in &done.bundles {
            println!("saved {}", path.display());
        }
        if let Some(timeline) = &done.timeline {
            println!("timeline {}", timeline.display());
        }
    }
    Ok(if done.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn cmd_merge(bundles: &[PathBuf], out: Option<&Path>) -> Result<ExitCode> {
    let read = bundles
        .iter()
        .map(|path| diag::read(path))
        .collect::<Result<Vec<_>>>()?;
    let merged = diag::merge(&read)?;
    for note in &merged.notes {
        eprintln!("{note}");
    }
    let mut text = merged.lines.join("\n");
    text.push('\n');
    match out {
        Some(path) => {
            diag::write_new(path, text.as_bytes())?;
        }
        None => print!("{text}"),
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert!(parse_duration("1w").is_err());
        assert!(parse_duration("h").is_err());
    }
}
