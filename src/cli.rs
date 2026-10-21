//! Command line parsing.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Colour policy for the live progress line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Decide from the environment (NO_COLOR, TERM, TTY)
    Auto,
    /// Always use colour
    Always,
    /// Never use colour
    Never,
}

/// Camera card copy tool: sy mirror copy plus XXH64 verification.
#[derive(Debug, Parser)]
#[command(
    name = "rcca",
    version,
    about = "Camera card copy tool: sy mirror copy plus XXH64 verification",
    long_about = None,
    after_help = "Exit codes: 0 success; 1 copy or verify failure; 2 usage error; 3 preflight failure; 130 interrupted."
)]
pub struct Cli {
    /// Source directory (card root)
    pub source: PathBuf,

    /// Target directory (created when missing)
    pub target: PathBuf,

    /// Keep the source directory name in the target layout (target/<source name>/...)
    #[arg(long)]
    pub keep_source_name: bool,

    /// Skip unreadable source files instead of failing the preflight
    #[arg(long)]
    pub skip_unreadable: bool,

    /// Continue even when the target looks too small
    #[arg(long)]
    pub force: bool,

    /// Abort on the first failure
    #[arg(long)]
    pub fail_fast: bool,

    /// Do not draw the progress line, print the summary only
    #[arg(long, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Keep and print the complete failure list instead of the first few
    #[arg(long, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Keep mismatching target files as .badhash instead of deleting them
    #[arg(long)]
    pub keep_bad: bool,

    /// Bypass the page cache while hashing so hashes come from the device
    #[arg(long)]
    pub verify_no_cache: bool,

    /// Remove leftover .sy-*.tmp files from the target before starting
    #[arg(long)]
    pub clean_stale: bool,

    /// Number of parallel file transfers passed to sy (sy's -j), 1 means serial
    #[arg(short = 'j', long, value_name = "N", default_value_t = 1, value_parser = parse_jobs)]
    pub jobs: usize,

    /// Colour policy for the progress line (defaults to NO_COLOR, TERM and TTY)
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,
}

/// Reject zero and non-numbers: sy needs at least one transfer slot.
fn parse_jobs(raw: &str) -> Result<usize, String> {
    let jobs: usize = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a positive number"))?;
    if jobs == 0 {
        return Err("jobs must be at least 1".to_string());
    }
    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_defaults() {
        let cli = Cli::try_parse_from(["rcca", "src", "dst"]).unwrap();
        assert_eq!(cli.color, ColorChoice::Auto);
        assert_eq!(cli.jobs, 1);
        assert!(!cli.keep_source_name);
        assert!(!cli.skip_unreadable);
        assert!(!cli.fail_fast);
        assert!(!cli.quiet);
        assert!(!cli.verbose);
    }

    #[test]
    fn color_flag_parses() {
        let cli = Cli::try_parse_from(["rcca", "src", "dst", "--color", "never"]).unwrap();
        assert_eq!(cli.color, ColorChoice::Never);
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--color", "sometimes"]).is_err());
    }

    #[test]
    fn jobs_flag_parses_both_spellings() {
        let long = Cli::try_parse_from(["rcca", "src", "dst", "--jobs", "8"]).unwrap();
        assert_eq!(long.jobs, 8);
        let short = Cli::try_parse_from(["rcca", "src", "dst", "-j", "4"]).unwrap();
        assert_eq!(short.jobs, 4);
    }

    #[test]
    fn jobs_rejects_zero_and_garbage() {
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "-j", "0"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "-j", "many"]).is_err());
    }

    #[test]
    fn conflicting_flags_rejected() {
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--quiet", "--verbose"]).is_err());
    }

    #[test]
    fn removed_engine_flags_are_rejected() {
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--chunk", "4M"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--serial-io"]).is_err());
        assert!(Cli::try_parse_from(["rcca", "src", "dst", "--no-fsync"]).is_err());
    }
}
