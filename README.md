# CodeCLI

[![License: MPL-2.0](https://img.shields.io/badge/License-MPL--2.0-blue.svg)](LICENSE)

CodeCLI 是一个面向 macOS 与 Windows 的开源桌面安装、配置和自检工具，帮助用户在自己的电脑上安装与管理 Claude Code、Codex CLI 等命令行开发工具。

本仓库是 **CodeCLI 社区版**。它强调本地优先、操作可见和可恢复：安装或修改配置前展示计划，关键下载固定来源并校验完整性，尽量只修改由 CodeCLI 管理的内容。

> **独立项目声明：** CodeCLI 由南京孤岛网络科技有限公司维护，与 Anthropic、OpenAI、Claude、Claude Code、ChatGPT、Codex 及其他 Provider 的权利人不存在隶属、授权、赞助或背书关系。第三方名称仅用于说明兼容性。

## 社区版边界

### 包含

- 操作系统、架构、磁盘、网络、Node 与已安装 CLI 的本机检查；
- 用户级 Node 运行时及受支持 CLI 的安装、检查与更新；
- Claude/Codex Provider 配置、连通性验证和配置方案切换；
- 受管配置的备份、恢复、诊断日志与卸载；
- 所有核心客户端源码、锁文件和持续集成配置。

具体 Provider 和兼容端点见 [`docs/providers.md`](docs/providers.md)。

### 明确不包含

- **远程控制：** 不包含远程 Shell、SSH 接入、Tailscale/Headscale 会话、操作员接管或远程协助服务；
- **遥测：** 不包含分析 SDK、行为追踪、广告标识、崩溃自动上报或后台使用统计；
- **激活 DRM：** 不连接 CodeCLI 运营方的兑换码、许可证或设备绑定服务；
- 第三方账号、会员、API 额度、共享 Key、破解或鉴权绕过；
- Anthropic、OpenAI 或其他第三方提供的官方技术支持。

CodeCLI 仍会按用户选择访问必要的第三方网络资源，例如下载安装包、打开官方下载页或测试用户指定的 Provider。详见 [`PRIVACY.md`](PRIVACY.md)。

## 安全设计原则

- API Key 仅保存在用户设备的受管配置中；测试 Provider 时会直接发送给用户选择的服务，而不是发送给 CodeCLI 维护者；
- 下载和安装行为尽量使用固定来源、固定版本及完整性校验；
- 备份、恢复和卸载遵循 ownership 记录，不主动删除无法证明由 CodeCLI 管理的内容；
- 不要求用户关闭 Gatekeeper、SmartScreen、杀毒软件或系统安全策略；
- 安全问题请按 [`SECURITY.md`](SECURITY.md) 私下报告，不要在公开 Issue 粘贴密钥或漏洞细节。

## 当前状态

项目处于社区公开初期。源码可供审计和参与开发，但在首个经过 CI 验证、签名并发布的版本出现前，不应把任意第三方构建视为“官方安装包”。

仅从本仓库 GitHub Releases 获取官方发布物，并自行核对发布页提供的校验与签名信息。任何 fork 或第三方分发均由其发布者负责。

## 开发

### 前置条件

- Node.js 22
- pnpm 11
- Rust 1.94
- 对应平台的 [Tauri 2 系统依赖](https://v2.tauri.app/start/prerequisites/)

### 启动

```bash
git clone https://github.com/Hardboiled98k/codecli.git
cd codecli
pnpm install --frozen-lockfile
pnpm tauri dev
```

### 基础检查

```bash
pnpm build

cd src-tauri
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
```

请不要把本地未签名构建冒充官方发行物。提交代码前请阅读 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

## 获取帮助

- 使用问题和可复现缺陷：查看 [`SUPPORT.md`](SUPPORT.md) 后提交 Issue；
- 安全漏洞：按照 [`SECURITY.md`](SECURITY.md) 私下报告；
- 隐私问题：`825242058@qq.com`。

维护者不会要求用户提供完整 API Key、密码、Token，也不会要求通过远程控制接管设备。

## 许可证与品牌

除另有说明的文件和第三方依赖外，源码按 [Mozilla Public License 2.0](LICENSE) 提供。修改并分发 MPL 覆盖的文件时，需要遵守该许可证的源码提供义务。

- 版权与第三方说明：[`NOTICE.md`](NOTICE.md)、[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md)
- 图片与图标许可：[`ASSET_LICENSES.md`](ASSET_LICENSES.md)
- CodeCLI 名称和标识使用规则：[`TRADEMARKS.md`](TRADEMARKS.md)

Copyright © 2026 南京孤岛网络科技有限公司。
