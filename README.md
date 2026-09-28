# gearbox

`gearbox` 是一个使用 Rust 开发的个人 AI Agent Runtime，目标是让长程任务能够按可扩展的工作流持续执行、验证和恢复。

项目以基础 Agent Loop 为起点，并通过统一的插件机制扩展工具和工作流节点。框架只规定 **Agent Loop Node** 与 **Control Node** 两类节点的基本契约，提供必备的内置实现，主要由用户自定义具体节点。Agent Loop Node 调用模型处理任务；Control Node 执行编译、测试等检查，并按程序规则路由到下一节点或结束。Runtime 还负责管理树型、多分支的会话记录，允许从历史记录处分叉、切换并继续执行。详见 [gearbox 定位](doc/gearbox-positioning.md)、[工作流节点模型](doc/design/workflow-node-model.md)、[会话分支设计](doc/design/session-branching.md)。

当前实现以代码和测试为准：单进程 Rust CLI 通过 Responses API 调用模型，串行执行唯一的 `read_file` 工具，再将结果回传模型，直到收到最终回答或达到请求上限。历史仅保存在内存中，不保存完整对话日志。插件、工作流编排、树型会话记录、持久化和恢复尚未实现。

## 运行演示

安装 Rust 工具链后，在仓库目录使用 PowerShell：

```powershell
if (-not (Test-Path -LiteralPath config.toml)) {
    Copy-Item -LiteralPath config.example.toml -Destination config.toml
}
notepad config.toml
```

在 `config.toml` 中填写 `providers.<name>.api_key`。可以先只配置 provider，再运行模型同步命令：

```powershell
cargo run -- update-models --config=./config.toml
```

命令会从每个 provider 的模型目录获取模型，并把新的 `[[models]]` 配置追加到当前配置文件；新模型的 `reasoning` 默认是 `false`，需要时可以手动调整。配置中已有模型不会被删除或覆盖。完成后运行：

```powershell
cargo run -- run --workspace=./examples/demo --task="读取 index.txt，找到其中指定的项目资料文件，再读取该文件，告诉我项目名称和当前阶段。" --config=./config.toml
```

`run` 是当前默认命令，也可以省略。所有选项都使用 `--name=value` 形式；`--help` 可查看顶层帮助，`run --help` 与 `update-models --help` 可查看各命令帮助。

任务先读取索引，再根据索引读取资料；最终回答应包含“Gearbox Demo”和“最小 Loop 原型”。stdout 仅输出最终回答；stderr 显示模型请求轮次、工具名、读取路径和停止原因。收到最终回答返回退出码 0；配置、请求、协议错误或额度耗尽返回非零。

## 配置

配置可通过各命令的 `--config=PATH` 选项显式指定；未指定时依次查找 workspace 下的 `config.toml`、用户配置目录，最后使用内置默认配置（仍需提供有效 API Key）。例如：`gearbox run --workspace=./examples/demo --task="读取 index.txt" --config=./my.toml`。本地 `config.toml` 已加入 Git 忽略；已有旧版 `[model]` 配置需要迁移为 `[providers.<name>]` 与 `[[models]]`。

| 字段 | 含义 | 模板值 |
| --- | --- | --- |
| `providers.<name>.base_url` | provider 的 HTTP(S) 基地址，不含 `/responses` 或 `/models` | `https://api.openai.com/v1` |
| `providers.<name>.protocol` | 协议适配器 | `openai-responses` |
| `providers.<name>.api_key` | 网关 API Key，供该 provider 的所有模型共享 | 空，需填写 |
| `providers.<name>.discovery` | 可选的模型目录发现和冲突策略 | `models`、`warn`、`use-config` |
| `models[].provider` | 引用 provider 名称；可先不配置，使用 `update-models` 生成 | `example` |
| `models[].id` | 网关接受的模型标识 | `gpt-6-astra` |
| `models[].reasoning` | 模型是否支持思考 | `true` |
| `limits.max_model_requests` | 一次运行最多发起的模型请求次数 | `8` |
| `limits.request_timeout_secs` | 每次请求的超时秒数 | `60` |
| `limits.max_output_tokens` | 每次响应的输出 token 上限 | `8192` |
| `limits.max_file_bytes` | 单次读取的文件字节上限 | `16384` |

模板数值是初值，运行限制以配置为准。程序不自动重试；最后一次允许的请求若仍包含工具调用，完成该批只读调用后报告额度耗尽。配置非法会在启动时失败，错误不回显密钥或配置原文。模型目录发现只用于校验已配置模型的状态；缺失、弃用和发现失败分别按 provider 中的策略处理。网关若使用其他模型 ID，修改 `models[].id`，或重新运行 `update-models`。

`read_file` 仅接受工具工作目录内的普通 UTF-8 文件，拒绝绝对路径、目录越界、超限文件及本次加载的配置文件（包括指向它的链接）。文件内容会发送到所配置的模型服务；当前原型用于用户控制的小型演示目录，路径检查不提供操作系统沙箱。

## 验证

```powershell
cargo fmt --check
cargo check
cargo test
```

2026-09-28 本地验证：`cargo fmt --check`、`cargo check --locked --offline`、`cargo test --locked --offline` 均通过，共 31 个单元测试和 12 个 CLI＋本地 HTTP stub 集成测试，包括命令行语法、模型目录写回、配置硬链接保护及请求额度边界。

真实服务演示尚未完成：本地 `config.toml` 的 provider API Key 为空，演示命令在启动校验阶段返回非零，未发出模型请求。填入密钥后运行上述演示，确认实际读取两份文件且回答正确，再记录 provider、模型和简短运行摘要；不记录密钥或完整对话。Step 0 的真实端到端验收仍待完成。

## License

本项目采用 [MIT License](LICENSE)。
