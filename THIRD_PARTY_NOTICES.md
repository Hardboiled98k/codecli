# 第三方软件说明

CodeCLI 使用开源依赖，并会在用户选择相应功能后访问或下载某些独立的第三方工具。第三方组件不因出现在本项目中而改用 MPL-2.0；其版权、许可证、服务条款和商标权仍属于各自权利人。

## 构建依赖

精确版本和传递依赖以当前提交的以下锁文件为准：

- JavaScript/TypeScript：`pnpm-lock.yaml`
- Rust：`src-tauri/Cargo.lock`

发布构建应从锁文件生成并随附机器可读 SBOM、逐项许可证清单及许可证要求的完整文本。仓库中的本说明是边界概览，不替代逐版本生成的材料。

## 用户选择后使用的独立工具与服务

CodeCLI 可能按用户明确操作下载、打开或配置：

- Node.js；
- Anthropic Claude Code；
- OpenAI Codex CLI 及 ChatGPT/Codex 下载页；
- 飞书/Lark CLI；
- 用户选择的 Anthropic、OpenAI 或兼容 Provider。

这些工具和服务不是 CodeCLI 的组成部分，也不由南京孤岛网络科技有限公司控制。用户应查看对应版本随附的许可证、隐私政策和服务条款。

## 非关联声明

Anthropic、Claude、Claude Code、OpenAI、ChatGPT、Codex、飞书/Lark 及其他第三方名称和标识属于各自权利人。名称仅用于说明兼容性，不表示授权、合作、赞助、认可或官方支持。

CodeCLI 社区版不包含 Tailscale、Headscale、SSH 远程接入或其他远程控制组件。
