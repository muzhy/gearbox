mod agent;
mod cli;
mod config;
mod model;
mod tool;

use std::{collections::HashSet, fs, path::Path, process::ExitCode};

use anyhow::{Context, Result, ensure};
use cli::{CliEarlyExit, Command, RunOptions, UpdateModelsOptions, parse_args};
use config::file::{Config, LoggingConfig, load_config_for};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

fn init_logging(logging: &LoggingConfig) -> tracing_appender::non_blocking::WorkerGuard {
    let appender = tracing_appender::rolling::daily(&logging.directory, "gearbox.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false);
    let filter = EnvFilter::new(&logging.level);
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();
    guard
}

async fn run() -> Result<String> {
    match parse_args(std::env::args_os().skip(1))? {
        Command::Run(options) => RunCommand.exec(options).await,
        Command::UpdateModels(options) => UpdateModelsCommand.exec(options).await,
    }
}

trait CliCommand {
    type Options;

    async fn exec(&self, options: Self::Options) -> Result<String>;
}

struct RunCommand;

impl CliCommand for RunCommand {
    type Options = RunOptions;

    async fn exec(&self, options: RunOptions) -> Result<String> {
        run_session(options).await
    }
}

struct UpdateModelsCommand;

impl CliCommand for UpdateModelsCommand {
    type Options = UpdateModelsOptions;

    async fn exec(&self, options: UpdateModelsOptions) -> Result<String> {
        let workspace = fs::canonicalize(".").context("cannot resolve the current directory")?;
        let (config, config_path) = load_config_for(&workspace, options.config.as_deref())?;
        let _logging_guard = init_logging(&config.logging);
        update_models_config(&config, &config_path).await
    }
}

async fn run_session(options: RunOptions) -> Result<String> {
    let workspace =
        fs::canonicalize(options.workspace).context("cannot resolve the workspace directory")?;
    ensure!(workspace.is_dir(), "workspace must be a directory");
    let (config, config_path) = load_config_for(&workspace, options.config.as_deref())?;
    let _logging_guard = init_logging(&config.logging);

    let model = config
        .models
        .first()
        .ok_or_else(|| anyhow::anyhow!("no models configured; run gearbox update-models first"))?;
    let limits = config.limits_for(model);
    let provider = config.provider_for(model);
    model::validate_configured_model(model, provider, limits).await?;
    let client = model::ModelClient::new(model, provider, limits)?;
    let tool = tool::FileTool::new(workspace, config_path, limits.max_file_bytes);
    let answer = agent::run(&client, &tool, &options.task, limits.max_model_requests).await?;
    tracing::info!("stopped: final answer received");
    Ok(answer)
}

async fn update_models_config(config: &Config, config_path: &Path) -> Result<String> {
    ensure!(
        config_path.is_file(),
        "cannot update the built-in configuration; create a config.toml or pass --config=PATH"
    );
    let mut discovered = Vec::new();
    for (provider_name, provider) in &config.providers {
        let path = provider
            .discovery
            .as_ref()
            .map_or("models", |discovery| discovery.path.as_str());
        let models = model::fetch_provider_models(provider, path, &config.limits).await?;
        for model_id in models {
            discovered.push((provider_name.as_str(), model_id));
        }
    }

    let existing = config
        .models
        .iter()
        .map(|model| (model.provider.clone(), model.id.clone()))
        .collect::<HashSet<_>>();
    let mut seen = existing.clone();
    let additions = discovered
        .into_iter()
        .filter(|(provider, model)| seen.insert(((*provider).to_owned(), model.clone())))
        .collect::<Vec<_>>();
    if additions.is_empty() {
        return Ok(format!(
            "configuration already contains all discovered models: {}",
            config_path.display()
        ));
    }

    let mut contents = fs::read_to_string(config_path)
        .with_context(|| format!("cannot read configuration file {}", config_path.display()))?;
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    for (provider, model) in &additions {
        contents.push_str("\n[[models]]\nprovider = ");
        contents.push_str(&toml_string(provider));
        contents.push_str("\nid = ");
        contents.push_str(&toml_string(model));
        contents.push_str("\nreasoning = false\n");
    }
    fs::write(config_path, contents)
        .with_context(|| format!("cannot update configuration file {}", config_path.display()))?;
    Ok(format!(
        "added {} model(s) to {}",
        additions.len(),
        config_path.display()
    ))
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(answer) => {
            println!("{answer}");
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
            tracing::error!(error = ?error, "stopped with error");
            ExitCode::FAILURE
        }
    }
}
