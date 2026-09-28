use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use reqwest::{Url, header::HeaderValue};
use serde::{Deserialize, Deserializer, de::Error};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) providers: BTreeMap<String, ProviderConfig>,
    /// User-selected model profiles are configured in order.
    #[serde(default)]
    pub(crate) models: Vec<ModelConfig>,
    /// Defaults used by models without a model-specific limits table.
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
pub(crate) struct ProviderConfig {
    #[serde(deserialize_with = "deserialize_url")]
    pub(crate) base_url: Url,
    pub(crate) protocol: ProviderProtocol,
    pub(crate) api_key: String,
    /// When absent, the configured models are used without a catalog request.
    #[serde(default)]
    pub(crate) discovery: Option<ModelDiscoveryConfig>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProviderProtocol {
    OpenaiResponses,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelDiscoveryConfig {
    #[serde(default = "default_discovery_path")]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) on_missing: MissingModelPolicy,
    #[serde(default)]
    pub(crate) on_deprecated: DeprecatedModelPolicy,
    #[serde(default)]
    pub(crate) on_error: DiscoveryErrorPolicy,
}

fn default_discovery_path() -> String {
    "models".to_owned()
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MissingModelPolicy {
    Allow,
    #[default]
    Warn,
    Error,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DeprecatedModelPolicy {
    Ignore,
    #[default]
    Warn,
    Error,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DiscoveryErrorPolicy {
    #[default]
    UseConfig,
    Error,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelConfig {
    pub(crate) provider: String,
    pub(crate) id: String,
    /// Whether this model supports reasoning responses.
    pub(crate) reasoning: bool,
    /// `None` means that the global limits apply to this model.
    #[serde(default)]
    pub(crate) limits: Option<Limits>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub max_model_requests: u32,
    pub request_timeout_secs: u64,
    pub max_output_tokens: u32,
    pub max_file_bytes: u64,
}

impl Config {
    pub(crate) fn limits_for<'a>(&'a self, model: &'a ModelConfig) -> &'a Limits {
        model.limits.as_ref().unwrap_or(&self.limits)
    }

    pub(crate) fn provider_for<'a>(&'a self, model: &ModelConfig) -> &'a ProviderConfig {
        self.providers
            .get(&model.provider)
            .expect("configuration validation requires every model provider to exist")
    }
}

impl ProviderConfig {
    pub(crate) fn endpoint(&self, relative_path: &str) -> Url {
        let mut base = self.base_url.clone();
        let base_path = format!("{}/", base.path().trim_end_matches('/'));
        base.set_path(&base_path);
        base.join(relative_path.trim_start_matches('/'))
            .expect("configuration validation requires a valid relative endpoint path")
    }

    pub(crate) fn responses_endpoint(&self) -> Url {
        match self.protocol {
            ProviderProtocol::OpenaiResponses => self.endpoint("responses"),
        }
    }
}

fn deserialize_url<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Url, D::Error> {
    let text = String::deserialize(deserializer)?;
    Url::parse(&text).map_err(|_| D::Error::custom("invalid provider base URL"))
}

fn validate_limits(limits: &Limits, prefix: &str) -> Result<()> {
    ensure!(
        limits.max_model_requests > 0,
        "{prefix}.max_model_requests must be positive"
    );
    ensure!(
        limits.max_output_tokens > 0,
        "{prefix}.max_output_tokens must be positive"
    );
    ensure!(
        limits.request_timeout_secs > 0
            && limits.request_timeout_secs.checked_mul(1000).is_some()
            && Instant::now()
                .checked_add(Duration::from_secs(limits.request_timeout_secs))
                .is_some(),
        "{prefix}.request_timeout_secs must be positive and representable as both a deadline and milliseconds"
    );
    ensure!(
        limits.max_file_bytes > 0 && limits.max_file_bytes < isize::MAX as u64,
        "{prefix}.max_file_bytes must be positive and allow one extra byte within the addressable buffer size"
    );
    Ok(())
}

fn parse_config(contents: &str) -> Result<Config> {
    // TOML/Serde diagnostics can quote source lines and values, including the key.
    let config: Config = toml::from_str(contents).map_err(|error: toml::de::Error| {
        let location = error
            .span()
            .map(|span| {
                let line = contents.bytes().take(span.start).filter(|b| *b == b'\n').count() + 1;
                format!(" near line {line}")
            })
            .unwrap_or_default();
        anyhow::anyhow!(
            "invalid config.toml{location}: check TOML syntax, required fields, unknown fields and value types against config.example.toml"
        )
    })?;

    ensure!(
        !config.providers.is_empty(),
        "providers must contain at least one provider"
    );
    for (index, (name, provider)) in config.providers.iter().enumerate() {
        let prefix = format!("providers entry {index}");
        ensure!(!name.trim().is_empty(), "{prefix} name must not be empty");
        ensure!(
            matches!(provider.base_url.scheme(), "http" | "https")
                && provider.base_url.host_str().is_some()
                && provider.base_url.username().is_empty()
                && provider.base_url.password().is_none()
                && provider.base_url.query().is_none()
                && provider.base_url.fragment().is_none(),
            "{prefix}.base_url must be an HTTP(S) URL without credentials, a query or a fragment"
        );
        ensure!(
            !provider.api_key.trim().is_empty(),
            "{prefix}.api_key must not be empty; set it in config.toml"
        );
        ensure!(
            !provider.api_key.chars().any(char::is_whitespace)
                && HeaderValue::from_str(&format!("Bearer {}", provider.api_key)).is_ok(),
            "{prefix}.api_key must be a valid authentication token without whitespace"
        );
        if let Some(discovery) = &provider.discovery {
            let discovery_path = discovery.path.trim_start_matches('/');
            ensure!(
                valid_relative_endpoint(&discovery.path)
                    && provider.base_url.join(discovery_path).is_ok(),
                "{prefix}.discovery.path must be a non-empty model path without parent segments or a fragment"
            );
        }
    }

    validate_limits(&config.limits, "limits")?;

    for (index, model) in config.models.iter().enumerate() {
        let prefix = format!("models[{index}]");
        ensure!(
            !model.provider.trim().is_empty(),
            "{prefix}.provider must not be empty"
        );
        ensure!(
            config.providers.contains_key(&model.provider),
            "{prefix}.provider must reference a configured provider"
        );
        ensure!(!model.id.trim().is_empty(), "{prefix}.id must not be empty");
        if let Some(limits) = &model.limits {
            validate_limits(limits, &format!("{prefix}.limits"))?;
        }
    }

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

fn valid_relative_endpoint(path: &str) -> bool {
    let path_without_query = path.split_once('?').map_or(path, |(path, _)| path);
    let normalized = path_without_query.trim_start_matches('/');
    !path.trim().is_empty()
        && !path.starts_with("//")
        && !path.contains('#')
        && Url::parse(path).is_err()
        && !normalized
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
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
    if let Some(path) = explicit {
        let loaded = load_config(path)?;
        tracing::info!(source = "explicit", path = %path.display(), "loaded configuration");
        return Ok(loaded);
    }

    let workspace_config = workspace.join("config.toml");
    if workspace_config.is_file() {
        let loaded = load_config(&workspace_config)?;
        tracing::info!(source = "workspace", path = %workspace_config.display(), "loaded configuration");
        return Ok(loaded);
    }

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
        format!(
            "{}\n\n[[models]]\nprovider = \"example\"\nid = \"gpt-6-astra\"\nreasoning = true\n",
            include_str!("../../config.example.toml")
                .replace("api_key = \"\"", "api_key = \"test-secret\"")
        )
    }

    #[test]
    fn loads_multiple_models_and_inherits_global_limits() {
        let text = format!(
            "{}\n\n[[models]]\nprovider = \"example\"\nid = \"second\"\nreasoning = false\n",
            valid_config()
        );
        let config = parse_config(&text).unwrap();
        assert_eq!(config.models.len(), 2);
        assert!(config.models[0].limits.is_none());
        assert_eq!(
            config.limits_for(&config.models[0]).max_output_tokens,
            config.limits.max_output_tokens
        );
        assert_eq!(config.models[1].id, "second");
        assert_eq!(config.models[1].provider, "example");
        assert_eq!(config.providers.len(), 1);
    }

    #[test]
    fn model_limits_override_global_limits() {
        let text = valid_config().replace(
            "reasoning = true",
            "reasoning = true\n\n[models.limits]\nmax_model_requests = 2\nrequest_timeout_secs = 1\nmax_output_tokens = 128\nmax_file_bytes = 4",
        );
        let config = parse_config(&text).unwrap();
        let limits = config.limits_for(&config.models[0]);
        assert_eq!(limits.max_model_requests, 2);
        assert_eq!(limits.max_output_tokens, 128);
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
        assert_eq!(config.models[0].id, "gpt-6-astra");
        assert_eq!(config.models[0].provider, "example");
        assert_eq!(
            config.provider_for(&config.models[0]).base_url.path(),
            "/v1"
        );
        assert_eq!(
            config
                .provider_for(&config.models[0])
                .responses_endpoint()
                .path(),
            "/v1/responses"
        );
        assert_eq!(
            config
                .provider_for(&config.models[0])
                .endpoint("models")
                .path(),
            "/v1/models"
        );
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
            "[providers\napi_key = \"test-secret\"".to_owned(),
            valid.replace("api_key = \"test-secret\"", "api_key = \"\""),
            valid.replace("api_key = \"test-secret\"", "api_key = 123"),
            valid.replace("api_key = \"test-secret\"", "api_key = \"test secret\""),
            valid.replace("id = \"gpt-6-astra\"", "id = \" \""),
            valid.replace(
                "id = \"gpt-6-astra\"",
                "id = \"gpt-6-astra\"\nunknown = \"test-secret\"",
            ),
            valid.replace("id = \"gpt-6-astra\"\n", ""),
            valid.replace("reasoning = true", "reasoning = \"test-secret\""),
            valid.replace("https://api.openai.com/v1", "file:///test-secret"),
            valid.replace("https://api.openai.com/v1", "not-a-url-test-secret"),
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
            valid.replace("path = \"models\"", "path = \"../models\""),
            valid.replace("provider = \"example\"", "provider = \"missing\""),
        ];
        for (index, text) in invalid.into_iter().enumerate() {
            let error = parse_config(&text)
                .err()
                .unwrap_or_else(|| panic!("invalid config case {index} must fail"));
            assert!(!format!("{error:#}").contains("test-secret"));
        }
    }

    #[test]
    fn accepts_discovery_defaults_and_resolves_provider_endpoints() {
        let config = parse_config(&valid_config()).unwrap();
        let provider = config.provider_for(&config.models[0]);
        let discovery = provider.discovery.as_ref().unwrap();
        assert_eq!(discovery.path, "models");
        assert!(matches!(discovery.on_missing, MissingModelPolicy::Warn));
        assert!(matches!(
            discovery.on_deprecated,
            DeprecatedModelPolicy::Warn
        ));
        assert!(matches!(
            discovery.on_error,
            DiscoveryErrorPolicy::UseConfig
        ));
        assert_eq!(provider.responses_endpoint().path(), "/v1/responses");
        assert_eq!(provider.endpoint("models").path(), "/v1/models");
        assert_eq!(provider.endpoint("/models").path(), "/v1/models");
    }

    #[test]
    fn defaults_discovery_path_to_models() {
        let without_path = valid_config().replace("path = \"models\"\n", "");
        let config = parse_config(&without_path).unwrap();
        assert_eq!(
            config
                .provider_for(&config.models[0])
                .discovery
                .as_ref()
                .unwrap()
                .path,
            "models"
        );
    }

    #[test]
    fn accepts_empty_models_and_rejects_unknown_provider_references() {
        let no_models = r#"
[providers.example]
base_url = "https://api.openai.com/v1"
protocol = "openai-responses"
api_key = "test-secret"

[limits]
max_model_requests = 8
request_timeout_secs = 60
max_output_tokens = 8192
max_file_bytes = 16384
"#;
        assert!(parse_config(&no_models).is_ok());
        let unknown = valid_config().replace("provider = \"example\"", "provider = \"missing\"");
        assert!(parse_config(&unknown).is_err());
    }
}
