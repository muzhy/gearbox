//! Command-line input parsing and help output.

use std::{error::Error as StdError, ffi::OsString, fmt, path::PathBuf};

use anyhow::Result;

#[derive(Debug)]
pub(crate) enum Command {
    Run(RunOptions),
    UpdateModels(UpdateModelsOptions),
}

#[derive(Debug)]
pub(crate) struct RunOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) task: String,
    pub(crate) config: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct UpdateModelsOptions {
    pub(crate) config: Option<PathBuf>,
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

const TOP_HELP: &str = "Usage: gearbox [command] [options]

Commands:
  run            Run one agent session (default)
  update-models  Fetch provider models and update the configuration

Use gearbox <command> --help for command options.
";
const RUN_HELP: &str = "Usage: gearbox [run] --workspace=PATH --task=TEXT [--config=PATH] [--help]

Options:
  --workspace=PATH  Workspace directory (required)
  --task=TEXT       Task to execute (required)
  --config=PATH     Configuration file path
  --help            Show this command's help

Example:
  gearbox run --workspace=./examples/demo --task=\"读取 index.txt\"
";
const UPDATE_MODELS_HELP: &str = "Usage: gearbox update-models [--config=PATH] [--help]

Options:
  --config=PATH  Configuration file path (search current and user config if omitted)
  --help         Show this command's help

Existing model settings are preserved; newly discovered models are appended.

Example:
  gearbox update-models --config=./config.toml
";

#[derive(Clone, Copy)]
enum CommandName {
    Run,
    UpdateModels,
}

impl CommandName {
    fn name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::UpdateModels => "update-models",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Self::Run => RUN_HELP,
            Self::UpdateModels => UPDATE_MODELS_HELP,
        }
    }
}

fn usage_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CliEarlyExit {
        output: format!("{}\nUse gearbox --help for usage.\n", message.into()),
        success: false,
    })
}

fn help(text: &str) -> anyhow::Error {
    anyhow::Error::new(CliEarlyExit {
        output: text.to_owned(),
        success: true,
    })
}

pub(crate) fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let args = args
        .into_iter()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| usage_error("command-line arguments must be valid Unicode"))
        })
        .collect::<Result<Vec<_>>>()?;

    if args.len() == 1 && args[0] == "--help" {
        return Err(help(TOP_HELP));
    }

    let (command, first_option) = match args.first() {
        Some(first) if first.starts_with("--") => (CommandName::Run, 0),
        Some(first) if first.starts_with(|c: char| c.is_ascii_alphabetic()) => {
            let command = match first.as_str() {
                "run" => CommandName::Run,
                "update-models" => CommandName::UpdateModels,
                _ => return Err(usage_error("unknown command at argument 1")),
            };
            (command, 1)
        }
        Some(_) => return Err(usage_error("invalid command at argument 1")),
        None => (CommandName::Run, 0),
    };

    let mut help_requested = false;
    let mut config = None;
    let mut workspace = None;
    let mut task = None;

    for (index, argument) in args.iter().enumerate().skip(first_option) {
        let Some(option) = argument.strip_prefix("--") else {
            let kind = if argument.starts_with(|c: char| c.is_ascii_alphabetic()) {
                "unexpected command"
            } else {
                "invalid argument"
            };
            return Err(usage_error(format!(
                "{kind} at argument {} for {}",
                index + 1,
                command.name()
            )));
        };
        let (name, value) = option
            .split_once('=')
            .map_or((option, None), |(name, value)| (name, Some(value)));
        match name {
            "help" => {
                if value.is_some() {
                    return Err(usage_error("--help does not take a value"));
                }
                if help_requested {
                    return Err(usage_error("--help was provided more than once"));
                }
                help_requested = true;
            }
            "config" => set_path_option(&mut config, "config", value)?,
            "workspace" if matches!(command, CommandName::Run) => {
                set_path_option(&mut workspace, "workspace", value)?;
            }
            "task" if matches!(command, CommandName::Run) => {
                let value = required_value("task", value)?;
                if value.trim().is_empty() {
                    return Err(usage_error("--task must not be empty"));
                }
                if task.replace(value.to_owned()).is_some() {
                    return Err(usage_error("--task was provided more than once"));
                }
            }
            _ => {
                return Err(usage_error(format!(
                    "unknown option --{name} for {}",
                    command.name()
                )));
            }
        }
    }

    if help_requested {
        return Err(help(command.help()));
    }

    match command {
        CommandName::Run => Ok(Command::Run(RunOptions {
            workspace: workspace.ok_or_else(|| usage_error("--workspace is required for run"))?,
            task: task.ok_or_else(|| usage_error("--task is required for run"))?,
            config,
        })),
        CommandName::UpdateModels => Ok(Command::UpdateModels(UpdateModelsOptions { config })),
    }
}

fn required_value<'a>(name: &str, value: Option<&'a str>) -> Result<&'a str> {
    value.ok_or_else(|| usage_error(format!("--{name} requires =VALUE")))
}

fn set_path_option(slot: &mut Option<PathBuf>, name: &str, value: Option<&str>) -> Result<()> {
    let value = required_value(name, value)?;
    if value.is_empty() {
        return Err(usage_error(format!("--{name} must not be empty")));
    }
    if slot.replace(PathBuf::from(value)).is_some() {
        return Err(usage_error(format!("--{name} was provided more than once")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(input: &[&str]) -> Result<Command> {
        parse_args(input.iter().map(OsString::from))
    }

    fn error(input: &[&str]) -> String {
        let error = parse(input).unwrap_err();
        let exit = error.downcast_ref::<CliEarlyExit>().unwrap();
        assert!(!exit.success);
        exit.output.clone()
    }

    #[test]
    fn explicit_and_default_run_use_the_same_options() {
        for input in [
            vec![
                "run",
                "--workspace=demo",
                "--task=read index.txt",
                "--config=custom.toml",
            ],
            vec![
                "--workspace=demo",
                "--task=read index.txt",
                "--config=custom.toml",
            ],
        ] {
            let Command::Run(options) = parse(&input).unwrap() else {
                panic!("expected run command");
            };
            assert_eq!(options.workspace, Path::new("demo"));
            assert_eq!(options.task, "read index.txt");
            assert_eq!(options.config.as_deref(), Some(Path::new("custom.toml")));
        }
    }

    #[test]
    fn update_models_has_its_own_options() {
        let Command::UpdateModels(options) =
            parse(&["update-models", "--config=custom.toml"]).unwrap()
        else {
            panic!("expected update-models command");
        };
        assert_eq!(options.config.as_deref(), Some(Path::new("custom.toml")));
        assert!(matches!(
            parse(&["update-models"]).unwrap(),
            Command::UpdateModels(_)
        ));
        assert!(error(&["update-models", "--task=read"]).contains("unknown option --task"));
    }

    #[test]
    fn help_is_common_and_skips_required_run_options() {
        for (input, expected) in [
            (vec!["--help"], "Commands:"),
            (vec!["run", "--help"], "--workspace=PATH"),
            (vec!["update-models", "--help"], "--config=PATH"),
        ] {
            let error = parse(&input).unwrap_err();
            let exit = error.downcast_ref::<CliEarlyExit>().unwrap();
            assert!(exit.success);
            assert!(exit.output.contains(expected));
        }
    }

    #[test]
    fn rejects_arguments_outside_the_documented_grammar() {
        for input in [
            vec!["run", "demo", "--task=read"],
            vec!["run", "update-models", "--task=read"],
            vec!["./demo", "--task=read"],
            vec!["-h"],
            vec!["help"],
            vec!["--"],
            vec!["run", "--config", "custom.toml"],
            vec!["run", "--workspace=demo", "--task=read", "--unknown=value"],
        ] {
            assert!(parse(&input).is_err(), "input must fail: {input:?}");
        }
    }

    #[test]
    fn rejects_missing_empty_or_repeated_options() {
        for input in [
            vec![],
            vec!["run", "--workspace=demo"],
            vec!["run", "--task=read"],
            vec!["run", "--workspace=", "--task=read"],
            vec!["run", "--workspace=demo", "--task= "],
            vec!["run", "--workspace=demo", "--task=read", "--task=again"],
            vec!["update-models", "--config="],
            vec!["update-models", "--config=one", "--config=two"],
            vec!["run", "--workspace=demo", "--task=read", "--help=value"],
        ] {
            assert!(parse(&input).is_err(), "input must fail: {input:?}");
        }
    }

    #[test]
    fn values_keep_equals_and_are_never_echoed_in_errors() {
        let Command::Run(options) = parse(&["--workspace=demo", "--task=read=a=b"]).unwrap() else {
            panic!("expected run command");
        };
        assert_eq!(options.task, "read=a=b");
        assert!(
            !error(&[
                "run",
                "--task=sensitive-key",
                "--workspace=demo",
                "--task=again"
            ])
            .contains("sensitive-key")
        );
    }
}
