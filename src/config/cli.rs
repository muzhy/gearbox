use std::{error::Error as StdError, ffi::OsString, fmt, path::PathBuf};

use anyhow::{Result, ensure};
use argh::FromArgs;

#[derive(Debug, FromArgs)]
/// Run an agent task in a workspace.
pub(crate) struct CliArgs {
    /// optional configuration file path
    #[argh(option, long = "config")]
    pub(crate) config: Option<PathBuf>,

    /// workspace directory
    #[argh(positional)]
    pub(crate) workspace: PathBuf,

    /// task to execute
    #[argh(positional)]
    pub(crate) task: String,
}

#[derive(Debug)]
pub(crate) struct CliEarlyExit {
    pub(crate) output: String,
    pub(crate) success: bool,
}

impl fmt::Display for CliEarlyExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.output.trim_end())
    }
}

impl StdError for CliEarlyExit {}

pub(crate) fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<CliArgs> {
    let args = args
        .into_iter()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| anyhow::anyhow!("command-line arguments must be valid Unicode"))
        })
        .collect::<Result<Vec<_>>>()?;
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let parsed = CliArgs::from_args(&["gearbox"], &args).map_err(|error| CliEarlyExit {
        output: error.output,
        success: error.status.is_ok(),
    })?;
    ensure!(
        !parsed.workspace.as_os_str().is_empty(),
        "workspace must not be empty"
    );
    ensure!(!parsed.task.trim().is_empty(), "task must not be empty");
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn requires_exactly_two_arguments_and_a_nonempty_task() {
        assert!(parse_args(Vec::<OsString>::new()).is_err());
        assert!(parse_args(["demo".into(), " ".into()]).is_err());
        assert!(parse_args(["demo".into(), "task".into(), "extra".into()]).is_err());
        let parsed = parse_args(["demo".into(), "read index.txt".into()]).unwrap();
        assert_eq!(parsed.workspace, Path::new("demo"));
        assert_eq!(parsed.task, "read index.txt");
        assert!(parsed.config.is_none());
        let parsed = parse_args([
            "--config".into(),
            "custom.toml".into(),
            "demo".into(),
            "task".into(),
        ])
        .unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("custom.toml")));
    }

    #[test]
    fn argh_generates_help_and_rejects_unknown_options() {
        let help = parse_args(["--help".into()]).unwrap_err();
        let help = help.downcast_ref::<CliEarlyExit>().unwrap();
        assert!(help.success);
        assert!(help.output.contains("--config"));
        assert!(help.output.contains("<workspace>"));

        let invalid = parse_args(["--unknown".into()]).unwrap_err();
        let invalid = invalid.downcast_ref::<CliEarlyExit>().unwrap();
        assert!(!invalid.success);
    }
}
