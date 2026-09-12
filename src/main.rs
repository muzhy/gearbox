mod agent;
mod config;
mod model;
mod tool;

use std::{fs, process::ExitCode};

use anyhow::{Context, Result, ensure};
use config::{
    cli::{CliEarlyExit, parse_args},
    file::{LoggingConfig, load_config_for},
};
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
    // ? 取出解析后的参数，失败时从当前函数返回错误，后续代码不再执行
    // skip(1) 跳过第一个参数，通常是程序的路径
    // 解析命令行函数
    let args = parse_args(std::env::args_os().skip(1))?;
    let workspace = args.workspace;
    let task = args.task;
    let explicit_config = args.config;
    // 确认工作目录是否存在并且是一个目录
    let workspace =
        fs::canonicalize(workspace).context("cannot resolve the workspace directory")?;
    ensure!(workspace.is_dir(), "workspace must be a directory");
    // 加载配置文件
    let (config, config_path) = load_config_for(&workspace, explicit_config.as_deref())?;
    let _logging_guard = init_logging(&config.logging);

    let client = model::ModelClient::new(&config.model, &config.limits)?;
    let tool = tool::FileTool::new(workspace, config_path, config.limits.max_file_bytes);
    agent::run(&client, &tool, &task, config.limits.max_model_requests).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(answer) => {
            println!("{answer}");
            tracing::info!("stopped: final answer received");
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
