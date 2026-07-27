// SPDX-License-Identifier: MPL-2.0
//! 不经 npm 解析的固定 npm bundle 安装器。
//!
//! 每个输入 tarball 的官方 registry URL、包名、版本与 SHA-512
//! SRI 全部编译期固定。下载、哈希和解包始终复用同一个 File
//! handle，不会在验证后按路径重新打开 tarball。

use base64::Engine as _;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tar::Archive;

use super::cmd::check_cancelled;

const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_ARCHIVE_DEPTH: usize = 32;
const MAX_FILE_BYTES: u64 = 384 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 768 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const RECEIPT_SCHEMA_VERSION: u8 = 1;
const REMOVAL_JOURNAL_SCHEMA_VERSION: u8 = 1;

/// 在任何最终 prefix 副作用发生前持久化的内容收据。
///
/// package 与每个 launcher 分开指纹，使得进程在多条
/// no-replace rename 之间崩溃后，下次也只能隔离并删除与收据
/// 逐个匹配的精确条目，绝不让 npm 解析可变 manifest。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PinnedBundleReceipt {
    pub schema_version: u8,
    pub package_sha256: String,
    pub launcher_sha256: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemovalJournal {
    schema_version: u8,
    package: String,
    command: String,
    receipt: PinnedBundleReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackageSpec {
    name: &'static str,
    version: &'static str,
    url: &'static str,
    sri: &'static str,
}

const CLAUDE_TOP: PackageSpec = PackageSpec {
    name: "@anthropic-ai/claude-code",
    version: "2.1.211",
    url: "https://registry.npmjs.org/@anthropic-ai/claude-code/-/claude-code-2.1.211.tgz",
    sri: "sha512-yGhXSF9YfHoVGe0S6N9ky5uajx79f+vt6ZT3HhBJLFSjJtiGEs67H0h93iTdOvPU/wOffijpTUAn76U/+vQnTQ==",
};

const CODEX_TOP: PackageSpec = PackageSpec {
    name: "@openai/codex",
    version: "0.144.5",
    url: "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5.tgz",
    sri: "sha512-jjB+K+OMv572mKhS+2QuLxWXDJNdpwbPenf+V+8bdq7wg4Scqt3cn6WEekD8wPqDVZqck0HSX17K9rD9kbDJQA==",
};

const FEISHU_TOP: PackageSpec = PackageSpec {
    name: "@larksuite/cli",
    version: "1.0.70",
    url: "https://registry.npmjs.org/@larksuite/cli/-/cli-1.0.70.tgz",
    sri: "sha512-6x5AXaH5eWHYKfzpOgWVoanpYRFq5O1v02OYDToHw8KgcNY9zwZ8KvoM2eQs9B6oO07QDqRUMpM1XjsVGb1dCA==",
};

const FEISHU_CLOSURE: [PackageSpec; 6] = [
    PackageSpec {
        name: "@clack/prompts",
        version: "1.7.0",
        url: "https://registry.npmjs.org/@clack/prompts/-/prompts-1.7.0.tgz",
        sri: "sha512-y7/yvZ2TPAnR9+jnc00klvNNLkJiXFFrQA/hlLCcxA9a2A4zQIOimyFQ9XfwYKiGD1fb5GY8vbKIIgO8d5Tb2A==",
    },
    PackageSpec {
        name: "@clack/core",
        version: "1.4.3",
        url: "https://registry.npmjs.org/@clack/core/-/core-1.4.3.tgz",
        sri: "sha512-/kr3UWNtdJfxZtPgDqUOmG2pvwlmcLGheex5yiZKdwbzZJxhV+HMNR9QNmyY5cGwTNV6LrR7Jtp+KjhUAP1qBQ==",
    },
    PackageSpec {
        name: "fast-string-width",
        version: "3.0.2",
        url: "https://registry.npmjs.org/fast-string-width/-/fast-string-width-3.0.2.tgz",
        sri: "sha512-gX8LrtNEI5hq8DVUfRQMbr5lpaS4nMIWV+7XEbXk2b8kiQIizgnlr12B4dA3ZEx3308ze0O4Q1R+cHts8kyUJg==",
    },
    PackageSpec {
        name: "fast-string-truncated-width",
        version: "3.0.3",
        url: "https://registry.npmjs.org/fast-string-truncated-width/-/fast-string-truncated-width-3.0.3.tgz",
        sri: "sha512-0jjjIEL6+0jag3l2XWWizO64/aZVtpiGE3t0Zgqxv0DPuxiMjvB3M24fCyhZUO4KomJQPj3LTSUnDP3GpdwC0g==",
    },
    PackageSpec {
        name: "fast-wrap-ansi",
        version: "0.2.2",
        url: "https://registry.npmjs.org/fast-wrap-ansi/-/fast-wrap-ansi-0.2.2.tgz",
        sri: "sha512-7F2Fl+TjRSenLqlU3UjSH0iyqopqoZIu7eZVpEirP2g1GtWa2G/ecEmBdgz31+Mxr+ELclgg6sokpSFIQiZ02Q==",
    },
    PackageSpec {
        name: "sisteransi",
        version: "1.0.5",
        url: "https://registry.npmjs.org/sisteransi/-/sisteransi-1.0.5.tgz",
        sri: "sha512-bLGGlR1QxBcynn2d5YmDX4MGjlZvy2MRBDRNHLJ8VI6l6+9FUiyTFNJ0IveOSP0bcXgVDPRcfGqA0pjaqUpfVg==",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedPlatform {
    DarwinArm64,
    DarwinX64,
    Win32Arm64,
    Win32X64,
}

#[derive(Debug, Clone, Copy)]
struct NativeSelection {
    spec: PackageSpec,
    alias: &'static str,
    source_binary: &'static str,
}

fn supported_platform(os: &str, arch: &str) -> Result<SupportedPlatform, String> {
    match (os, arch) {
        ("macos" | "darwin", "aarch64" | "arm64") => Ok(SupportedPlatform::DarwinArm64),
        ("macos" | "darwin", "x86_64" | "x64") => Ok(SupportedPlatform::DarwinX64),
        ("windows" | "win32", "aarch64" | "arm64") => Ok(SupportedPlatform::Win32Arm64),
        ("windows" | "win32", "x86_64" | "x64") => Ok(SupportedPlatform::Win32X64),
        ("linux", _) => Err("固定 npm bundle 不支持 Linux".into()),
        _ => Err(format!("不支持的平台: {os}/{arch}")),
    }
}

fn current_platform() -> Result<SupportedPlatform, String> {
    supported_platform(std::env::consts::OS, std::env::consts::ARCH)
}

fn claude_native(platform: SupportedPlatform) -> NativeSelection {
    match platform {
        SupportedPlatform::DarwinArm64 => NativeSelection {
            spec: PackageSpec { name: "@anthropic-ai/claude-code-darwin-arm64", version: "2.1.211", url: "https://registry.npmjs.org/@anthropic-ai/claude-code-darwin-arm64/-/claude-code-darwin-arm64-2.1.211.tgz", sri: "sha512-ogsLXqbHlHSFE9ApgpoeoP6wXJKkcUyYM4f8rrAbTvQStvqQ/bpHLV5mgbuEGn/N9NPWBQt826bfH/XvlYi0kg==" },
            alias: "@anthropic-ai/claude-code-darwin-arm64", source_binary: "claude",
        },
        SupportedPlatform::DarwinX64 => NativeSelection {
            spec: PackageSpec { name: "@anthropic-ai/claude-code-darwin-x64", version: "2.1.211", url: "https://registry.npmjs.org/@anthropic-ai/claude-code-darwin-x64/-/claude-code-darwin-x64-2.1.211.tgz", sri: "sha512-t3AgChHNAe6Djp//73U6SoeRbmc2A/ia6FHU3gJuMyvCeQ8I9c5PvXOhs2p37H0+bYiJMoXxI6MdmY9sPEFa8g==" },
            alias: "@anthropic-ai/claude-code-darwin-x64", source_binary: "claude",
        },
        SupportedPlatform::Win32Arm64 => NativeSelection {
            spec: PackageSpec { name: "@anthropic-ai/claude-code-win32-arm64", version: "2.1.211", url: "https://registry.npmjs.org/@anthropic-ai/claude-code-win32-arm64/-/claude-code-win32-arm64-2.1.211.tgz", sri: "sha512-W04nNnYZl54o5Dmr69nSCz9aEG3TIw4Vr2nmeNQcqJjIzHTy19xmXKbioY25yCHQgrHe/AHMVMzWneAp8yylPw==" },
            alias: "@anthropic-ai/claude-code-win32-arm64", source_binary: "claude.exe",
        },
        SupportedPlatform::Win32X64 => NativeSelection {
            spec: PackageSpec { name: "@anthropic-ai/claude-code-win32-x64", version: "2.1.211", url: "https://registry.npmjs.org/@anthropic-ai/claude-code-win32-x64/-/claude-code-win32-x64-2.1.211.tgz", sri: "sha512-/pXHWP02ni+xM37QP0Yrn0rG3K2MKq47nxB5xuUrMirpQG1zA5orFtCiP4hmQaiYICRgW39ZmQdEQpsvt2t+pg==" },
            alias: "@anthropic-ai/claude-code-win32-x64", source_binary: "claude.exe",
        },
    }
}

fn codex_native(platform: SupportedPlatform) -> NativeSelection {
    match platform {
        SupportedPlatform::DarwinArm64 => NativeSelection {
            spec: PackageSpec { name: "@openai/codex", version: "0.144.5-darwin-arm64", url: "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5-darwin-arm64.tgz", sri: "sha512-zcT6NfBCqLFt+BReNSETTZW6v6PdbH0dzNtm9j7l7mDGqwPbKZDGJdnpkBao2389I0ZacyIKgSZoI0vez1d4Dw==" },
            alias: "@openai/codex-darwin-arm64", source_binary: "vendor/aarch64-apple-darwin/bin/codex",
        },
        SupportedPlatform::DarwinX64 => NativeSelection {
            spec: PackageSpec { name: "@openai/codex", version: "0.144.5-darwin-x64", url: "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5-darwin-x64.tgz", sri: "sha512-//Mo0m1MwaoT6psu5xsmofXpKx4/0irIkeq10xJvk59+886EG355ibjA+ZmlRcKhE3bLjsKD7p81nTbAdRL/bw==" },
            alias: "@openai/codex-darwin-x64", source_binary: "vendor/x86_64-apple-darwin/bin/codex",
        },
        SupportedPlatform::Win32Arm64 => NativeSelection {
            spec: PackageSpec { name: "@openai/codex", version: "0.144.5-win32-arm64", url: "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5-win32-arm64.tgz", sri: "sha512-0Pj7iqjEOEvPQPO3kFfCy9vGX4BTu76ChFFZHr2eNNIfVc3FOENAv/X98u4L+iIUtDOK9DbqmfUudW3DPapshg==" },
            alias: "@openai/codex-win32-arm64", source_binary: "vendor/aarch64-pc-windows-msvc/bin/codex.exe",
        },
        SupportedPlatform::Win32X64 => NativeSelection {
            spec: PackageSpec { name: "@openai/codex", version: "0.144.5-win32-x64", url: "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5-win32-x64.tgz", sri: "sha512-DnsSTlnnzleTxvLwIGnBitKInscxn2I7qASqosS8Fv+qysBygd+ZiBn/SQsRCgQ28PAlsNzmd3Gf3ZTecolAmg==" },
            alias: "@openai/codex-win32-x64", source_binary: "vendor/x86_64-pc-windows-msvc/bin/codex.exe",
        },
    }
}

fn verify_sri_and_rewind(file: &mut File, expected_sri: &str) -> Result<(), String> {
    let encoded = expected_sri
        .strip_prefix("sha512-")
        .ok_or("SRI 不是 sha512")?;
    let expected = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("SRI base64 无效: {error}"))?;
    if expected.len() != 64 {
        return Err("SRI SHA-512 长度无效".into());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("重置 tarball 句柄失败: {error}"))?;
    let mut hasher = Sha512::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_cancelled()?;
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("读取 tarball 哈希失败: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hasher.finalize();
    let mismatch = actual
        .iter()
        .zip(&expected)
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b));
    if mismatch != 0 {
        return Err("npm tarball SHA-512 SRI 不匹配".into());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("哈希后重置 tarball 句柄失败: {error}"))?;
    Ok(())
}

fn safe_archive_relative(raw: &[u8], is_dir: bool) -> Result<(PathBuf, String), String> {
    let text = std::str::from_utf8(raw).map_err(|_| "tar 路径不是 UTF-8")?;
    if text.starts_with('/') || text.starts_with('\\') || text.contains('\\') || text.contains('\0')
    {
        return Err("tar 路径为绝对路径或含 Windows 分隔符".into());
    }
    let trimmed = if is_dir {
        text.trim_end_matches('/')
    } else {
        text
    };
    if trimmed != text && !is_dir {
        return Err("tar 普通文件路径异常".into());
    }
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.first().copied() != Some("package") || parts.iter().any(|part| part.is_empty()) {
        return Err("tar 条目不在 package/... 下".into());
    }
    let relative = &parts[1..];
    if relative.len() > MAX_ARCHIVE_DEPTH {
        return Err("tar 路径深度超限".into());
    }
    let mut result = PathBuf::new();
    for part in relative {
        if matches!(*part, "." | "..")
            || part.ends_with('.')
            || part.ends_with(' ')
            || part.contains(':')
            || part.chars().any(char::is_control)
        {
            return Err("tar 路径包含越界或平台危险分量".into());
        }
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        ) {
            return Err("tar 路径包含 Windows 保留名".into());
        }
        result.push(part);
    }
    let key = relative.join("/").to_ascii_lowercase();
    Ok((result, key))
}

fn unpack_verified_archive(file: &mut File, destination: &Path) -> Result<(), String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("解包前重置 tarball 句柄失败: {error}"))?;
    std::fs::create_dir(destination)
        .map_err(|error| format!("创建 fresh package staging 失败: {error}"))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("读取 tar 失败: {error}"))?;
    let mut seen = HashSet::new();
    let mut entry_count = 0_usize;
    let mut total = 0_u64;
    for item in entries {
        check_cancelled()?;
        entry_count += 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            return Err("tar 条目数超限".into());
        }
        let mut entry = item.map_err(|error| format!("读取 tar 条目失败: {error}"))?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err("tar 包含 symlink/hardlink/device/fifo 或其他非普通条目".into());
        }
        let (relative, key) = safe_archive_relative(&entry.path_bytes(), kind.is_dir())?;
        if !seen.insert(key) {
            return Err("tar 包含重复路径".into());
        }
        if relative.as_os_str().is_empty() {
            if !kind.is_dir() {
                return Err("package 根条目不是目录".into());
            }
            continue;
        }
        let output = destination.join(&relative);
        if kind.is_dir() {
            // Entry::size() 会采用 PAX size override；raw header.size()
            // 不能作为解压边界，否则 PAX 可绕过大小限制。
            if entry.size() != 0 {
                return Err("tar 目录带有数据".into());
            }
            std::fs::create_dir_all(&output)
                .map_err(|error| format!("创建 staging 目录失败: {error}"))?;
            continue;
        }
        // 必须用 Entry::size()，它包含 PAX size override，且也是
        // Entry::read 实际允许读取的长度。
        let size = entry.size();
        if size > MAX_FILE_BYTES {
            return Err("tar 单文件大小超限".into());
        }
        total = total.checked_add(size).ok_or("tar 总大小溢出")?;
        if total > MAX_TOTAL_BYTES {
            return Err("tar 解包总大小超限".into());
        }
        let parent = output.parent().ok_or("tar 目标没有父目录")?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 staging 父目录失败: {error}"))?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output_file = options
            .open(&output)
            .map_err(|error| format!("创建 staging 文件失败: {error}"))?;
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            check_cancelled()?;
            let read = entry
                .read(&mut buffer)
                .map_err(|error| format!("解包普通文件失败: {error}"))?;
            if read == 0 {
                break;
            }
            output_file
                .write_all(&buffer[..read])
                .map_err(|error| format!("写入解包文件失败: {error}"))?;
            copied = copied
                .checked_add(read as u64)
                .ok_or("tar 条目解包长度溢出")?;
            if copied > size {
                return Err("tar 条目实际长度超过 header".into());
            }
        }
        if copied != size {
            return Err("tar 条目实际长度与 header 不符".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let archived_mode = entry.header().mode().unwrap_or(0o644);
            let mode = if archived_mode & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            std::fs::set_permissions(&output, std::fs::Permissions::from_mode(mode))
                .map_err(|error| format!("设置 staging 文件权限失败: {error}"))?;
        }
        // chmod 也是发布内容的一部分，必须在 fsync 前完成。
        output_file
            .sync_all()
            .map_err(|error| format!("持久化 staging 文件失败: {error}"))?;
    }
    Ok(())
}

fn validate_manifest(destination: &Path, spec: PackageSpec) -> Result<(), String> {
    let path = destination.join("package.json");
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("检查 package.json 失败: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_MANIFEST_BYTES
    {
        return Err("package.json 不是可信普通文件或大小异常".into());
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("读取 package.json 失败: {error}"))?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("package.json JSON 无效: {error}"))?;
    if manifest.get("name").and_then(|value| value.as_str()) != Some(spec.name)
        || manifest.get("version").and_then(|value| value.as_str()) != Some(spec.version)
    {
        return Err(format!(
            "package.json name/version 不匹配，期望 {}@{}",
            spec.name, spec.version
        ));
    }
    Ok(())
}

fn pinned_http_client() -> Result<Client, String> {
    Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(20))
        // Codex 平台包可超过 140 MiB，不能用过短的 total timeout
        // 惩罚中国慢链路。活跃传输在每个 64 KiB chunk 观察取消；
        // blocking reqwest 完全无进展的单次 read 仍受这个总上限约束。
        .timeout(Duration::from_secs(600))
        .user_agent("CodeCLI-Installer/pinned-npm-v1")
        .build()
        .map_err(|error| format!("创建固定 npm HTTPS 客户端失败: {error}"))
}

fn validate_official_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("固定 npm URL 无效: {error}"))?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("registry.npmjs.org")
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("固定 npm URL 不是无凭据的 registry.npmjs.org HTTPS URL".into());
    }
    Ok(())
}

fn download_into_handle(client: &Client, spec: PackageSpec, file: &mut File) -> Result<(), String> {
    check_cancelled()?;
    validate_official_url(spec.url)?;
    let mut response = client
        .get(spec.url)
        .send()
        .map_err(|error| format!("下载 {}@{} 失败: {error}", spec.name, spec.version))?;
    check_cancelled()?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!(
            "下载 {}@{} 返回非 200 状态 {}（禁止 redirect）",
            spec.name,
            spec.version,
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length == 0 || length > MAX_DOWNLOAD_BYTES)
    {
        return Err(format!(
            "{}@{} tarball Content-Length 超限",
            spec.name, spec.version
        ));
    }
    file.set_len(0)
        .map_err(|error| format!("清空临时 tarball 失败: {error}"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("重置临时 tarball 失败: {error}"))?;
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_cancelled()?;
        let read = response
            .read(&mut buffer)
            .map_err(|error| format!("接收 {}@{} 失败: {error}", spec.name, spec.version))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or("tarball 下载长度溢出")?;
        if total > MAX_DOWNLOAD_BYTES {
            return Err(format!("{}@{} tarball 下载超限", spec.name, spec.version));
        }
        file.write_all(&buffer[..read])
            .map_err(|error| format!("写入临时 tarball 失败: {error}"))?;
    }
    if total == 0 {
        return Err(format!("{}@{} tarball 为空", spec.name, spec.version));
    }
    file.sync_all()
        .map_err(|error| format!("持久化临时 tarball 失败: {error}"))?;
    verify_sri_and_rewind(file, spec.sri)
}

fn ensure_real_directory_chain(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("目录不是绝对路径: {}", path.display()));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                // `\\?\C:` 这类裸前缀不是目录路径：对它做 metadata 会打开
                // 卷句柄并报 ERROR_INVALID_FUNCTION。前缀本身无需校验，
                // 紧随其后的根目录组件会以 `\\?\C:\` 形式接受同样检查。
                current.push(component.as_os_str());
                continue;
            }
            Component::RootDir | Component::Normal(_) => current.push(component.as_os_str()),
            Component::CurDir | Component::ParentDir => return Err("目录路径包含 ./..".into()),
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(format!("目录链包含链接或非目录: {}", current.display()));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)
                    .map_err(|error| format!("创建目录失败 {}: {error}", current.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o700))
                        .map_err(|error| format!("设置目录权限失败: {error}"))?;
                }
                sync_directory(&current)?;
                if let Some(parent) = current.parent() {
                    sync_directory(parent)?;
                }
            }
            Err(error) => return Err(format!("检查目录链失败: {error}")),
        }
    }
    Ok(())
}

static STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct BundleStage {
    root: PathBuf,
    prefix: PathBuf,
    downloads: PathBuf,
}

impl BundleStage {
    fn create(final_prefix: &Path) -> Result<Self, String> {
        if !final_prefix.is_absolute() {
            return Err("固定 npm prefix 必须是绝对路径".into());
        }
        let parent = final_prefix.parent().ok_or("固定 npm prefix 没有父目录")?;
        ensure_real_directory_chain(parent)?;
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut root = None;
        for _ in 0..128 {
            let sequence = STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".codecli-pinned-stage-{}-{epoch}-{sequence}",
                std::process::id()
            ));
            #[cfg(unix)]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&candidate) {
                Ok(()) => {
                    root = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("创建私有 npm staging 失败: {error}")),
            }
        }
        let root = root.ok_or("无法分配唯一 npm staging 目录")?;
        sync_directory(&root)?;
        sync_directory(parent)?;
        let prefix = root.join("prefix");
        let downloads = root.join("downloads");
        std::fs::create_dir(&prefix)
            .map_err(|error| format!("创建 staging prefix 失败: {error}"))?;
        std::fs::create_dir(&downloads)
            .map_err(|error| format!("创建私有下载目录失败: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("设置 staging prefix 权限失败: {error}"))?;
            std::fs::set_permissions(&downloads, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("设置下载目录权限失败: {error}"))?;
        }
        sync_directory(&prefix)?;
        sync_directory(&downloads)?;
        sync_directory(&root)?;
        Ok(Self {
            root,
            prefix,
            downloads,
        })
    }

    fn archive_file(&self, index: usize) -> Result<File, String> {
        let path = self.downloads.join(format!("archive-{index}.tgz"));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .map_err(|error| format!("创建私有临时 tarball 失败: {error}"))
    }
}

impl Drop for BundleStage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fetch_package(
    client: &Client,
    stage: &BundleStage,
    index: usize,
    spec: PackageSpec,
    destination: &Path,
) -> Result<(), String> {
    check_cancelled()?;
    let parent = destination.parent().ok_or("package staging 没有父目录")?;
    ensure_real_directory_chain(parent)?;
    let mut archive = stage.archive_file(index)?;
    download_into_handle(client, spec, &mut archive)?;
    check_cancelled()?;
    // `archive` 还是上面 response 直接写入的同一句柄。
    unpack_verified_archive(&mut archive, destination)?;
    check_cancelled()?;
    // 文件已逐个 fsync；再自底向上同步每个目录的目录项，以及
    // destination 在父目录中的存在，之后才允许发布。
    sync_directory_tree(destination, 0)?;
    sync_directory(parent)?;
    validate_manifest(destination, spec)
}

fn npm_modules_relative() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("node_modules")
    } else {
        PathBuf::from("lib").join("node_modules")
    }
}

fn package_name_relative(package: &str) -> Result<PathBuf, String> {
    let parts: Vec<&str> = package.split('/').collect();
    let atom = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
    };
    let valid = match parts.as_slice() {
        [plain] => atom(plain) && !plain.starts_with('@'),
        [scope, name] => scope.starts_with('@') && atom(&scope[1..]) && atom(name),
        _ => false,
    };
    if !valid {
        return Err(format!("非法 npm 包名: {package}"));
    }
    Ok(parts.iter().collect())
}

fn package_dir(prefix: &Path, package: &str) -> Result<PathBuf, String> {
    Ok(prefix
        .join(npm_modules_relative())
        .join(package_name_relative(package)?))
}

fn nested_package_dir(top: &Path, package: &str) -> Result<PathBuf, String> {
    Ok(top
        .join("node_modules")
        .join(package_name_relative(package)?))
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn path_relative_key(prefix: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(prefix)
        .map_err(|_| "bundle 收据路径不在 prefix 内")?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("bundle 收据路径包含非法分量".into());
        };
        let text = part.to_str().ok_or("bundle 收据路径不是 UTF-8")?;
        if text.is_empty() || text == "." || text == ".." {
            return Err("bundle 收据路径分量无效".into());
        }
        parts.push(text);
    }
    if parts.is_empty() {
        return Err("bundle 收据路径为空".into());
    }
    Ok(parts.join("/"))
}

fn expected_top_bin(package: &str, command: &str) -> Result<&'static str, String> {
    match (package, command) {
        ("@anthropic-ai/claude-code", "claude") => Ok("bin/claude.exe"),
        ("@openai/codex", "codex") => Ok("bin/codex.js"),
        ("@larksuite/cli", "lark-cli") => Ok("scripts/run.js"),
        _ => Err("固定 bundle 包与 command 不在已审计白名单".into()),
    }
}

fn expected_spec(package: &str, command: &str) -> Result<PackageSpec, String> {
    match (package, command) {
        ("@anthropic-ai/claude-code", "claude") => Ok(CLAUDE_TOP),
        ("@openai/codex", "codex") => Ok(CODEX_TOP),
        ("@larksuite/cli", "lark-cli") => Ok(FEISHU_TOP),
        _ => Err("固定 bundle 包与 command 不在已审计白名单".into()),
    }
}

fn validate_exact_top_manifest(
    package_path: &Path,
    package: &str,
    command: &str,
) -> Result<(), String> {
    let spec = expected_spec(package, command)?;
    validate_manifest(package_path, spec)?;
    let bytes = std::fs::read(package_path.join("package.json"))
        .map_err(|error| format!("读取顶层 package.json 失败: {error}"))?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("顶层 package.json JSON 无效: {error}"))?;
    let bin = manifest
        .get("bin")
        .and_then(serde_json::Value::as_object)
        .ok_or("顶层 package.json bin 不是精确对象")?;
    if bin.len() != 1
        || bin.get(command).and_then(serde_json::Value::as_str)
            != Some(expected_top_bin(package, command)?)
    {
        return Err(format!(
            "顶层 package.json bin 必须且只能声明已审计的 {command} 入口"
        ));
    }
    Ok(())
}

fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_mode(hasher: &mut Sha256, metadata: &std::fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        hasher.update(metadata.permissions().mode().to_le_bytes());
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        hasher.update(0_u32.to_le_bytes());
    }
}

struct TreeHashState {
    entries: usize,
    total: u64,
}

fn hash_tree_directory(
    root: &Path,
    directory: &Path,
    depth: usize,
    state: &mut TreeHashState,
    hasher: &mut Sha256,
) -> Result<(), String> {
    if depth > MAX_ARCHIVE_DEPTH + 2 {
        return Err("bundle 指纹目录深度超限".into());
    }
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| format!("检查 bundle 目录失败 {}: {error}", directory.display()))?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(format!(
            "bundle 指纹遇到链接/reparse/非目录: {}",
            directory.display()
        ));
    }
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| "bundle 指纹目录越界")?;
    hasher.update([b'D']);
    hash_field(
        hasher,
        relative.to_string_lossy().replace('\\', "/").as_bytes(),
    );
    hash_mode(hasher, &metadata);

    let mut children = std::fs::read_dir(directory)
        .map_err(|error| format!("读取 bundle 目录失败 {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取 bundle 目录项失败: {error}"))?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        state.entries += 1;
        if state.entries > MAX_ARCHIVE_ENTRIES {
            return Err("bundle 指纹条目数超限".into());
        }
        let path = child.path();
        let name = child
            .file_name()
            .to_str()
            .ok_or("bundle 指纹遇到非 UTF-8 路径")?
            .to_string();
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            return Err("bundle 指纹遇到非法路径分量".into());
        }
        let child_metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("检查 bundle 条目失败 {}: {error}", path.display()))?;
        if metadata_is_link_or_reparse(&child_metadata) {
            return Err(format!(
                "bundle package 不得包含链接/reparse: {}",
                path.display()
            ));
        }
        if child_metadata.is_dir() {
            hash_tree_directory(root, &path, depth + 1, state, hasher)?;
            continue;
        }
        if !child_metadata.is_file() || child_metadata.len() > MAX_FILE_BYTES {
            return Err(format!(
                "bundle package 包含特殊文件或过大文件: {}",
                path.display()
            ));
        }
        state.total = state
            .total
            .checked_add(child_metadata.len())
            .ok_or("bundle 指纹总大小溢出")?;
        if state.total > MAX_TOTAL_BYTES {
            return Err("bundle 指纹总大小超限".into());
        }
        let relative = path.strip_prefix(root).map_err(|_| "bundle 指纹文件越界")?;
        hasher.update([b'F']);
        hash_field(
            hasher,
            relative.to_string_lossy().replace('\\', "/").as_bytes(),
        );
        hasher.update(child_metadata.len().to_le_bytes());
        hash_mode(hasher, &child_metadata);

        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| format!("安全打开 bundle 文件失败 {}: {error}", path.display()))?;
        let opened = file
            .metadata()
            .map_err(|error| format!("复查 bundle 文件失败: {error}"))?;
        if metadata_is_link_or_reparse(&opened)
            || !opened.is_file()
            || opened.len() != child_metadata.len()
        {
            return Err("bundle 文件在指纹期间被替换".into());
        }
        let mut read_total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| format!("读取 bundle 文件失败: {error}"))?;
            if read == 0 {
                break;
            }
            read_total += read as u64;
            hasher.update(&buffer[..read]);
        }
        let after = file
            .metadata()
            .map_err(|error| format!("指纹后复查 bundle 文件失败: {error}"))?;
        if read_total != child_metadata.len() || after.len() != child_metadata.len() {
            return Err("bundle 文件在指纹期间改变".into());
        }
    }
    Ok(())
}

fn fingerprint_package_tree(path: &Path) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(b"CodeCLI pinned package tree v1\0");
    let mut state = TreeHashState {
        entries: 0,
        total: 0,
    };
    hash_tree_directory(path, path, 0, &mut state, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn fingerprint_launcher(path: &Path) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("检查 bundle launcher 失败 {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(b"CodeCLI pinned launcher v1\0");
    #[cfg(unix)]
    {
        if !metadata.file_type().is_symlink() {
            return Err(format!(
                "Unix bundle launcher 不是符号链接: {}",
                path.display()
            ));
        }
        let target = std::fs::read_link(path)
            .map_err(|error| format!("读取 bundle launcher 目标失败: {error}"))?;
        let bytes = target
            .to_str()
            .ok_or("bundle launcher 目标不是 UTF-8")?
            .as_bytes();
        hasher.update([b'L']);
        hash_field(&mut hasher, bytes);
    }
    #[cfg(windows)]
    {
        if metadata_is_link_or_reparse(&metadata)
            || !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() > 64 * 1024
        {
            return Err(format!(
                "Windows bundle launcher 类型/大小异常: {}",
                path.display()
            ));
        }
        let bytes = std::fs::read(path)
            .map_err(|error| format!("读取 Windows bundle launcher 失败: {error}"))?;
        hasher.update([b'F']);
        hash_field(&mut hasher, &bytes);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        return Err("当前平台无已审计 launcher 指纹实现".into());
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) fn validate_receipt_shape(
    prefix: &Path,
    package: &str,
    command: &str,
    receipt: &PinnedBundleReceipt,
) -> Result<(), String> {
    expected_top_bin(package, command)?;
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION
        || !is_lower_hex_sha256(&receipt.package_sha256)
    {
        return Err("bundle 收据 schema 或 package SHA-256 无效".into());
    }
    let expected_paths = super::runtime::cli_launcher_paths(prefix, command)?;
    let mut expected_keys = Vec::with_capacity(expected_paths.len());
    for path in expected_paths {
        expected_keys.push(path_relative_key(prefix, &path)?);
    }
    let actual_keys = receipt.launcher_sha256.keys().cloned().collect::<Vec<_>>();
    if actual_keys != expected_keys {
        return Err("bundle 收据 launcher 路径集与当前平台不精确匹配".into());
    }
    if receipt
        .launcher_sha256
        .values()
        .any(|value| !is_lower_hex_sha256(value))
    {
        return Err("bundle 收据 launcher SHA-256 无效".into());
    }
    Ok(())
}

fn build_receipt(
    prefix: &Path,
    package: &str,
    command: &str,
    launchers: &[PathBuf],
) -> Result<PinnedBundleReceipt, String> {
    let package_path = package_dir(prefix, package)?;
    validate_exact_top_manifest(&package_path, package, command)?;
    let script = package_path.join(expected_top_bin(package, command)?);
    super::runtime::validate_cli_launchers(prefix, command, &script)?;
    let mut launcher_sha256 = BTreeMap::new();
    for launcher in launchers {
        launcher_sha256.insert(
            path_relative_key(prefix, launcher)?,
            fingerprint_launcher(launcher)?,
        );
    }
    let receipt = PinnedBundleReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        package_sha256: fingerprint_package_tree(&package_path)?,
        launcher_sha256,
    };
    validate_receipt_shape(prefix, package, command, &receipt)?;
    Ok(receipt)
}

fn trusted_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("检查 {label} 失败: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_FILE_BYTES
    {
        return Err(format!("{label} 不是大小合法的普通文件"));
    }
    Ok(())
}

fn replace_with_verified_native(source: &Path, target: &Path) -> Result<(), String> {
    check_cancelled()?;
    trusted_regular_file(source, "Claude native 源文件")?;
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("Claude top bin/claude.exe 不是普通 stub".into())
        }
        Ok(_) => std::fs::remove_file(target)
            .map_err(|error| format!("删除 Claude top stub 失败: {error}"))?,
        Err(error) => return Err(format!("Claude top 缺少 bin/claude.exe stub: {error}")),
    }
    let mut input =
        File::open(source).map_err(|error| format!("打开 Claude native 失败: {error}"))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }
    let mut output = options
        .open(target)
        .map_err(|error| format!("创建 Claude top native bin 失败: {error}"))?;
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_cancelled()?;
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("读取 Claude native 失败: {error}"))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| format!("复制 Claude native 失败: {error}"))?;
        copied = copied
            .checked_add(read as u64)
            .ok_or("Claude native 复制长度溢出")?;
        if copied > MAX_FILE_BYTES {
            return Err("Claude native 复制长度异常".into());
        }
    }
    if copied == 0 || copied > MAX_FILE_BYTES {
        return Err("Claude native 复制长度异常".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("设置 Claude native 执行权限失败: {error}"))?;
    }
    output
        .sync_all()
        .map_err(|error| format!("持久化 Claude native 失败: {error}"))?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        // Windows 的 read-only directory handle 上 FlushFileBuffers 会
        // ERROR_ACCESS_DENIED。真正的发布 rename 在下方通过
        // MoveFileExW(MOVEFILE_WRITE_THROUGH) 持久化；不用一个
        // 必然失败的目录 flush 阻断所有 Windows 安装。
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        File::open(path)
            .map_err(|error| format!("打开目录以持久化失败 {}: {error}", path.display()))?
            .sync_all()
            .map_err(|error| format!("持久化目录失败 {}: {error}", path.display()))
    }
}

fn sync_directory_tree(path: &Path, depth: usize) -> Result<(), String> {
    check_cancelled()?;
    if depth > MAX_ARCHIVE_DEPTH + 1 {
        return Err("staging 目录同步深度超限".into());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("检查 staging 目录失败 {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("staging 同步目标不是真实目录: {}", path.display()));
    }
    let mut children = std::fs::read_dir(path)
        .map_err(|error| format!("读取 staging 目录失败 {}: {error}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取 staging 条目失败: {error}"))?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        check_cancelled()?;
        let child = entry.path();
        let child_metadata = std::fs::symlink_metadata(&child)
            .map_err(|error| format!("检查 staging 条目失败 {}: {error}", child.display()))?;
        if child_metadata.is_dir() && !child_metadata.file_type().is_symlink() {
            sync_directory_tree(&child, depth + 1)?;
        } else if child_metadata.file_type().is_symlink() || !child_metadata.is_file() {
            return Err(format!("staging 出现非普通条目: {}", child.display()));
        }
    }
    sync_directory(path)
}

fn rename_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source =
            CString::new(source.as_os_str().as_bytes()).map_err(|_| "staging 源路径包含 NUL")?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| "bundle 目标路径包含 NUL")?;
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            return Ok(());
        }
        Err(format!(
            "renamex_np(RENAME_EXCL) 失败: {}",
            std::io::Error::last_os_error()
        ))
    }

    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source =
            CString::new(source.as_os_str().as_bytes()).map_err(|_| "staging 源路径包含 NUL")?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| "bundle 目标路径包含 NUL")?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        Err(format!(
            "renameat2(RENAME_NOREPLACE) 失败: {}",
            std::io::Error::last_os_error()
        ))
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // 不包含 MOVEFILE_REPLACE_EXISTING，因而原子 fail-if-exists；
        // WRITE_THROUGH 使 move 在 API 返回前落盘。
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        };
        if result != 0 {
            return Ok(());
        }
        Err(format!(
            "MoveFileExW(no-replace) 失败: {}",
            std::io::Error::last_os_error()
        ))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (source, destination);
        Err("当前平台无 no-replace 原子 rename".into())
    }
}

fn remove_path_no_follow(path: &Path) -> Result<(), String> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            std::fs::remove_file(path)
                .map_err(|error| format!("删除文件失败 {}: {error}", path.display()))
        }
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)
            .map_err(|error| format!("删除目录失败 {}: {error}", path.display())),
        Ok(_) => Err(format!("拒绝删除特殊文件: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("检查待删除路径失败: {error}")),
    };
    result?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn path_present(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "检查精确 bundle 条目失败 {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn verify_pinned_bundle_receipt(
    prefix: &Path,
    package: &str,
    command: &str,
    receipt: &PinnedBundleReceipt,
) -> Result<(), String> {
    validate_receipt_shape(prefix, package, command, receipt)?;
    let package_path = package_dir(prefix, package)?;
    validate_exact_top_manifest(&package_path, package, command)?;
    let actual_package = fingerprint_package_tree(&package_path)?;
    if actual_package != receipt.package_sha256 {
        return Err("已发布 bundle package 与发布前收据不匹配".into());
    }
    let launcher_paths = super::runtime::cli_launcher_paths(prefix, command)?;
    for launcher in &launcher_paths {
        let key = path_relative_key(prefix, launcher)?;
        let expected = receipt
            .launcher_sha256
            .get(&key)
            .ok_or("bundle 收据缺少 launcher")?;
        if fingerprint_launcher(launcher)? != *expected {
            return Err(format!("bundle launcher 与发布前收据不匹配: {key}"));
        }
    }
    let script = package_path.join(expected_top_bin(package, command)?);
    super::runtime::validate_cli_launchers(prefix, command, &script)
}

fn write_removal_journal(path: &Path, journal: &RemovalJournal) -> Result<(), String> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| format!("序列化 bundle 隔离日志失败: {error}"))?;
    if bytes.len() > 64 * 1024 {
        return Err("bundle 隔离日志超限".into());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("创建 bundle 隔离日志失败: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("写入 bundle 隔离日志失败: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("持久化 bundle 隔离日志失败: {error}"))
}

fn read_removal_journal(path: &Path) -> Result<Option<RemovalJournal>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("检查 bundle 隔离日志失败: {error}")),
    };
    if metadata_is_link_or_reparse(&metadata)
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > 64 * 1024
    {
        return Err("bundle 隔离日志不是可信小文件".into());
    }
    let bytes =
        std::fs::read(path).map_err(|error| format!("读取 bundle 隔离日志失败: {error}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("bundle 隔离日志损坏: {error}"))
}

fn quarantine_allowed_names(launcher_count: usize) -> HashSet<String> {
    let mut result = HashSet::from([
        "receipt.json".to_string(),
        "package-delete-authorized.json".to_string(),
        "package".to_string(),
    ]);
    for index in 0..launcher_count {
        result.insert(format!("launcher-{index}"));
    }
    result
}

fn create_quarantine_with_journal(
    prefix: &Path,
    quarantine: &Path,
    journal: &RemovalJournal,
) -> Result<(), String> {
    ensure_real_directory_chain(prefix)?;
    let parent = prefix.parent().ok_or("bundle prefix 没有父目录")?;
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stage = parent.join(format!(
        ".codecli-remove-journal-stage-{}-{epoch}-{}",
        std::process::id(),
        STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    #[cfg(unix)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&stage)
        .map_err(|error| format!("创建私有 bundle 隔离日志 staging 失败: {error}"))?;
    let result = (|| {
        write_removal_journal(&stage.join("receipt.json"), journal)?;
        sync_directory(&stage)?;
        rename_no_replace(&stage, quarantine)?;
        sync_directory(prefix)?;
        sync_directory(parent)
    })();
    if result.is_err() && path_present(&stage).unwrap_or(false) {
        let _ = remove_path_no_follow(&stage);
    }
    result
}

fn publish_package_delete_authorization(
    prefix: &Path,
    quarantine: &Path,
    journal: &RemovalJournal,
) -> Result<(), String> {
    let destination = quarantine.join("package-delete-authorized.json");
    if path_present(&destination)? {
        return match read_removal_journal(&destination)? {
            Some(actual) if actual == *journal => Ok(()),
            _ => Err("bundle package 删除授权与 durable ownership 收据不匹配".into()),
        };
    }
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let parent = prefix.parent().ok_or("bundle prefix 没有父目录")?;
    let temp = parent.join(format!(
        ".codecli-package-delete-auth-{}-{epoch}-{}",
        std::process::id(),
        STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        write_removal_journal(&temp, journal)?;
        rename_no_replace(&temp, &destination)?;
        sync_directory(quarantine)?;
        sync_directory(parent)
    })();
    if result.is_err() && path_present(&temp).unwrap_or(false) {
        let _ = remove_path_no_follow(&temp);
    }
    result
}

fn validate_quarantine_entries(path: &Path, launcher_count: usize) -> Result<(), String> {
    let allowed = quarantine_allowed_names(launcher_count);
    for entry in
        std::fs::read_dir(path).map_err(|error| format!("读取 bundle 隔离目录失败: {error}"))?
    {
        let entry = entry.map_err(|error| format!("读取 bundle 隔离条目失败: {error}"))?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or("bundle 隔离条目不是 UTF-8")?
            .to_string();
        if !allowed.contains(&name) {
            return Err(format!("bundle 隔离目录包含未授权条目: {name}"));
        }
    }
    Ok(())
}

fn directory_is_empty(path: &Path) -> Result<bool, String> {
    Ok(std::fs::read_dir(path)
        .map_err(|error| format!("读取目录失败 {}: {error}", path.display()))?
        .next()
        .is_none())
}

fn quarantine_rejection_with_restore(
    source: &Path,
    quarantined: &Path,
    label: &str,
    reason: String,
) -> String {
    let restore = match path_present(source) {
        Ok(false) => rename_no_replace(quarantined, source).and_then(|()| {
            sync_directory(source.parent().ok_or("恢复路径没有父目录")?)?;
            sync_directory(quarantined.parent().ok_or("隔离槽没有父目录")?)
        }),
        Ok(true) => Err("原位已被其他条目占用".into()),
        Err(error) => Err(format!("检查原位是否可恢复失败: {error}")),
    };
    format!(
        "{label} {reason}，已拒绝删除{}",
        restore
            .err()
            .map(|error| format!("；恢复原位也失败: {error}"))
            .unwrap_or_default()
    )
}

fn quarantine_artifact(
    source: &Path,
    quarantined: &Path,
    expected_sha256: &str,
    label: &str,
    fingerprint: fn(&Path) -> Result<String, String>,
) -> Result<bool, String> {
    let source_present = path_present(source)?;
    let quarantined_present = path_present(quarantined)?;
    if source_present && quarantined_present {
        return Err(format!("{label} 同时出现在最终路径与隔离槽，已拒绝删除"));
    }
    if source_present {
        rename_no_replace(source, quarantined)?;
        let durable = source
            .parent()
            .ok_or_else(|| "bundle 条目没有父目录".to_string())
            .and_then(sync_directory)
            .and_then(|()| {
                quarantined
                    .parent()
                    .ok_or_else(|| "隔离槽没有父目录".to_string())
                    .and_then(sync_directory)
            });
        if let Err(error) = durable {
            return Err(quarantine_rejection_with_restore(
                source,
                quarantined,
                label,
                format!("隔离 rename 后持久化目录失败: {error}"),
            ));
        }
    }
    if !path_present(quarantined)? {
        return Ok(false);
    }
    let actual = fingerprint(quarantined);
    if matches!(actual, Ok(ref value) if value == expected_sha256) {
        return Ok(true);
    }

    // 隔离后才复验指纹；“不匹配”与“无法计算”都必须
    // 尽力原子恢复原位，不会将未证明内容留在待删除槽中。
    let reason = actual
        .err()
        .map(|error| format!("指纹计算失败: {error}"))
        .unwrap_or_else(|| "指纹与 durable ownership 收据不匹配".into());
    Err(quarantine_rejection_with_restore(
        source,
        quarantined,
        label,
        reason,
    ))
}

/// 在 package 递归删除尚未 durable 授权前，任一后续槽位
/// 校验失败都应尽力把已验证的早先条目原子放回。
/// 这不覆盖重新出现的原路径，也不遍历/删除隔离内容。
fn restore_quarantined_before_delete(
    quarantined: &[(PathBuf, PathBuf, String)],
) -> Result<(), String> {
    let mut failures = Vec::new();
    for (source, slot, label) in quarantined.iter().rev() {
        let slot_present = match path_present(slot) {
            Ok(value) => value,
            Err(error) => {
                failures.push(format!("{label}: 检查隔离槽失败: {error}"));
                continue;
            }
        };
        if !slot_present {
            continue;
        }
        match path_present(source) {
            Ok(false) => match rename_no_replace(slot, source).and_then(|()| {
                sync_directory(source.parent().ok_or("恢复路径没有父目录")?)?;
                sync_directory(slot.parent().ok_or("隔离槽没有父目录")?)
            }) {
                Ok(()) => {}
                Err(error) => failures.push(format!("{label}: 原子恢复失败: {error}")),
            },
            Ok(true) => failures.push(format!("{label}: 原路径已重新出现，不覆盖")),
            Err(error) => failures.push(format!("{label}: 检查原路径失败: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn removal_error_with_restore(error: String, quarantined: &[(PathBuf, PathBuf, String)]) -> String {
    match restore_quarantined_before_delete(quarantined) {
        Ok(()) => format!("{error}；已原子恢复本次早先隔离的条目"),
        Err(restore_error) => {
            format!("{error}；早先隔离条目未能全部恢复: {restore_error}")
        }
    }
}

/// 不调用 npm，不解析当前可变 manifest。只将收据里的
/// 精确 package 与固定 launcher 隔离、隔离后复验、再删除。
pub(crate) fn remove_pinned_bundle_exact(
    prefix: &Path,
    package: &str,
    command: &str,
    receipt: &PinnedBundleReceipt,
) -> Result<(), String> {
    validate_receipt_shape(prefix, package, command, receipt)?;
    let package_path = package_dir(prefix, package)?;
    let launcher_paths = super::runtime::cli_launcher_paths(prefix, command)?;
    let quarantine = prefix.join(format!(".codecli-owned-remove-{command}"));
    let journal_path = quarantine.join("receipt.json");
    let expected_journal = RemovalJournal {
        schema_version: REMOVAL_JOURNAL_SCHEMA_VERSION,
        package: package.to_string(),
        command: command.to_string(),
        receipt: receipt.clone(),
    };

    let originals_present = path_present(&package_path)?
        || launcher_paths
            .iter()
            .map(|path| path_present(path))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|present| present);

    if path_present(&quarantine)? {
        let metadata = std::fs::symlink_metadata(&quarantine)
            .map_err(|error| format!("检查 bundle 隔离目录失败: {error}"))?;
        if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err("bundle 隔离路径不是可信真实目录".into());
        }
        match read_removal_journal(&journal_path)? {
            Some(actual) if actual == expected_journal => {}
            Some(_) => return Err("bundle 隔离日志与 durable ownership 收据不匹配".into()),
            None if directory_is_empty(&quarantine)? => {
                std::fs::remove_dir(&quarantine)
                    .map_err(|error| format!("清理空隔离目录失败: {error}"))?;
                sync_directory(prefix)?;
            }
            None => return Err("bundle 隔离目录存在但缺少 durable 收据日志".into()),
        }
    }

    if !path_present(&quarantine)? && !originals_present {
        return Ok(());
    }
    if !path_present(&quarantine)? {
        create_quarantine_with_journal(prefix, &quarantine, &expected_journal)?;
    }
    validate_quarantine_entries(&quarantine, launcher_paths.len())?;

    let package_slot = quarantine.join("package");
    let package_delete_authorized_path = quarantine.join("package-delete-authorized.json");
    let package_delete_authorized = match read_removal_journal(&package_delete_authorized_path)? {
        Some(actual) if actual == expected_journal => true,
        Some(_) => return Err("bundle package 删除授权与 durable ownership 收据不匹配".into()),
        None => false,
    };
    if package_delete_authorized && path_present(&package_path)? {
        return Err("bundle package 已进入 durable 删除阶段，但最终路径重新出现内容".into());
    }
    let mut reversible_quarantined = Vec::new();
    let package_quarantined = if package_delete_authorized {
        path_present(&package_slot)?
    } else {
        quarantine_artifact(
            &package_path,
            &package_slot,
            &receipt.package_sha256,
            "bundle package",
            fingerprint_package_tree,
        )?
    };
    if package_quarantined && !package_delete_authorized {
        reversible_quarantined.push((
            package_path.clone(),
            package_slot.clone(),
            "bundle package".to_string(),
        ));
    }
    let mut launcher_slots = Vec::with_capacity(launcher_paths.len());
    for (index, launcher) in launcher_paths.iter().enumerate() {
        let slot = quarantine.join(format!("launcher-{index}"));
        let key = path_relative_key(prefix, launcher)?;
        let expected = receipt
            .launcher_sha256
            .get(&key)
            .ok_or("bundle 收据缺少 launcher 指纹")?;
        let label = format!("bundle launcher {key}");
        let quarantined =
            match quarantine_artifact(launcher, &slot, expected, &label, fingerprint_launcher) {
                Ok(value) => value,
                Err(error) if !package_delete_authorized => {
                    return Err(removal_error_with_restore(error, &reversible_quarantined));
                }
                Err(error) => return Err(error),
            };
        if quarantined && !package_delete_authorized {
            reversible_quarantined.push((launcher.clone(), slot.clone(), label));
        }
        launcher_slots.push((slot, quarantined));
    }
    if let Err(error) = validate_quarantine_entries(&quarantine, launcher_paths.len()) {
        if !package_delete_authorized {
            return Err(removal_error_with_restore(error, &reversible_quarantined));
        }
        return Err(error);
    }

    // 进入 package 递归删除授权前，先将全部 launcher 槽位
    // 一次性复验。这样后一个 launcher 漂移不会让早先的
    // package/launcher 留在隔离中或先被删除。
    for (index, (slot, quarantined)) in launcher_slots.iter().enumerate() {
        if !*quarantined {
            continue;
        }
        let key = path_relative_key(prefix, &launcher_paths[index])?;
        let expected = receipt.launcher_sha256.get(&key).expect("shape checked");
        let actual = fingerprint_launcher(slot);
        if !matches!(actual, Ok(ref value) if value == expected) {
            let error = actual
                .err()
                .map(|detail| format!("隔离后 bundle launcher 复验失败 {key}: {detail}"))
                .unwrap_or_else(|| format!("隔离后 bundle launcher 在删除前发生改变: {key}"));
            if !package_delete_authorized {
                return Err(removal_error_with_restore(error, &reversible_quarantined));
            }
            return Err(error);
        }
    }

    if package_quarantined {
        if !package_delete_authorized {
            // 递归删除可能在任意子树中崩溃。必须先对完整
            // 隔离树复验，再 durable 记录“可续删”阶段。
            match fingerprint_package_tree(&package_slot) {
                Ok(actual) if actual == receipt.package_sha256 => {}
                Ok(_) => {
                    return Err(removal_error_with_restore(
                        "隔离后 bundle package 在删除授权前发生改变".into(),
                        &reversible_quarantined,
                    ));
                }
                Err(error) => {
                    return Err(removal_error_with_restore(
                        format!("隔离后 bundle package 复验失败: {error}"),
                        &reversible_quarantined,
                    ));
                }
            }
            if let Err(error) =
                publish_package_delete_authorization(prefix, &quarantine, &expected_journal)
            {
                // 若授权文件尚未出现，还没进入不可逆的递归删除阶段。
                // 若文件已经发布（但 fsync 报错），保留隔离状态供下次续做。
                if !path_present(&package_delete_authorized_path)? {
                    return Err(removal_error_with_restore(error, &reversible_quarantined));
                }
                return Err(error);
            }
        }
        remove_path_no_follow(&package_slot)?;
    }
    if path_present(&package_delete_authorized_path)? {
        std::fs::remove_file(&package_delete_authorized_path)
            .map_err(|error| format!("删除 bundle package 删除授权失败: {error}"))?;
        sync_directory(&quarantine)?;
    }
    for (index, (slot, quarantined)) in launcher_slots.iter().enumerate() {
        if *quarantined {
            let key = path_relative_key(prefix, &launcher_paths[index])?;
            let expected = receipt.launcher_sha256.get(&key).expect("shape checked");
            if fingerprint_launcher(slot)? != *expected {
                return Err(format!("隔离后 bundle launcher 在删除前发生改变: {key}"));
            }
            remove_path_no_follow(slot)?;
        }
    }

    if path_present(&package_path)?
        || launcher_paths
            .iter()
            .map(|path| path_present(path))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|present| present)
    {
        return Err("bundle 精确隔离后最终路径仍有残留".into());
    }
    validate_quarantine_entries(&quarantine, launcher_paths.len())?;
    std::fs::remove_file(&journal_path)
        .map_err(|error| format!("删除 bundle 隔离日志失败: {error}"))?;
    sync_directory(&quarantine)?;
    std::fs::remove_dir(&quarantine)
        .map_err(|error| format!("删除空 bundle 隔离目录失败: {error}"))?;
    sync_directory(prefix)?;
    Ok(())
}

fn publish_bundle(
    stage: &BundleStage,
    final_prefix: &Path,
    package: &str,
    command: &str,
    stage_script: &Path,
    stage_launchers: &[PathBuf],
) -> Result<(PathBuf, PathBuf, Vec<PathBuf>), String> {
    let stage_package = package_dir(&stage.prefix, package)?;
    let final_package = package_dir(final_prefix, package)?;
    let script_relative = stage_script
        .strip_prefix(&stage.prefix)
        .map_err(|_| "package bin 不在 staging prefix 内")?;
    let final_script = final_prefix.join(script_relative);

    let mut entries = vec![(stage_package, final_package.clone())];
    let mut final_launchers = Vec::new();
    for launcher in stage_launchers {
        let relative = launcher
            .strip_prefix(&stage.prefix)
            .map_err(|_| "launcher 不在 staging prefix 内")?;
        let final_launcher = final_prefix.join(relative);
        entries.push((launcher.clone(), final_launcher.clone()));
        final_launchers.push(final_launcher);
    }

    ensure_real_directory_chain(final_prefix)?;
    for (_, destination) in &entries {
        ensure_real_directory_chain(destination.parent().ok_or("发布路径没有父目录")?)?;
        match std::fs::symlink_metadata(destination) {
            Ok(_) => {
                return Err(format!(
                    "固定 bundle 目标已存在，拒绝覆盖或接管: {}；外层必须先用可信 ownership 证明并精确清理旧包",
                    destination.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查 bundle 目标失败: {error}")),
        }
    }

    for (published_count, (source, destination)) in entries.iter().enumerate() {
        if let Err(error) = rename_no_replace(source, destination) {
            return Err(format!(
                "no-replace 原子发布 bundle 条目失败 {}: {error}；已保留 {published_count} 个完整已发布条目，由首次 rename 前持久化的收据精确恢复",
                destination.display(),
            ));
        }
        let durable = sync_directory(destination.parent().ok_or("发布目标没有父目录")?)
            .and_then(|()| sync_directory(source.parent().ok_or("staging 源没有父目录")?));
        if let Err(error) = durable {
            return Err(format!(
                "原子发布后持久化目录失败: {error}；不做非事务性递归回滚，由 durable 收据精确恢复"
            ));
        }
    }

    if let Err(error) = super::runtime::validate_cli_launchers(final_prefix, command, &final_script)
    {
        return Err(format!(
            "bundle 发布后 launcher 复验失败: {error}；保留完整原子条目供 durable 收据恢复"
        ));
    }
    Ok((final_package, final_script, final_launchers))
}

#[derive(Debug, Clone)]
pub struct PinnedBundleInstall {
    pub package_dir: PathBuf,
    pub bin_script: PathBuf,
    pub launcher_paths: Vec<PathBuf>,
    /// Feishu 外层在完成 ownership/Pending 检查后，可仅执行此已验证脚本。
    pub install_script: Option<PathBuf>,
    /// Codex 当前平台 alias 包目录；其他 bundle 为 None。
    pub native_alias_dir: Option<PathBuf>,
}

pub struct PreparedPinnedBundle {
    stage: BundleStage,
    final_prefix: PathBuf,
    package: &'static str,
    command: &'static str,
    stage_script: PathBuf,
    stage_launchers: Vec<PathBuf>,
    install_script_relative: Option<PathBuf>,
    native_alias_relative: Option<PathBuf>,
    receipt: PinnedBundleReceipt,
}

impl PreparedPinnedBundle {
    pub(crate) fn receipt(&self) -> &PinnedBundleReceipt {
        &self.receipt
    }

    /// Feishu 的已 pin install.js 必须只在私有 staging 内
    /// 运行。返回的所有路径都仍在 staging，不是最终 prefix。
    pub(crate) fn staged_install(&self) -> PinnedBundleInstall {
        let stage_package = package_dir(&self.stage.prefix, self.package)
            .expect("prepared package identity was validated");
        PinnedBundleInstall {
            package_dir: stage_package,
            bin_script: self.stage_script.clone(),
            launcher_paths: self.stage_launchers.clone(),
            install_script: self
                .install_script_relative
                .as_ref()
                .map(|relative| self.stage.prefix.join(relative)),
            native_alias_dir: self
                .native_alias_relative
                .as_ref()
                .map(|relative| self.stage.prefix.join(relative)),
        }
    }

    /// 已审计的 Feishu install.js 在 staging 内生成 native
    /// 后，重新同步整树并生成包含 native 的最终收据。
    pub(crate) fn refresh_receipt_after_staging_changes(&mut self) -> Result<(), String> {
        let stage_package = package_dir(&self.stage.prefix, self.package)?;
        sync_directory_tree(&stage_package, 0)?;
        sync_directory(stage_package.parent().ok_or("staging package 没有父目录")?)?;
        for launcher in &self.stage_launchers {
            sync_directory(launcher.parent().ok_or("staging launcher 没有父目录")?)?;
        }
        self.receipt = build_receipt(
            &self.stage.prefix,
            self.package,
            self.command,
            &self.stage_launchers,
        )?;
        Ok(())
    }

    /// 调用者必须先用 CAS 将 `receipt()` 写入 Pending
    /// ownership，然后才能调用 publish。
    pub fn publish(self) -> Result<PinnedBundleInstall, String> {
        let stage_package = package_dir(&self.stage.prefix, self.package)?;
        // 持久化 ownership 可能耗时，所以在首个最终 rename
        // 前再次自底向上 fsync 整个 package，并复算收据。
        sync_directory_tree(&stage_package, 0)?;
        sync_directory(stage_package.parent().ok_or("staging package 没有父目录")?)?;
        for launcher in &self.stage_launchers {
            sync_directory(launcher.parent().ok_or("staging launcher 没有父目录")?)?;
        }
        let current = build_receipt(
            &self.stage.prefix,
            self.package,
            self.command,
            &self.stage_launchers,
        )?;
        if current != self.receipt {
            return Err("固定 bundle staging 在收据持久化后发生改变，已拒绝发布".into());
        }

        let (package_dir, bin_script, launcher_paths) = publish_bundle(
            &self.stage,
            &self.final_prefix,
            self.package,
            self.command,
            &self.stage_script,
            &self.stage_launchers,
        )?;
        verify_pinned_bundle_receipt(
            &self.final_prefix,
            self.package,
            self.command,
            &self.receipt,
        )?;
        Ok(PinnedBundleInstall {
            package_dir,
            bin_script,
            launcher_paths,
            install_script: self
                .install_script_relative
                .map(|relative| self.final_prefix.join(relative)),
            native_alias_dir: self
                .native_alias_relative
                .map(|relative| self.final_prefix.join(relative)),
        })
    }
}

struct PreparedBundleParts {
    package: &'static str,
    command: &'static str,
    stage_script: PathBuf,
    stage_launchers: Vec<PathBuf>,
    install_script_relative: Option<PathBuf>,
    native_alias_relative: Option<PathBuf>,
}

fn finish_preparation(
    stage: BundleStage,
    final_prefix: &Path,
    parts: PreparedBundleParts,
) -> Result<PreparedPinnedBundle, String> {
    let stage_package = package_dir(&stage.prefix, parts.package)?;
    // fetch_package 后 Claude stub/Codex alias/Feishu closure 仍会改变
    // package，因此必须在所有修改完成后再同步整树。
    sync_directory_tree(&stage_package, 0)?;
    sync_directory(stage_package.parent().ok_or("staging package 没有父目录")?)?;
    for launcher in &parts.stage_launchers {
        sync_directory(launcher.parent().ok_or("staging launcher 没有父目录")?)?;
    }
    sync_directory(&stage.prefix)?;
    let receipt = build_receipt(
        &stage.prefix,
        parts.package,
        parts.command,
        &parts.stage_launchers,
    )?;
    Ok(PreparedPinnedBundle {
        stage,
        final_prefix: final_prefix.to_path_buf(),
        package: parts.package,
        command: parts.command,
        stage_script: parts.stage_script,
        stage_launchers: parts.stage_launchers,
        install_script_relative: parts.install_script_relative,
        native_alias_relative: parts.native_alias_relative,
        receipt,
    })
}

pub fn prepare_claude_bundle(prefix: &Path) -> Result<PreparedPinnedBundle, String> {
    let platform = current_platform()?;
    let native = claude_native(platform);
    let client = pinned_http_client()?;
    let stage = BundleStage::create(prefix)?;
    let top = package_dir(&stage.prefix, CLAUDE_TOP.name)?;
    fetch_package(&client, &stage, 0, CLAUDE_TOP, &top)?;
    let native_staging = stage.root.join("claude-native");
    fetch_package(&client, &stage, 1, native.spec, &native_staging)?;
    replace_with_verified_native(
        &native_staging.join(native.source_binary),
        &top.join("bin").join("claude.exe"),
    )?;
    // 不执行 top 包中的 install.cjs/postinstall。
    let script = top.join("bin").join("claude.exe");
    let launchers =
        super::runtime::create_and_validate_cli_launchers(&stage.prefix, "claude", &script)?;
    finish_preparation(
        stage,
        prefix,
        PreparedBundleParts {
            package: CLAUDE_TOP.name,
            command: "claude",
            stage_script: script,
            stage_launchers: launchers,
            install_script_relative: None,
            native_alias_relative: None,
        },
    )
}

pub fn prepare_codex_bundle(prefix: &Path) -> Result<PreparedPinnedBundle, String> {
    let platform = current_platform()?;
    let native = codex_native(platform);
    let client = pinned_http_client()?;
    let stage = BundleStage::create(prefix)?;
    let top = package_dir(&stage.prefix, CODEX_TOP.name)?;
    fetch_package(&client, &stage, 0, CODEX_TOP, &top)?;
    // native tar 的 manifest name 仍是 @openai/codex，但必须发布在 alias 目录。
    let native_alias = nested_package_dir(&top, native.alias)?;
    fetch_package(&client, &stage, 1, native.spec, &native_alias)?;
    trusted_regular_file(
        &native_alias.join(native.source_binary),
        "Codex 当前平台 native binary",
    )?;
    let script = top.join("bin").join("codex.js");
    let launchers =
        super::runtime::create_and_validate_cli_launchers(&stage.prefix, "codex", &script)?;
    let alias_relative = native_alias
        .strip_prefix(&stage.prefix)
        .map_err(|_| "Codex alias 路径不在 staging prefix")?
        .to_path_buf();
    finish_preparation(
        stage,
        prefix,
        PreparedBundleParts {
            package: CODEX_TOP.name,
            command: "codex",
            stage_script: script,
            stage_launchers: launchers,
            install_script_relative: None,
            native_alias_relative: Some(alias_relative),
        },
    )
}

pub fn prepare_feishu_bundle(prefix: &Path) -> Result<PreparedPinnedBundle, String> {
    let _ = current_platform()?;
    let client = pinned_http_client()?;
    let stage = BundleStage::create(prefix)?;
    let top = package_dir(&stage.prefix, FEISHU_TOP.name)?;
    fetch_package(&client, &stage, 0, FEISHU_TOP, &top)?;
    for (index, dependency) in FEISHU_CLOSURE.iter().copied().enumerate() {
        let destination = nested_package_dir(&top, dependency.name)?;
        fetch_package(&client, &stage, index + 1, dependency, &destination)?;
    }
    let script = top.join("scripts").join("run.js");
    let install_script_relative = top
        .join("scripts")
        .join("install.js")
        .strip_prefix(&stage.prefix)
        .map_err(|_| "Feishu install.js 不在 staging prefix")?
        .to_path_buf();
    trusted_regular_file(
        &stage.prefix.join(&install_script_relative),
        "Feishu install.js",
    )?;
    let launchers =
        super::runtime::create_and_validate_cli_launchers(&stage.prefix, "lark-cli", &script)?;
    finish_preparation(
        stage,
        prefix,
        PreparedBundleParts {
            package: FEISHU_TOP.name,
            command: "lark-cli",
            stage_script: script,
            stage_launchers: launchers,
            install_script_relative: Some(install_script_relative),
            native_alias_relative: None,
        },
    )
}

#[cfg(test)]
fn install_feishu_bundle(prefix: &Path) -> Result<PinnedBundleInstall, String> {
    prepare_feishu_bundle(prefix)?.publish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Cursor;
    use tar::{Builder, EntryType, Header};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
            let path = temp.join(format!(
                "codecli-pinned-test-{label}-{}-{}",
                std::process::id(),
                STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("create test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone, Copy)]
    struct TestEntry<'a> {
        path: &'a str,
        kind: EntryType,
        body: &'a [u8],
        raw_path: bool,
    }

    fn tarball(entries: &[TestEntry<'_>]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        for item in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(item.kind);
            header.set_mode(if item.kind.is_dir() { 0o755 } else { 0o644 });
            header.set_size(item.body.len() as u64);
            if item.raw_path {
                let name = item.path.as_bytes();
                assert!(name.len() < 100);
                header.as_mut_bytes()[..100].fill(0);
                header.as_mut_bytes()[..name.len()].copy_from_slice(name);
            } else {
                header.set_path(item.path).expect("set safe test path");
            }
            header.set_cksum();
            builder
                .append(&header, Cursor::new(item.body))
                .expect("append test entry");
        }
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip")
    }

    fn pax_record(key: &str, value: &str) -> Vec<u8> {
        let payload = format!(" {key}={value}\n");
        let mut length = payload.len() + 1;
        loop {
            let next = payload.len() + length.to_string().len();
            if next == length {
                return format!("{length}{payload}").into_bytes();
            }
            length = next;
        }
    }

    fn pax_size_override_tarball(advertised_size: u64) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        let pax = pax_record("size", &advertised_size.to_string());
        let mut pax_header = Header::new_gnu();
        pax_header.set_entry_type(EntryType::XHeader);
        pax_header.set_mode(0o644);
        pax_header.set_size(pax.len() as u64);
        pax_header
            .set_path("package/PaxHeaders/file")
            .expect("pax path");
        pax_header.set_cksum();
        builder
            .append(&pax_header, Cursor::new(pax))
            .expect("append pax header");

        let mut file_header = Header::new_gnu();
        file_header.set_entry_type(EntryType::Regular);
        file_header.set_mode(0o644);
        file_header.set_size(1);
        file_header.set_path("package/file").expect("file path");
        file_header.set_cksum();
        builder
            .append(&file_header, Cursor::new(b"x"))
            .expect("append pax target");
        let encoder = builder.into_inner().expect("finish pax tar");
        encoder.finish().expect("finish pax gzip")
    }

    fn file_with(bytes: &[u8], root: &TestDir, name: &str) -> File {
        let path = root.0.join(name);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .expect("create test archive");
        file.write_all(bytes).expect("write test archive");
        file
    }

    #[test]
    fn sri_mismatch_is_rejected_and_valid_sri_rewinds() {
        let root = TestDir::new("sri");
        let bytes = b"immutable bytes";
        let mut file = file_with(bytes, &root, "payload");
        assert!(verify_sri_and_rewind(
            &mut file,
            "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
        )
        .is_err());

        let encoded = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes));
        verify_sri_and_rewind(&mut file, &format!("sha512-{encoded}")).expect("valid SRI");
        assert_eq!(file.stream_position().expect("position"), 0);
    }

    #[test]
    fn malicious_tar_entries_are_rejected() {
        let cases = [
            ("traversal", "package/../escape", EntryType::Regular, true),
            ("absolute", "/package/escape", EntryType::Regular, true),
            ("symlink", "package/link", EntryType::Symlink, false),
            ("hardlink", "package/link", EntryType::Link, false),
            ("fifo", "package/fifo", EntryType::Fifo, false),
        ];
        for (index, (label, path, kind, raw_path)) in cases.into_iter().enumerate() {
            let root = TestDir::new(label);
            let bytes = tarball(&[TestEntry {
                path,
                kind,
                body: b"x",
                raw_path,
            }]);
            let mut file = file_with(&bytes, &root, "bad.tgz");
            let destination = root.0.join(format!("out-{index}"));
            assert!(
                unpack_verified_archive(&mut file, &destination).is_err(),
                "{label} must fail closed"
            );
        }
    }

    #[test]
    fn pax_size_override_cannot_bypass_single_file_limit() {
        let root = TestDir::new("pax-size");
        let bytes = pax_size_override_tarball(MAX_FILE_BYTES + 1);
        let mut file = file_with(&bytes, &root, "pax.tgz");
        let error = unpack_verified_archive(&mut file, &root.0.join("out"))
            .expect_err("PAX effective size must be bounded");
        assert!(error.contains("单文件大小超限"), "{error}");
    }

    #[test]
    fn duplicate_tar_paths_are_rejected_case_insensitively() {
        let root = TestDir::new("duplicate");
        let bytes = tarball(&[
            TestEntry {
                path: "package/file.txt",
                kind: EntryType::Regular,
                body: b"one",
                raw_path: false,
            },
            TestEntry {
                path: "package/FILE.txt",
                kind: EntryType::Regular,
                body: b"two",
                raw_path: false,
            },
        ]);
        let mut file = file_with(&bytes, &root, "duplicate.tgz");
        assert!(unpack_verified_archive(&mut file, &root.0.join("out")).is_err());
    }

    #[test]
    fn manifest_name_and_version_must_match_exactly() {
        let root = TestDir::new("manifest");
        let bytes = tarball(&[TestEntry {
            path: "package/package.json",
            kind: EntryType::Regular,
            body: br#"{"name":"@openai/codex","version":"0.144.4"}"#,
            raw_path: false,
        }]);
        let mut file = file_with(&bytes, &root, "manifest.tgz");
        let output = root.0.join("out");
        unpack_verified_archive(&mut file, &output).expect("safe archive extracts");
        assert!(validate_manifest(&output, CODEX_TOP).is_err());
    }

    #[test]
    fn top_manifest_rejects_extra_bin_delete_targets() {
        let root = TestDir::new("extra-bin");
        let package = root.0.join("package");
        std::fs::create_dir(&package).expect("package dir");
        std::fs::write(
            package.join("package.json"),
            br#"{"name":"@openai/codex","version":"0.144.5","bin":{"codex":"bin/codex.js","claude":"bin/codex.js","user-tool":"bin/codex.js"}}"#,
        )
        .expect("manifest");
        let error = validate_exact_top_manifest(&package, "@openai/codex", "codex")
            .expect_err("extra bins must never become delete targets");
        assert!(error.contains("只能声明"));
    }

    #[test]
    fn platform_mapping_is_closed_and_linux_is_rejected() {
        assert_eq!(
            supported_platform("macos", "aarch64").unwrap(),
            SupportedPlatform::DarwinArm64
        );
        assert_eq!(
            supported_platform("darwin", "x64").unwrap(),
            SupportedPlatform::DarwinX64
        );
        assert_eq!(
            supported_platform("windows", "aarch64").unwrap(),
            SupportedPlatform::Win32Arm64
        );
        assert_eq!(
            supported_platform("win32", "x86_64").unwrap(),
            SupportedPlatform::Win32X64
        );
        assert!(supported_platform("linux", "x86_64").is_err());
        assert!(supported_platform("freebsd", "x86_64").is_err());
    }

    #[test]
    fn codex_native_uses_alias_path_but_real_manifest_identity() {
        let top = Path::new("/prefix/lib/node_modules/@openai/codex");
        for platform in [
            SupportedPlatform::DarwinArm64,
            SupportedPlatform::DarwinX64,
            SupportedPlatform::Win32Arm64,
            SupportedPlatform::Win32X64,
        ] {
            let selected = codex_native(platform);
            assert_eq!(selected.spec.name, "@openai/codex");
            assert!(selected.spec.version.starts_with("0.144.5-"));
            let alias = nested_package_dir(top, selected.alias).expect("alias path");
            assert!(alias.starts_with(top.join("node_modules/@openai")));
            assert_eq!(
                alias.file_name().and_then(|value| value.to_str()),
                Some(selected.alias.rsplit('/').next().unwrap())
            );
        }
    }

    #[test]
    fn audited_top_and_native_package_tables_are_exact() {
        fn assert_spec(actual: PackageSpec, expected: (&str, &str, &str, &str)) {
            assert_eq!(
                (actual.name, actual.version, actual.url, actual.sri),
                expected
            );
            validate_official_url(actual.url).expect("official registry URL");
            let digest = actual.sri.strip_prefix("sha512-").expect("sha512 SRI");
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(digest)
                    .expect("valid SRI base64")
                    .len(),
                64
            );
        }

        let top = [
            (
                CLAUDE_TOP,
                (
                    "@anthropic-ai/claude-code",
                    "2.1.211",
                    "https://registry.npmjs.org/@anthropic-ai/claude-code/-/claude-code-2.1.211.tgz",
                    "sha512-yGhXSF9YfHoVGe0S6N9ky5uajx79f+vt6ZT3HhBJLFSjJtiGEs67H0h93iTdOvPU/wOffijpTUAn76U/+vQnTQ==",
                ),
            ),
            (
                CODEX_TOP,
                (
                    "@openai/codex",
                    "0.144.5",
                    "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5.tgz",
                    "sha512-jjB+K+OMv572mKhS+2QuLxWXDJNdpwbPenf+V+8bdq7wg4Scqt3cn6WEekD8wPqDVZqck0HSX17K9rD9kbDJQA==",
                ),
            ),
            (
                FEISHU_TOP,
                (
                    "@larksuite/cli",
                    "1.0.70",
                    "https://registry.npmjs.org/@larksuite/cli/-/cli-1.0.70.tgz",
                    "sha512-6x5AXaH5eWHYKfzpOgWVoanpYRFq5O1v02OYDToHw8KgcNY9zwZ8KvoM2eQs9B6oO07QDqRUMpM1XjsVGb1dCA==",
                ),
            ),
        ];
        for (actual, expected) in top {
            assert_spec(actual, expected);
        }

        let platforms = [
            SupportedPlatform::DarwinArm64,
            SupportedPlatform::DarwinX64,
            SupportedPlatform::Win32Arm64,
            SupportedPlatform::Win32X64,
        ];
        let claude_expected = [
            (
                "@anthropic-ai/claude-code-darwin-arm64",
                "2.1.211",
                "https://registry.npmjs.org/@anthropic-ai/claude-code-darwin-arm64/-/claude-code-darwin-arm64-2.1.211.tgz",
                "sha512-ogsLXqbHlHSFE9ApgpoeoP6wXJKkcUyYM4f8rrAbTvQStvqQ/bpHLV5mgbuEGn/N9NPWBQt826bfH/XvlYi0kg==",
                "@anthropic-ai/claude-code-darwin-arm64",
                "claude",
            ),
            (
                "@anthropic-ai/claude-code-darwin-x64",
                "2.1.211",
                "https://registry.npmjs.org/@anthropic-ai/claude-code-darwin-x64/-/claude-code-darwin-x64-2.1.211.tgz",
                "sha512-t3AgChHNAe6Djp//73U6SoeRbmc2A/ia6FHU3gJuMyvCeQ8I9c5PvXOhs2p37H0+bYiJMoXxI6MdmY9sPEFa8g==",
                "@anthropic-ai/claude-code-darwin-x64",
                "claude",
            ),
            (
                "@anthropic-ai/claude-code-win32-arm64",
                "2.1.211",
                "https://registry.npmjs.org/@anthropic-ai/claude-code-win32-arm64/-/claude-code-win32-arm64-2.1.211.tgz",
                "sha512-W04nNnYZl54o5Dmr69nSCz9aEG3TIw4Vr2nmeNQcqJjIzHTy19xmXKbioY25yCHQgrHe/AHMVMzWneAp8yylPw==",
                "@anthropic-ai/claude-code-win32-arm64",
                "claude.exe",
            ),
            (
                "@anthropic-ai/claude-code-win32-x64",
                "2.1.211",
                "https://registry.npmjs.org/@anthropic-ai/claude-code-win32-x64/-/claude-code-win32-x64-2.1.211.tgz",
                "sha512-/pXHWP02ni+xM37QP0Yrn0rG3K2MKq47nxB5xuUrMirpQG1zA5orFtCiP4hmQaiYICRgW39ZmQdEQpsvt2t+pg==",
                "@anthropic-ai/claude-code-win32-x64",
                "claude.exe",
            ),
        ];
        let codex_expected = [
            (
                "@openai/codex",
                "0.144.5-darwin-arm64",
                "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5-darwin-arm64.tgz",
                "sha512-zcT6NfBCqLFt+BReNSETTZW6v6PdbH0dzNtm9j7l7mDGqwPbKZDGJdnpkBao2389I0ZacyIKgSZoI0vez1d4Dw==",
                "@openai/codex-darwin-arm64",
                "vendor/aarch64-apple-darwin/bin/codex",
            ),
            (
                "@openai/codex",
                "0.144.5-darwin-x64",
                "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5-darwin-x64.tgz",
                "sha512-//Mo0m1MwaoT6psu5xsmofXpKx4/0irIkeq10xJvk59+886EG355ibjA+ZmlRcKhE3bLjsKD7p81nTbAdRL/bw==",
                "@openai/codex-darwin-x64",
                "vendor/x86_64-apple-darwin/bin/codex",
            ),
            (
                "@openai/codex",
                "0.144.5-win32-arm64",
                "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5-win32-arm64.tgz",
                "sha512-0Pj7iqjEOEvPQPO3kFfCy9vGX4BTu76ChFFZHr2eNNIfVc3FOENAv/X98u4L+iIUtDOK9DbqmfUudW3DPapshg==",
                "@openai/codex-win32-arm64",
                "vendor/aarch64-pc-windows-msvc/bin/codex.exe",
            ),
            (
                "@openai/codex",
                "0.144.5-win32-x64",
                "https://registry.npmjs.org/@openai/codex/-/codex-0.144.5-win32-x64.tgz",
                "sha512-DnsSTlnnzleTxvLwIGnBitKInscxn2I7qASqosS8Fv+qysBygd+ZiBn/SQsRCgQ28PAlsNzmd3Gf3ZTecolAmg==",
                "@openai/codex-win32-x64",
                "vendor/x86_64-pc-windows-msvc/bin/codex.exe",
            ),
        ];
        for ((platform, expected), codex) in platforms
            .into_iter()
            .zip(claude_expected)
            .zip(codex_expected)
        {
            let claude = claude_native(platform);
            assert_spec(
                claude.spec,
                (expected.0, expected.1, expected.2, expected.3),
            );
            assert_eq!(
                (claude.alias, claude.source_binary),
                (expected.4, expected.5)
            );

            let selected = codex_native(platform);
            assert_spec(selected.spec, (codex.0, codex.1, codex.2, codex.3));
            assert_eq!((selected.alias, selected.source_binary), (codex.4, codex.5));
        }
    }

    #[test]
    fn feishu_dependency_closure_is_fully_pinned() {
        let expected = [
            ("@clack/prompts", "1.7.0", "https://registry.npmjs.org/@clack/prompts/-/prompts-1.7.0.tgz", "sha512-y7/yvZ2TPAnR9+jnc00klvNNLkJiXFFrQA/hlLCcxA9a2A4zQIOimyFQ9XfwYKiGD1fb5GY8vbKIIgO8d5Tb2A=="),
            ("@clack/core", "1.4.3", "https://registry.npmjs.org/@clack/core/-/core-1.4.3.tgz", "sha512-/kr3UWNtdJfxZtPgDqUOmG2pvwlmcLGheex5yiZKdwbzZJxhV+HMNR9QNmyY5cGwTNV6LrR7Jtp+KjhUAP1qBQ=="),
            ("fast-string-width", "3.0.2", "https://registry.npmjs.org/fast-string-width/-/fast-string-width-3.0.2.tgz", "sha512-gX8LrtNEI5hq8DVUfRQMbr5lpaS4nMIWV+7XEbXk2b8kiQIizgnlr12B4dA3ZEx3308ze0O4Q1R+cHts8kyUJg=="),
            ("fast-string-truncated-width", "3.0.3", "https://registry.npmjs.org/fast-string-truncated-width/-/fast-string-truncated-width-3.0.3.tgz", "sha512-0jjjIEL6+0jag3l2XWWizO64/aZVtpiGE3t0Zgqxv0DPuxiMjvB3M24fCyhZUO4KomJQPj3LTSUnDP3GpdwC0g=="),
            ("fast-wrap-ansi", "0.2.2", "https://registry.npmjs.org/fast-wrap-ansi/-/fast-wrap-ansi-0.2.2.tgz", "sha512-7F2Fl+TjRSenLqlU3UjSH0iyqopqoZIu7eZVpEirP2g1GtWa2G/ecEmBdgz31+Mxr+ELclgg6sokpSFIQiZ02Q=="),
            ("sisteransi", "1.0.5", "https://registry.npmjs.org/sisteransi/-/sisteransi-1.0.5.tgz", "sha512-bLGGlR1QxBcynn2d5YmDX4MGjlZvy2MRBDRNHLJ8VI6l6+9FUiyTFNJ0IveOSP0bcXgVDPRcfGqA0pjaqUpfVg=="),
        ];
        assert_eq!(FEISHU_CLOSURE.len(), expected.len());
        for (spec, (name, version, url, sri)) in FEISHU_CLOSURE.iter().zip(expected) {
            assert_eq!(
                (spec.name, spec.version, spec.url, spec.sri),
                (name, version, url, sri)
            );
            validate_official_url(spec.url).expect("official registry URL");
            assert!(spec.sri.starts_with("sha512-"));
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(spec.sri.trim_start_matches("sha512-"))
                    .expect("valid base64")
                    .len(),
                64
            );
        }
    }

    #[test]
    fn publish_refuses_to_replace_an_existing_package() {
        let root = TestDir::new("no-clobber");
        let prefix = root.0.join("final-prefix");
        let stage = BundleStage::create(&prefix).expect("stage");
        let top = package_dir(&stage.prefix, "example-cli").expect("stage package path");
        std::fs::create_dir_all(top.join("bin")).expect("create stage package");
        let script = top.join("bin/example.js");
        std::fs::write(&script, b"#!/usr/bin/env node\n").expect("write node bin");
        let launchers = super::super::runtime::create_and_validate_cli_launchers(
            &stage.prefix,
            "example",
            &script,
        )
        .expect("stage launcher");

        let existing = package_dir(&prefix, "example-cli").expect("final package path");
        std::fs::create_dir_all(&existing).expect("create existing package");
        std::fs::write(existing.join("user-sentinel"), b"do not replace").expect("write sentinel");
        assert!(publish_bundle(
            &stage,
            &prefix,
            "example-cli",
            "example",
            &script,
            &launchers
        )
        .is_err());
        assert_eq!(
            std::fs::read(existing.join("user-sentinel")).expect("sentinel remains"),
            b"do not replace"
        );
    }

    fn create_test_codex_bundle(prefix: &Path) -> (PathBuf, Vec<PathBuf>, PinnedBundleReceipt) {
        let package = package_dir(prefix, "@openai/codex").expect("package path");
        std::fs::create_dir_all(package.join("bin")).expect("package bin");
        std::fs::write(
            package.join("package.json"),
            br#"{"name":"@openai/codex","version":"0.144.5","bin":{"codex":"bin/codex.js"}}"#,
        )
        .expect("exact manifest");
        let script = package.join("bin/codex.js");
        std::fs::write(&script, b"#!/usr/bin/env node\nconsole.log('0.144.5')\n")
            .expect("node bin");
        let launchers =
            super::super::runtime::create_and_validate_cli_launchers(prefix, "codex", &script)
                .expect("launchers");
        let receipt = build_receipt(prefix, "@openai/codex", "codex", &launchers)
            .expect("pre-publish receipt");
        (package, launchers, receipt)
    }

    fn injected_fingerprint_failure(_path: &Path) -> Result<String, String> {
        Err("注入的指纹 I/O 失败".into())
    }

    #[test]
    fn fingerprint_error_restores_current_quarantine_slot() {
        let root = TestDir::new("receipt-fingerprint-error-restore");
        let source = root.0.join("package");
        let quarantine = root.0.join("quarantine");
        let slot = quarantine.join("package");
        std::fs::create_dir(&source).expect("source directory");
        std::fs::write(source.join("keep"), b"owned bytes").expect("source content");
        std::fs::create_dir(&quarantine).expect("quarantine directory");

        let error = quarantine_artifact(
            &source,
            &slot,
            &"0".repeat(64),
            "bundle package",
            injected_fingerprint_failure,
        )
        .expect_err("fingerprint error must abort removal");
        assert!(error.contains("指纹计算失败"), "{error}");
        assert_eq!(
            std::fs::read(source.join("keep")).expect("source restored"),
            b"owned bytes"
        );
        assert!(!slot.exists());
    }

    #[test]
    fn exact_receipt_removal_never_deletes_sibling_package_or_launcher() {
        let root = TestDir::new("receipt-remove");
        let prefix = root.0.join("prefix");
        let (codex_package, codex_launchers, receipt) = create_test_codex_bundle(&prefix);
        let claude_package = package_dir(&prefix, "@anthropic-ai/claude-code").unwrap();
        std::fs::create_dir_all(&claude_package).expect("sibling package");
        std::fs::write(claude_package.join("user-sentinel"), b"keep").expect("sentinel");
        let sibling_launcher = if cfg!(windows) {
            prefix.join("claude.cmd")
        } else {
            prefix.join("bin/claude")
        };
        if let Some(parent) = sibling_launcher.parent() {
            std::fs::create_dir_all(parent).expect("sibling launcher parent");
        }
        std::fs::write(&sibling_launcher, b"user-owned launcher").expect("sibling launcher");

        remove_pinned_bundle_exact(&prefix, "@openai/codex", "codex", &receipt)
            .expect("receipt-scoped removal");
        assert!(!codex_package.exists());
        assert!(codex_launchers
            .iter()
            .all(|path| std::fs::symlink_metadata(path).is_err()));
        assert_eq!(
            std::fs::read(claude_package.join("user-sentinel")).expect("sibling remains"),
            b"keep"
        );
        assert_eq!(
            std::fs::read(sibling_launcher).expect("sibling launcher remains"),
            b"user-owned launcher"
        );
    }

    #[test]
    fn changed_package_is_restored_and_never_deleted() {
        let root = TestDir::new("receipt-drift");
        let prefix = root.0.join("prefix");
        let (package, _, receipt) = create_test_codex_bundle(&prefix);
        std::fs::write(package.join("user-file"), b"must survive").expect("drift file");

        let error = remove_pinned_bundle_exact(&prefix, "@openai/codex", "codex", &receipt)
            .expect_err("drift must fail closed");
        assert!(error.contains("指纹"), "{error}");
        assert_eq!(
            std::fs::read(package.join("user-file")).expect("drift restored"),
            b"must survive"
        );
    }

    #[test]
    fn changed_later_launcher_restores_previously_quarantined_package() {
        let root = TestDir::new("receipt-launcher-drift-rollback");
        let prefix = root.0.join("prefix");
        let (package, launchers, receipt) = create_test_codex_bundle(&prefix);
        let changed = launchers.last().expect("at least one launcher");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            remove_path_no_follow(changed).expect("remove owned launcher");
            symlink("user-owned-target", changed).expect("replace with user launcher");
        }
        #[cfg(windows)]
        {
            std::fs::write(changed, b"@echo off\r\necho user launcher\r\n")
                .expect("replace with user launcher");
        }

        let error = remove_pinned_bundle_exact(&prefix, "@openai/codex", "codex", &receipt)
            .expect_err("launcher drift must fail closed");
        assert!(error.contains("指纹") || error.contains("复验"), "{error}");
        assert!(
            package.is_dir(),
            "earlier quarantined package must be restored"
        );
        assert!(launchers
            .iter()
            .all(|path| std::fs::symlink_metadata(path).is_ok()));
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(changed).expect("changed launcher preserved"),
            PathBuf::from("user-owned-target")
        );
        #[cfg(windows)]
        assert_eq!(
            std::fs::read(changed).expect("changed launcher preserved"),
            b"@echo off\r\necho user launcher\r\n"
        );
    }

    #[test]
    fn receipt_recovers_package_only_publish_crash() {
        let root = TestDir::new("partial-publish");
        let prefix = root.0.join("prefix");
        let (package, launchers, receipt) = create_test_codex_bundle(&prefix);
        for launcher in &launchers {
            remove_path_no_follow(launcher).expect("simulate crash before launcher publish");
        }

        remove_pinned_bundle_exact(&prefix, "@openai/codex", "codex", &receipt)
            .expect("package-only crash is exactly recoverable");
        assert!(!package.exists());
    }

    #[test]
    fn durable_delete_phase_recovers_partially_removed_package_tree() {
        let root = TestDir::new("partial-delete");
        let prefix = root.0.join("prefix");
        let (package, _, receipt) = create_test_codex_bundle(&prefix);
        let quarantine = prefix.join(".codecli-owned-remove-codex");
        let journal = RemovalJournal {
            schema_version: REMOVAL_JOURNAL_SCHEMA_VERSION,
            package: "@openai/codex".into(),
            command: "codex".into(),
            receipt: receipt.clone(),
        };
        create_quarantine_with_journal(&prefix, &quarantine, &journal)
            .expect("durable quarantine journal");
        let slot = quarantine.join("package");
        rename_no_replace(&package, &slot).expect("quarantine package");
        assert_eq!(
            fingerprint_package_tree(&slot).expect("full fingerprint"),
            receipt.package_sha256
        );
        publish_package_delete_authorization(&prefix, &quarantine, &journal)
            .expect("durable delete authorization");
        std::fs::remove_file(slot.join("bin/codex.js")).expect("simulate mid-delete crash");

        remove_pinned_bundle_exact(&prefix, "@openai/codex", "codex", &receipt)
            .expect("authorized partial tree deletion resumes");
        assert!(!slot.exists());
        assert!(!quarantine.exists());
    }

    #[test]
    fn invalid_or_mismatched_receipt_shape_fails_closed() {
        let root = TestDir::new("bad-receipt");
        let prefix = root.0.join("prefix");
        let (package, _, mut receipt) = create_test_codex_bundle(&prefix);
        receipt
            .launcher_sha256
            .insert("bin/user-tool".into(), "c".repeat(64));
        assert!(remove_pinned_bundle_exact(&prefix, "@openai/codex", "codex", &receipt).is_err());
        assert!(package.exists());
    }

    #[test]
    #[ignore = "explicit live registry smoke test; ordinary tests remain fully offline"]
    fn live_feishu_bundle_smoke_test() {
        fn regular_file_sha256(path: &Path) -> String {
            let mut file = File::open(path).expect("open pinned support file");
            let mut hasher = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer).expect("read pinned support file");
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            hex::encode(hasher.finalize())
        }

        let root = TestDir::new("live-feishu");
        let prefix = root.0.join("prefix");
        std::fs::create_dir(&prefix).expect("create prefix with reservation");
        let marker = prefix.join(".codecli-feishu-reservation");
        std::fs::write(&marker, b"reservation\n").expect("write reservation");
        let installed = install_feishu_bundle(&prefix).expect("install pinned Feishu bundle");
        assert_eq!(
            std::fs::read(&marker).expect("reservation marker remains"),
            b"reservation\n"
        );
        assert!(installed.package_dir.join("package.json").is_file());
        assert!(installed.bin_script.is_file());
        assert!(installed
            .install_script
            .as_ref()
            .is_some_and(|path| path.is_file()));
        assert_eq!(
            regular_file_sha256(&installed.package_dir.join("scripts/install.js")),
            "c057a117af60f1bf908507ee799dd2d17acc582f315153e996de1bfedd7618de"
        );
        assert_eq!(
            regular_file_sha256(&installed.package_dir.join("scripts/run.js")),
            "b6b575a31d62ea45f55155f1090a49d31e79a1b0e5c70af15f9431ab850ca577"
        );
        assert_eq!(
            regular_file_sha256(&installed.package_dir.join("checksums.txt")),
            "106ac4329692a2d339145d4e08d905f50310733c02ef2783f29dfdc690c13ea7"
        );
        for dependency in FEISHU_CLOSURE {
            assert!(nested_package_dir(&installed.package_dir, dependency.name)
                .expect("dependency path")
                .join("package.json")
                .is_file());
        }
    }
}
