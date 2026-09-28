# gearbox Agent 指南

## 项目范围

`gearbox` 是一个使用 Rust 编写的个人 AI Agent Runtime 命令行工具。开始处理任务前，请阅读 `README.md`、`Cargo.toml`、`src/`、`tests/` 和与任务相关的文档，以实际代码和文档为准，不要仅根据本文件推断实现状态。

## 仓库导航

- `src/`：Rust 源代码；修改前阅读与任务相关的模块。
- `tests/`：集成测试；单元测试也可能位于源代码附近。
- `examples/demo/`：示例工作区，可用于手动运行。
- `doc/`：项目定位和设计文档；使用时结合实际代码核对。
- `Cargo.toml`：项目元数据、依赖和构建配置。
- `config.example.toml`：可共享的配置模板。本地 `config.toml` 已被忽略，可能包含凭据。
- `.agent/`：预留给后续仓库专用的 Agent skill、配置和脚本。

## 在本仓库中工作

- 编辑前先检查工作区状态，并保留与当前任务无关的已有修改。
- 遵循现有 Rust 风格，保持修改范围聚焦。行为发生变化时，同时更新相关测试和文档。
- 将 CLI 输入、配置、模型响应和工具参数视为外部输入。保留文件工具的工作区边界检查和配置文件保护机制。
- 不要将凭据写入受 Git 跟踪的文件或测试输出。Provider 测试应使用本地 HTTP stub，不要依赖真实服务。
- 修改代码后运行 `cargo fmt --check`、`cargo check --locked` 和 `cargo test --locked`。如果无法访问依赖，使用对应的 `--offline` 选项，并说明实际完成的验证范围。
