# gearbox 定位

`gearbox` 面向长程任务设计，提供基础的 Agent Runtime，支持通过插件进行扩展，支持工作流编排，负责历史会话管理。

本文是 `doc/` 的入口。它说明项目定位，并汇总具体设计决策的入口。具体行为以代码、测试和相关设计文档为准。

## 具体设计

- [命令行参数设计](design/cli-arguments.md)：命令选择、参数语法和错误处理。
- [工作流节点模型](design/workflow-node-model.md)：工作流节点的基本类型和职责。
- [工作流路由](design/workflow-routing.md)：检查结果、路由规则和停止条件。
- [插件扩展](design/plugin-extension.md)：插件提供节点与工具时应遵循的扩展边界。
- [会话检查点](design/session-checkpoints.md)：调用事件、检查点粒度和可恢复状态。
- [会话分支](design/session-branching.md)：历史分叉的条件、继承关系和继续执行语义。
- [分支重新执行](design/branch-reexecution.md)：从旧分叉点重新执行时的环境状态要求。

## 阅读约定

每个设计文件集中说明一个设计决策或一个紧密相关的问题。设计文件中的“待定”内容仍需通过实现和验证确定，不能视为已经提供的功能。修改实现时先阅读相关代码和测试，再判断设计文档是否需要同步更新。
