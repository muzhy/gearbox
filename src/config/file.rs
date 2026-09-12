use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use reqwest::{Url, header::HeaderValue};
use serde::{Deserialize, Deserializer, Serialize, de::Error};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) model: ModelConfig,
    pub(crate) limits: Limits,
    #[serde(default)]
    pub(crate) logging: LoggingConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoggingConfig {
    pub level: String,
    pub directory: PathBuf,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            directory: PathBuf::from("logs"),
        }
    }
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
    ensure!(
        !config.logging.level.trim().is_empty(),
        "logging.level must not be empty"
    );
    ensure!(
        config
            .logging
            .level
            .parse::<tracing_subscriber::filter::LevelFilter>()
            .is_ok(),
        "logging.level must be one of trace, debug, info, warn or error"
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

fn user_config_paths() -> Option<[PathBuf; 2]> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let home = PathBuf::from(home);
    Some([
        home.join(".gerabox").join("config.toml"),
        home.join(".gearbox").join("config.toml"),
    ])
}

pub(crate) fn load_config_for(
    workspace: &Path,
    explicit: Option<&Path>,
) -> Result<(Config, PathBuf)> {
    // 先尝试从显式指定的路径加载配置文件
    if let Some(path) = explicit {
        let loaded = load_config(path)?;
        tracing::info!(source = "explicit", path = %path.display(), "loaded configuration");
        return Ok(loaded);
    }
    // 如果没有显式指定路径，则尝试从工作区目录加载配置文件
    let workspace_config = workspace.join("config.toml");
    if workspace_config.is_file() {
        let loaded = load_config(&workspace_config)?;
        tracing::info!(source = "workspace", path = %workspace_config.display(), "loaded configuration");
        return Ok(loaded);
    }
    // 如果工作区目录没有配置文件，则尝试从用户目录加载配置文件
    if let Some(paths) = user_config_paths() {
        for path in paths {
            if path.is_file() {
                let loaded = load_config(&path)?;
                tracing::info!(source = "user", path = %path.display(), "loaded configuration");
                return Ok(loaded);
            }
        }
    }
    let config = parse_config(include_str!("../../config.example.toml"))
        .context("cannot use the built-in default configuration")?;
    tracing::info!(source = "built-in", "loaded configuration");
    Ok((config, workspace_config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> String {
        include_str!("../../config.example.toml")
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
}
