# 参与贡献

感谢你帮助改进 CodeCLI。我们欢迎缺陷修复、可复现报告、文档改进、无障碍优化和经过安全评审的新 Provider 支持。

参与本项目即表示你同意遵守 [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md)。

## 开始之前

1. 搜索现有 Issue 和 Pull Request，避免重复工作；
2. 对较大的功能、架构变化或新增依赖，先创建 Feature Request 讨论范围；
3. 安全漏洞不要公开提交，请按 [`SECURITY.md`](SECURITY.md) 私下报告；
4. 不要提交密钥、Token、Cookie、生产地址、用户日志、真实个人数据或签名材料。

## 开发环境

项目使用 Tauri 2、React、TypeScript 和 Rust。推荐使用仓库声明的固定工具链：

- Node.js 22
- pnpm 11
- Rust 1.94

```bash
pnpm install --frozen-lockfile
pnpm tauri dev
```

不同平台还需要安装 [Tauri 2 系统依赖](https://v2.tauri.app/start/prerequisites/)。

## 提交前检查

```bash
pnpm build

cd src-tauri
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
```

若你无法运行某个平台的检查，请在 Pull Request 中明确说明，由 GitHub Actions 提供跨平台结果。不要通过降低 lint、类型、锁文件或安全门禁来让检查“变绿”。

## 变更原则

- 保持社区版无远程控制、无遥测、无运营方激活 DRM；
- 安装、写配置和删除操作必须可解释、可回滚，并限制在可证明 ownership 的范围；
- 网络请求必须有明确产品目的，不得静默增加统计、追踪或后台上报；
- 新增下载源必须使用 HTTPS、固定可信来源和可复核的完整性校验；
- 不记录完整 API Key、Authorization、Token、密码或用户项目内容；
- 尽量提交小而聚焦的变更，避免无关重构；
- 修改用户可见行为时同步更新 README、隐私说明或 Provider 文档。

## Pull Request 要求

Pull Request 应包含：

- 问题与修复/设计说明；
- 实际验证命令及结果，或未验证原因；
- 风险、回滚方式和涉及的操作系统；
- UI 变化的截图（不得包含敏感信息）；
- 对安全、隐私、网络和文件 ownership 边界的影响说明。

维护者可能要求拆分过大的 PR、补充测试或在合并前调整设计。合并与发布时间由维护者根据风险和维护能力决定。

## 许可证

除明确标注例外的内容外，本项目采用 MPL-2.0。提交贡献即表示：

- 你有权提交该内容；
- 该贡献可按 MPL-2.0 与本项目一同分发；
- 贡献中没有未经许可复制的代码、素材或机密信息。

贡献者保留自己贡献的版权。第三方代码或素材必须在 PR 中说明来源、许可证和必要的归属信息。
