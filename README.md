# gearbox

`gearbox` 是一个使用 Rust 开发的个人 AI Agent 项目，面向长程、复杂任务的稳定执行。

长远计划以 **Graph + 状态机 + 主 Agent 协调 + 节点级 SubAgent** 为核心架构，把一个持续很久的任务拆成可追踪、可恢复、可验证的执行单元，降低上下文丢失以及模型对 Prompt 遵循能力随任务推进而下降带来的影响。

当前实现 [Step 0：最小 Agent Loop](doc/step-0-最小Agent-Loop实现方案.md)：单进程 Rust CLI，通过 Responses API 调用模型，串行执行唯一的 `read_file` 工具，再将结果回传模型，直到收到最终回答或达到请求上限。历史仅保存在内存中，不保存完整对话日志。Graph、持久化、恢复和多 Agent 留待后续阶段。

## 运行演示

安装 Rust 工具链后，在仓库目录使用 PowerShell：

```powershell
if (-not (Test-Path -LiteralPath config.toml)) {
    Copy-Item -LiteralPath config.example.toml -Destination config.toml
}
notepad config.toml
```

在 `config.toml` 中填写 `model.api_key`，确认端点和模型名后运行：

```powershell
cargo run -- ./examples/demo "读取 index.txt，找到其中指定的项目资料文件，再读取该文件，告诉我项目名称和当前阶段。"
```

任务先读取索引，再根据索引读取资料；最终回答应包含“Gearbox Demo”和“最小 Loop 原型”。stdout 仅输出最终回答；stderr 显示模型请求轮次、工具名、读取路径和停止原因。收到最终回答返回退出码 0；配置、请求、协议错误或额度耗尽返回非零。

## 配置

配置可通过 `--config <path>` 显式指定；未指定时依次查找 workspace 下的 `config.toml`、`~/gerabox/config.toml`，最后使用内置默认配置（仍需提供有效 API Key）。例如：`gearbox --config ./my.toml ./examples/demo "读取 index.txt"`。本地 `config.toml` 已加入 Git 忽略。

| 字段 | 含义 | 模板值 |
| --- | --- | --- |
| `model.endpoint` | 完整 Responses 请求 URL，直接使用，不追加路径 | `https://fireware.ai.ugreencloud.com/v1/responses` |
| `model.api_key` | 网关 API Key | 空，需填写 |
| `model.name` | 网关接受的模型标识 | `gpt-6-astra` |
| `model.reasoning_effort` | 推理强度：`low`、`medium`、`high`、`xhigh`、`max`、`ultra` | `low` |
| `limits.max_model_requests` | 一次运行最多发起的模型请求次数 | `8` |
| `limits.request_timeout_secs` | 每次请求的超时秒数 | `60` |
| `limits.max_output_tokens` | 每次响应的输出 token 上限 | `8192` |
| `limits.max_file_bytes` | 单次读取的文件字节上限 | `16384` |

模板数值是初值，运行限制以配置为准。程序不自动重试；最后一次允许的请求若仍包含工具调用，完成该批只读调用后报告额度耗尽。配置非法会在启动时失败，错误不回显密钥或配置原文。网关若使用其他模型别名，修改 `model.name`。

`read_file` 仅接受工具工作目录内的普通 UTF-8 文件，拒绝绝对路径、目录越界、超限文件及本次加载的配置文件（包括指向它的链接）。文件内容会发送到所配置的模型服务；当前原型用于用户控制的小型演示目录，路径检查不提供操作系统沙箱。

## 验证

```powershell
cargo fmt --check
cargo check
cargo test
```

2026-09-07 本地验证：`cargo fmt --check`、`cargo check --locked --offline`、`cargo test --locked --offline` 均通过，共 18 个单元测试和 11 个 CLI＋本地 HTTP stub 集成测试，包括 Windows 目录链接、配置硬链接保护及请求额度边界。

真实服务演示尚未完成：本地 `config.toml` 的 `model.api_key` 为空，演示命令在启动校验阶段返回非零，未发出模型请求。填入密钥后运行上述演示，确认实际读取两份文件且回答正确，再记录端点、模型和简短摘要；不记录密钥或完整对话。Step 0 的真实端到端验收仍待完成。

## License

本项目采用 [MIT License](LICENSE)。
