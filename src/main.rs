mod agent;
mod model;
mod tool;

use std::{
    error::Error as StdError,
    ffi::OsString,
    fmt, fs,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use argh::FromArgs;
use reqwest::{Url, header::HeaderValue};
use serde::{Deserialize, Deserializer, Serialize, de::Error};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    model: ModelConfig,
    limits: Limits,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelConfig {
    #[serde(deserialize_with = "deserialize_endpoint")]
    pub endpoint: Url,
    pub api_key: String,
    pub name: String,
    pub reasoning_effort: ReasoningEffort,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub max_model_requests: u32,
    pub request_timeout_secs: u64,
    pub max_output_tokens: u32,
    pub max_file_bytes: u64,
}

fn deserialize_endpoint<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Url, D::Error> {
    let text = String::deserialize(deserializer)?;
    Url::parse(&text).map_err(|_| D::Error::custom("invalid model.endpoint URL"))
}

fn parse_config(contents: &str) -> Result<Config> {
    // TOML/Serde diagnostics can quote source lines and values, including the key.
    let config: Config = toml::from_str(contents).map_err(|error: toml::de::Error| {
        let location = error.span().map(|span| {
            let line = contents.bytes().take(span.start).filter(|b| *b == b'\n').count() + 1;
            format!(" near line {line}")
        }).unwrap_or_default();
        anyhow::anyhow!(
            "invalid config.toml{location}: check TOML syntax, required fields, unknown fields and value types against config.example.toml"
        )
    })?;
    ensure!(
        matches!(config.model.endpoint.scheme(), "http" | "https")
            && config.model.endpoint.host_str().is_some()
            && config.model.endpoint.username().is_empty()
            && config.model.endpoint.password().is_none()
            && config.model.endpoint.fragment().is_none(),
        "model.endpoint must be a complete HTTP(S) URL without embedded credentials or a fragment"
    );
    ensure!(
        !config.model.name.trim().is_empty(),
        "model.name must not be empty"
    );
    ensure!(
        !config.model.api_key.trim().is_empty(),
        "model.api_key must not be empty; set it in config.toml"
    );
    ensure!(
        !config.model.api_key.chars().any(char::is_whitespace)
            && HeaderValue::from_str(&format!("Bearer {}", config.model.api_key)).is_ok(),
        "model.api_key must be a valid authentication token without whitespace"
    );
    ensure!(
        config.limits.max_model_requests > 0,
        "limits.max_model_requests must be positive"
    );
    ensure!(
        config.limits.max_output_tokens > 0,
        "limits.max_output_tokens must be positive"
    );
    ensure!(
        config.limits.request_timeout_secs > 0
            && config
                .limits
                .request_timeout_secs
                .checked_mul(1000)
                .is_some()
            && Instant::now()
                .checked_add(Duration::from_secs(config.limits.request_timeout_secs))
                .is_some(),
        "limits.request_timeout_secs must be positive and representable as both a deadline and milliseconds"
    );
    ensure!(
        config.limits.max_file_bytes > 0 && config.limits.max_file_bytes < isize::MAX as u64,
        "limits.max_file_bytes must be positive and allow one extra byte within the addressable buffer size"
    );
    Ok(config)
}

fn load_config(path: &Path) -> Result<(Config, PathBuf)> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("cannot find configuration file {}", path.display()))?;
    let contents = fs::read_to_string(&canonical).with_context(|| {
        format!(
            "cannot read configuration file {} as UTF-8 text",
            canonical.display()
        )
    })?;
    Ok((parse_config(&contents)?, canonical))
}

#[derive(FromArgs)]
/// Run an agent task in a workspace.
struct CliArgs {
    /// optional configuration file path
    #[argh(option, long = "config")]
    config: Option<PathBuf>,

    /// workspace directory
    #[argh(positional)]
    workspace: PathBuf,

    /// task to execute
    #[argh(positional)]
    task: String,
}

#[derive(Debug)]
struct CliEarlyExit {
    output: String,
    success: bool,
}

impl fmt::Display for CliEarlyExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.output.trim_end())
    }
}

impl StdError for CliEarlyExit {}

fn parse_args(
    args: impl IntoIterator<Item = OsString>,
) -> Result<(PathBuf, String, Option<PathBuf>)> {
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
    Ok((parsed.workspace, parsed.task, parsed.config))
}

fn user_config_paths() -> Option<[PathBuf; 2]> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let home = PathBuf::from(home);
    Some([
        home.join("gerabox").join("config.toml"),
        home.join("gearbox").join("config.toml"),
    ])
}

fn load_config_for(workspace: &Path, explicit: Option<&Path>) -> Result<(Config, PathBuf)> {
    if let Some(path) = explicit {
        return load_config(path);
    }
    let workspace_config = workspace.join("config.toml");
    if workspace_config.is_file() {
        return load_config(&workspace_config);
    }
    if let Some(paths) = user_config_paths() {
        for path in paths {
            if path.is_file() {
                return load_config(&path);
            }
        }
    }
    let config = parse_config(include_str!("../config.example.toml"))
        .context("cannot use the built-in default configuration")?;
    Ok((config, workspace_config))
}

async fn run() -> Result<String> {
    // ? 取出里面的元组，失败时从当前的函数返回错误，后续代码不再执行
    // skip(1) 跳过第一个参数，通常是程序的路径
    let (workspace, task, explicit_config) = parse_args(std::env::args_os().skip(1))?;
    let workspace =
        fs::canonicalize(workspace).context("cannot resolve the workspace directory")?;
    ensure!(workspace.is_dir(), "workspace must be a directory");
    let (config, config_path) = load_config_for(&workspace, explicit_config.as_deref())?;
    let client = model::ModelClient::new(&config.model, &config.limits)?;
    let tool = tool::FileTool::new(workspace, config_path, config.limits.max_file_bytes);
    agent::run(&client, &tool, &task, config.limits.max_model_requests).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(answer) => {
            println!("{answer}");
            eprintln!("Stopped: final answer received.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<CliEarlyExit>() {
                if exit.success {
                    println!("{}", exit.output.trim_end());
                    return ExitCode::SUCCESS;
                }
                eprint!("{}", exit.output);
                return ExitCode::FAILURE;
            }
            eprintln!("Stopped: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> String {
        include_str!("../config.example.toml")
            .replace("api_key = \"\"", "api_key = \"test-secret\"")
    }

    #[test]
    fn loads_config_from_explicit_path_and_preserves_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let text = valid_config()
            .replace("max_model_requests = 8", "max_model_requests = 2")
            .replace("request_timeout_secs = 60", "request_timeout_secs = 1")
            .replace("max_output_tokens = 8192", "max_output_tokens = 128")
            .replace("max_file_bytes = 16384", "max_file_bytes = 4");
        fs::write(&path, text).unwrap();
        let (config, canonical) = load_config(&path).unwrap();
        assert_eq!(canonical, fs::canonicalize(&path).unwrap());
        assert_eq!(config.limits.max_model_requests, 2);
        assert_eq!(config.limits.request_timeout_secs, 1);
        assert_eq!(config.limits.max_output_tokens, 128);
        assert_eq!(config.limits.max_file_bytes, 4);
        assert_eq!(config.model.name, "gpt-6-astra");
        assert_eq!(config.model.endpoint.path(), "/v1/responses");
    }

    #[test]
    fn configuration_selection_prefers_explicit_path_then_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let workspace_path = workspace.join("config.toml");
        fs::write(
            &workspace_path,
            valid_config().replace("max_model_requests = 8", "max_model_requests = 2"),
        )
        .unwrap();
        let explicit_path = temp.path().join("explicit.toml");
        fs::write(
            &explicit_path,
            valid_config().replace("max_model_requests = 8", "max_model_requests = 3"),
        )
        .unwrap();

        let (workspace_config, workspace_canonical) = load_config_for(&workspace, None).unwrap();
        assert_eq!(workspace_config.limits.max_model_requests, 2);
        assert_eq!(
            workspace_canonical,
            fs::canonicalize(&workspace_path).unwrap()
        );

        let (explicit_config, explicit_canonical) =
            load_config_for(&workspace, Some(&explicit_path)).unwrap();
        assert_eq!(explicit_config.limits.max_model_requests, 3);
        assert_eq!(
            explicit_canonical,
            fs::canonicalize(&explicit_path).unwrap()
        );
    }

    #[test]
    fn rejects_missing_invalid_or_unsupported_configuration_without_secrets() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load_config(&temp.path().join("missing.toml")).is_err());
        let valid = valid_config();
        let invalid = [
            "[model\napi_key = \"test-secret\"".to_owned(),
            valid.replace("api_key = \"test-secret\"", "api_key = \"\""),
            valid.replace("api_key = \"test-secret\"", "api_key = 123"),
            valid.replace("api_key = \"test-secret\"", "api_key = \"test secret\""),
            valid.replace("name = \"gpt-6-astra\"", "name = \" \""),
            valid.replace(
                "name = \"gpt-6-astra\"",
                "name = \"gpt-6-astra\"\nunknown = \"test-secret\"",
            ),
            valid.replace("name = \"gpt-6-astra\"\n", ""),
            valid.replace(
                "reasoning_effort = \"low\"",
                "reasoning_effort = \"test-secret\"",
            ),
            valid.replace(
                "https://fireware.ai.ugreencloud.com/v1/responses",
                "file:///test-secret",
            ),
            valid.replace(
                "https://fireware.ai.ugreencloud.com/v1/responses",
                "not-a-url-test-secret",
            ),
            valid.replace("max_model_requests = 8", "max_model_requests = 0"),
            valid.replace("max_model_requests = 8", "max_model_requests = 4294967296"),
            valid.replace("request_timeout_secs = 60", "request_timeout_secs = 0"),
            valid.replace(
                "request_timeout_secs = 60",
                "request_timeout_secs = 9223372036854775807",
            ),
            valid.replace("max_output_tokens = 8192", "max_output_tokens = -1"),
            valid.replace("max_output_tokens = 8192", "max_output_tokens = 0"),
            valid.replace("max_file_bytes = 16384", "max_file_bytes = 0"),
            valid.replace(
                "max_file_bytes = 16384",
                "max_file_bytes = 9223372036854775807",
            ),
        ];
        for (index, text) in invalid.into_iter().enumerate() {
            let error = parse_config(&text)
                .err()
                .unwrap_or_else(|| panic!("invalid config case {index} must fail"));
            assert!(!format!("{error:#}").contains("test-secret"));
        }
    }

    #[test]
    fn requires_exactly_two_arguments_and_a_nonempty_task() {
        assert!(parse_args(Vec::<OsString>::new()).is_err());
        assert!(parse_args(["demo".into(), " ".into()]).is_err());
        assert!(parse_args(["demo".into(), "task".into(), "extra".into()]).is_err());
        let (path, task, config) = parse_args(["demo".into(), "read index.txt".into()]).unwrap();
        assert_eq!(path, Path::new("demo"));
        assert_eq!(task, "read index.txt");
        assert!(config.is_none());
        let (_, _, config) = parse_args([
            "--config".into(),
            "custom.toml".into(),
            "demo".into(),
            "task".into(),
        ])
        .unwrap();
        assert_eq!(config, Some(PathBuf::from("custom.toml")));
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
