# Provider 端点对照（2026-07）

> 本表与当前客户端源码 `src-tauri/src/installer/providers.rs` 对齐。厂商可能调整 URL、模型和套餐约束；每次发版都要重新核对官方文档和真实 Key。源码预置不等于生产连通验收已完成。

## Claude Code（Anthropic Messages 兼容）

| Provider | Base URL | 当前默认模型 | 官方资料 |
|---|---|---|---|
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro` | [Claude Code 集成](https://api-docs.deepseek.com/quick_start/agent_integrations/claude_code/) |
| 通义千问 | `https://dashscope.aliyuncs.com/apps/anthropic` | `qwen3.7-plus` | [Anthropic Messages 兼容](https://help.aliyun.com/zh/model-studio/anthropic-api-messages) |
| Kimi（国内） | `https://api.moonshot.cn/anthropic` | `kimi-k2.7-code` | [Claude Code 接入](https://platform.kimi.com/docs/guide/claude-code-kimi) |
| 智谱 GLM | `https://open.bigmodel.cn/api/anthropic` | `glm-5.2` | [Claude Code 接入](https://docs.bigmodel.cn/cn/guide/develop/claude) |
| MiniMax（国内） | `https://api.minimaxi.com/anthropic` | `MiniMax-M3` | [Anthropic API](https://platform.minimaxi.com/docs/api-reference/text-anthropic-api) |
| Anthropic 官方 | `https://api.anthropic.com` | `claude-sonnet-5` | [模型弃用与生命周期](https://platform.claude.com/docs/en/docs/about-claude/model-deprecations) |
| 自定义 Claude | 用户填写 | 必填 | 必须真实兼容 Anthropic Messages |

国际端点可通过自定义项填写，例如 Kimi `https://api.moonshot.ai/anthropic`、MiniMax `https://api.minimax.io/anthropic`。

## Codex（OpenAI Responses API）

| Provider | Base URL | 当前默认模型 | 约束 |
|---|---|---|---|
| 通义千问 | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `qwen3.7-plus` | 必须使用支持 Responses API 的按量付费端点；当前 Codex 路径不使用 Chat Completions |
| OpenAI 官方 | `https://api.openai.com/v1` | `gpt-5.6` | [Responses API 可用的当前推荐 alias](https://developers.openai.com/api/docs/models) |
| 自定义 Codex | 用户填写 | 必填 | 必须真实支持 `POST /responses`；“OpenAI 兼容”但只有 `/chat/completions` 不够 |

DeepSeek、Kimi、智谱的普通 OpenAI-compatible Chat Completions 端点**不在当前 Codex 预置表中**，避免测试误通过但 Codex 运行失败。

## 写入与恢复边界

- Key：写入 `~/.claude/codecli-installer/secrets.env`，Unix 权限要求 `0600`；Claude/Codex 方案均由该受管 secrets 文件保存。
- Claude：非敏感 Base URL/模型写受管 shell profile 块，同时字段级合并 `~/.claude/settings.json` 的 `env`，不覆盖用户 hooks 等无关字段。
- Codex：使用 `toml_edit` 结构化维护 `~/.codex/config.toml` 中固定的 `model_providers.codecli_installer`，并设置 `wire_api = "responses"`。
- 清除/切换：通过 ownership 与 durable transaction 恢复本工具接管前的值；顶层配置文件为符号链接或状态损坏时 fail closed。
- 连通性测试：使用内置 `reqwest` 客户端，禁止重定向；Claude 发送最小 Messages 请求，Codex 发送最小 Responses 请求并校验响应结构。
