//! The `mallard-metrics import` subcommand.

use super::{Format, ImportError, Options, Policy, Report};
use crate::config::Config;
use crate::storage::parquet::ParquetStorage;
use duckdb::Connection;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

/// Exit code for a failure of any kind.
const EXIT_FAILURE: i32 = 1;
/// Exit code for a command line that cannot be understood.
const EXIT_USAGE: i32 = 2;

pub const USAGE: &str = "\
USAGE:
    mallard-metrics import <FORMAT> --site-id <ID> --file <PATH|-> [OPTIONS]

FORMATS:
    ga4-bigquery        Google Analytics 4 BigQuery export, newline-delimited JSON

OPTIONS:
    --site-id <ID>      Site the events are recorded under (required)
    --file <PATH|->     Input file, or - for standard input (required)
    --dry-run           Validate and count; open no database, write nothing
    --skip-invalid      Import the valid lines even if some are invalid
    --all-events        Also import the source's bookkeeping events
    --config <PATH>     Configuration file (default: MALLARD_* variables only)

The server must be stopped: the database file is locked while it runs.
Settings (data directory, privacy mode) are read exactly as the server reads them.";

/// Parsed command line.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub format: Format,
    pub site_id: String,
    pub file: String,
    pub dry_run: bool,
    pub skip_invalid: bool,
    pub all_events: bool,
    pub config: Option<PathBuf>,
}

/// Parse the arguments after `import`.
///
/// # Errors
///
/// A message describing the first problem.
pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut format = None;
    let (mut site_id, mut file, mut config) = (None, None, None);
    let (mut dry_run, mut skip_invalid, mut all_events) = (false, false, false);

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--site-id" => site_id = Some(value("--site-id")?),
            "--file" => file = Some(value("--file")?),
            "--config" => config = Some(PathBuf::from(value("--config")?)),
            "--dry-run" => dry_run = true,
            "--skip-invalid" => skip_invalid = true,
            "--all-events" => all_events = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option {other}"));
            }
            slug => {
                if format.is_some() {
                    return Err(format!("unexpected argument {slug}"));
                }
                format = Some(Format::from_slug(slug).ok_or_else(|| {
                    format!(
                        "unknown format {slug:?}; supported: {}",
                        Format::SLUGS.join(", ")
                    )
                })?);
            }
        }
    }

    Ok(Args {
        format: format.ok_or("missing FORMAT")?,
        site_id: site_id.ok_or("missing --site-id")?,
        file: file.ok_or("missing --file")?,
        dry_run,
        skip_invalid,
        all_events,
        config,
    })
}

/// The visitor-id secret, resolved as the server resolves it.
///
/// Used only as an HMAC key for imported ids, so that they cannot be linked to
/// anything without the instance's secret. The server's own rule is
/// `MALLARD_SECRET`, then `<data_dir>/.secret`, then a generated value that is
/// persisted; the same order is followed here so a first-ever import on a fresh
/// data directory leaves the secret the server will then pick up.
fn load_secret(config: &Config) -> Result<Vec<u8>, String> {
    if let Ok(secret) = std::env::var("MALLARD_SECRET") {
        let secret = secret.trim();
        if !secret.is_empty() {
            return Ok(secret.as_bytes().to_vec());
        }
    }
    let path = config.data_dir.join(".secret");
    match std::fs::read_to_string(&path) {
        Ok(existing) if !existing.trim().is_empty() => Ok(existing.trim().as_bytes().to_vec()),
        Ok(_) => generate_secret(&path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => generate_secret(&path),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn generate_secret(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let secret = uuid::Uuid::new_v4().to_string();
    crate::api::auth::write_private_file(path, secret.as_bytes()).map_err(|e| {
        format!(
            "cannot persist a visitor-ID secret at {}: {e}",
            path.display()
        )
    })?;
    Ok(secret.into_bytes())
}

fn open_input(file: &str) -> Result<Box<dyn BufRead>, String> {
    if file == "-" {
        return Ok(Box::new(BufReader::new(std::io::stdin().lock())));
    }
    std::fs::File::open(file)
        .map(|f| Box::new(BufReader::with_capacity(1 << 20, f)) as Box<dyn BufRead>)
        .map_err(|e| format!("cannot open {file}: {e}"))
}

fn print_errors(report: &Report) {
    for error in &report.errors {
        eprintln!("line {}: {}", error.line, error.message);
    }
    let unlisted = report.invalid.saturating_sub(report.errors.len() as u64);
    if unlisted > 0 {
        eprintln!("... and {unlisted} more invalid line(s)");
    }
}

/// Run the subcommand and return the process exit code.
pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    let args = match parse_args(args) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("import: {e}\n\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    match execute(&args) {
        Ok(report) => {
            print_errors(&report);
            print!("{}", report.render());
            0
        }
        Err(ImportError::Invalid(report)) => {
            print_errors(&report);
            eprintln!("import: {}", ImportError::Invalid(report.clone()));
            print!("{}", report.render());
            EXIT_FAILURE
        }
        Err(e) => {
            eprintln!("import: {e}");
            EXIT_FAILURE
        }
    }
}

fn options(args: &Args, config: &Config, key: Vec<u8>) -> Options {
    Options {
        format: args.format,
        site_id: args.site_id.clone(),
        key,
        policy: Policy::from_config(config),
        include_all_events: args.all_events,
        skip_invalid: args.skip_invalid,
        now_secs: chrono::Utc::now().timestamp(),
    }
}

fn execute(args: &Args) -> Result<Report, ImportError> {
    let loaded = Config::load(args.config.as_deref()).map_err(ImportError::Other)?;
    let config = loaded.config;
    config.validate().map_err(ImportError::Other)?;
    crate::api::stats::validate_site_id(&args.site_id)
        .map_err(|_| ImportError::Other(format!("{:?} is not a valid site id", args.site_id)))?;
    if !config.site_ids.is_empty() && !config.site_ids.contains(&args.site_id) {
        return Err(ImportError::Other(format!(
            "site {:?} is not in this instance's site_ids allowlist ({}); add it first",
            args.site_id,
            config.site_ids.join(", ")
        )));
    }

    let input = open_input(&args.file).map_err(ImportError::Other)?;

    if args.dry_run {
        // A dry run needs no secret and no database, so it also works while the
        // server is running. The key only shapes ids a dry run never shows.
        let opts = options(args, &config, b"dry-run".to_vec());
        let report = super::dry_run(input, &opts)?;
        // A dry run predicts the real run, which refuses invalid lines.
        return if report.invalid > 0 && !opts.skip_invalid {
            Err(ImportError::Invalid(Box::new(report)))
        } else {
            Ok(report)
        };
    }

    std::fs::create_dir_all(config.events_dir()).map_err(|e| {
        ImportError::Other(format!(
            "cannot create {}: {e}",
            config.events_dir().display()
        ))
    })?;
    let key = load_secret(&config).map_err(ImportError::Other)?;

    let conn = Connection::open(config.db_path()).map_err(|e| {
        ImportError::Other(format!(
            "cannot open {}: {e}\nIs the server still running? Stop it first; DuckDB allows \
             one process at a time.",
            config.db_path().display()
        ))
    })?;
    if let Some(limit) = &config.duckdb_memory_limit {
        conn.execute_batch(&format!(
            "SET memory_limit = '{}'",
            limit.replace('\'', "''")
        ))?;
    }
    if let Some(threads) = config.duckdb_threads {
        conn.execute_batch(&format!("SET threads = {threads}"))?;
    }
    let storage = ParquetStorage::new(&config.events_dir(), config.compact_after_files);
    super::prepare(&conn, &storage)?;
    super::import(&conn, &storage, input, &options(args, &config, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_a_full_command_line() {
        let parsed = parse_args(&args(&[
            "ga4-bigquery",
            "--site-id",
            "example.com",
            "--file",
            "-",
            "--dry-run",
            "--skip-invalid",
        ]))
        .unwrap();
        assert_eq!(parsed.format, Format::Ga4BigQuery);
        assert_eq!(parsed.site_id, "example.com");
        assert_eq!(parsed.file, "-");
        assert!(parsed.dry_run && parsed.skip_invalid && !parsed.all_events);
    }

    #[test]
    fn rejects_what_it_cannot_understand() {
        for (list, needle) in [
            (&["--site-id", "a", "--file", "f"][..], "missing FORMAT"),
            (&["ga4-bigquery", "--file", "f"][..], "missing --site-id"),
            (&["ga4-bigquery", "--site-id", "a"][..], "missing --file"),
            (
                &["plausible", "--site-id", "a", "--file", "f"][..],
                "unknown format",
            ),
            (&["ga4-bigquery", "--bogus"][..], "unknown option"),
            (&["ga4-bigquery", "--site-id"][..], "needs a value"),
        ] {
            let err = parse_args(&args(list)).unwrap_err();
            assert!(err.contains(needle), "{list:?} -> {err}");
        }
    }
}
