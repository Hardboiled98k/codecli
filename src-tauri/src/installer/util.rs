// SPDX-License-Identifier: MPL-2.0
//! 公共校验 / 脱敏 / 原子写 / 异步包装

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static ATOMIC_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 拒绝会破坏 shell / 注册表 / TOML 的字符
pub fn validate_env_value(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{} 不能为空", label));
    }
    if value.contains('\0') || value.contains('\n') || value.contains('\r') {
        return Err(format!("{} 含非法换行/空字符", label));
    }
    if value.len() > 8_192 {
        return Err(format!("{} 过长", label));
    }
    Ok(())
}

/// API Key / Token：更严，防 source 注入 + 过短泄漏
/// 允许：字母数字、_ - . + / =
pub fn validate_secret_value(label: &str, value: &str) -> Result<(), String> {
    validate_env_value(label, value)?;
    let n = value.chars().count();
    if n < 8 {
        return Err(format!("{} 太短（至少 8 个字符）", label));
    }
    // 拒绝 shell 元字符与空白（即使会加单引号，也杜绝误粘贴命令）
    let bad = |c: char| {
        matches!(
            c,
            '`' | '$'
                | ';'
                | '&'
                | '|'
                | '<'
                | '>'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '!'
                | '#'
                | '*'
                | '?'
                | '~'
                | '\\'
                | '"'
                | '\''
                | ' '
                | '\t'
        )
    };
    if value.chars().any(bad) {
        return Err(format!(
            "{} 含非法字符（请只粘贴纯 Key，不要带空格/引号/命令）",
            label
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | '='))
    {
        return Err(format!("{} 含不支持的字符", label));
    }
    Ok(())
}

/// 写入 secrets.env 的一行：KEY='shell-safe-value'
pub fn format_secret_line(key: &str, value: &str) -> Result<String, String> {
    validate_env_key(key)?;
    validate_secret_value(key, value)?;
    Ok(format!("{}={}", key, shell_single_quote(value)))
}

/// 解析 secrets.env 一行（支持 KEY=raw 旧格式与 KEY='quoted'）
pub fn parse_secret_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    let key = k.trim().to_string();
    let raw = v.trim();
    let val = if raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2 {
        // 解 shell 单引号：'a'\''b' → a'b
        let inner = &raw[1..raw.len() - 1];
        inner.replace("'\\''", "'")
    } else if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
        raw[1..raw.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        raw.to_string()
    };
    Some((key, val))
}

pub fn validate_env_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > 128 {
        return Err("环境变量名非法".into());
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("环境变量名非法: {}", key));
    }
    Ok(())
}

/// sh 单引号安全包裹
pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// PowerShell 单引号字符串：内部单引号用两个单引号转义。
/// 用于把 Windows 路径/URL 作为字面量传入 `powershell -Command`。
pub fn powershell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 只允许 https；localhost 可 http。拒绝 query/fragment/userinfo。
pub fn validate_base_url(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/').to_string();
    if s.is_empty() {
        return Err("Base URL 不能为空".into());
    }
    let parsed = url::Url::parse(&s).map_err(|e| format!("Base URL 无效: {}", e))?;
    let host = parsed.host_str().unwrap_or("");
    // url crate 版本间 `host_str()` 对 IPv6 方括号的表现可能不同；
    // localhost 判定统一比较去括号后的 canonical host。
    let host_unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    let is_local = host_unbracketed == "localhost"
        || host_unbracketed == "127.0.0.1"
        || host_unbracketed == "::1";
    match parsed.scheme() {
        "https" => {}
        "http" if is_local => {}
        "http" => return Err("出于安全只允许 HTTPS（本机 localhost 除外）".into()),
        other => return Err(format!("不支持的协议: {}", other)),
    }
    if parsed.username() != "" || parsed.password().is_some() {
        return Err("Base URL 不要带用户名密码，Key 请单独填".into());
    }
    if parsed.query().is_some() {
        return Err("Base URL 不要带 query 参数".into());
    }
    if parsed.fragment().is_some() {
        return Err("Base URL 不要带 #fragment".into());
    }
    // 规范化：scheme + host + port + path（无末尾 /）
    // `host_str()` 对 IPv6 返回不带方括号的 `::1`；重新拼 URL 时必须恢复
    // RFC 3986 的 `[::1]` 形式，否则会生成无法再次解析的地址。
    let display_host = match parsed.host() {
        Some(url::Host::Ipv6(address)) => format!("[{address}]"),
        _ => host.to_string(),
    };
    let mut out = format!("{}://{}", parsed.scheme(), display_host);
    if let Some(port) = parsed.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    let path = parsed.path();
    if path != "/" && !path.is_empty() {
        out.push_str(path.trim_end_matches('/'));
    }
    Ok(out)
}

fn join_url_path(base: &str, segments: &[&str]) -> String {
    let mut u = match url::Url::parse(base) {
        Ok(u) => u,
        Err(_) => {
            // fallback
            let mut b = base.trim_end_matches('/').to_string();
            for s in segments {
                b.push('/');
                b.push_str(s.trim_matches('/'));
            }
            return b;
        }
    };
    {
        let mut segs: Vec<String> = u
            .path_segments()
            .map(|ps| {
                ps.filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        // 避免重复追加
        let need: Vec<&str> = segments.to_vec();
        if segs.len() >= need.len() {
            let tail: Vec<&str> = segs[segs.len() - need.len()..]
                .iter()
                .map(|s| s.as_str())
                .collect();
            if tail == need {
                return u.to_string().trim_end_matches('/').to_string();
            }
        }
        // 特殊：已有 v1 再加 messages
        if need == ["v1", "messages"] && segs.last().map(|s| s.as_str()) == Some("v1") {
            segs.push("messages".into());
        } else {
            for s in need {
                segs.push(s.to_string());
            }
        }
        u.set_path(&format!("/{}", segs.join("/")));
    }
    u.to_string()
}

pub fn anthropic_messages_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with("/v1/messages") {
        b.to_string()
    } else if b.ends_with("/v1") {
        format!("{}/messages", b)
    } else {
        join_url_path(b, &["v1", "messages"])
    }
}

pub fn openai_responses_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with("/responses") {
        b.to_string()
    } else {
        join_url_path(b, &["responses"])
    }
}

/// Location 只保留 scheme/host/path，去掉 query（防 Key 泄漏）
pub fn sanitize_location(raw: &str) -> String {
    let scrubbed = mask_secrets_with(raw, &[]);
    if let Ok(u) = url::Url::parse(&scrubbed) {
        let host = u.host_str().unwrap_or("?");
        let mut s = format!("{}://{}{}", u.scheme(), host, u.path());
        if s.len() > 120 {
            s = s.chars().take(120).collect();
        }
        return s;
    }
    scrubbed.chars().take(80).collect()
}

fn prefix_suffix_chars(s: &str, n: usize) -> (String, String) {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n * 2 {
        return ("****".into(), String::new());
    }
    let pre: String = chars.iter().take(n).collect();
    let suf: String = chars
        .iter()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    (pre, suf)
}

pub fn mask_key(key: &str) -> String {
    let k = key.trim();
    let n = k.chars().count();
    if n == 0 {
        return "****".into();
    }
    if n <= 8 {
        return "****".into();
    }
    // 只显示后四，避免前缀泄漏
    let (_, suf) = prefix_suffix_chars(k, 4);
    format!("…{}", suf)
}

/// 精确替换已知 secret（任意非空）+ 常见 sk- 形态
pub fn mask_secrets_with(text: &str, secrets: &[&str]) -> String {
    let mut s = text.to_string();
    // 长 secret 优先，避免短串误伤
    let mut list: Vec<&str> = secrets
        .iter()
        .map(|x| x.trim())
        .filter(|t| !t.is_empty())
        .collect();
    list.sort_by_key(|a| std::cmp::Reverse(a.chars().count()));
    for t in list {
        if s.contains(t) {
            s = s.replace(t, &mask_key(t));
        }
    }
    let s = mask_sk_patterns(s);
    scrub_env_assignments(s)
}

/// 子进程启动前剥离可能含 Key 的环境变量
fn child_env_is_malformed_or_secret(name: &std::ffi::OsStr, value: &std::ffi::OsStr) -> bool {
    // Unix 环境变量允许非 UTF-8 字节。std::env::vars() 会在遇到这类值时
    // panic；子进程也可能因继承它们而崩溃。对无法安全检查的项目直接不传递。
    let (Some(name), Some(_value)) = (name.to_str(), value.to_str()) else {
        return true;
    };
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "ANTHROPIC_API_KEY"
            | "ANTHROPIC_AUTH_TOKEN"
            | "OPENAI_API_KEY"
            | "OPENAI_AUTH_TOKEN"
            | "API_KEY"
            | "ANTHROPIC_BASE_URL"
            | "OPENAI_BASE_URL"
    ) || (upper.starts_with("SCHEME_") && upper.ends_with("_KEY"))
        || ((upper.contains("API_KEY")
            || upper.contains("AUTH_TOKEN")
            || upper.ends_with("_SECRET"))
            && (upper.starts_with("ANTHROPIC")
                || upper.starts_with("OPENAI")
                || upper.starts_with("CODECLI")))
}

pub fn strip_secret_env_from_command(cmd: &mut std::process::Command) {
    const KEYS: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "OPENAI_API_KEY",
        "OPENAI_AUTH_TOKEN",
        "API_KEY",
        "ANTHROPIC_BASE_URL", // 不敏感但一并避免误带
        "OPENAI_BASE_URL",
    ];
    for k in KEYS {
        cmd.env_remove(k);
    }
    // 动态 SCHEME_*_KEY，并丢弃任何非 UTF-8 名称/值，避免本进程或子进程崩溃。
    for (k, value) in std::env::vars_os() {
        if child_env_is_malformed_or_secret(&k, &value) {
            cmd.env_remove(&k);
        }
    }
}

/// KEY=value / Bearer / Authorization 赋值脱敏
fn scrub_env_assignments(s: String) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let lower = line.to_lowercase();
        let is_secret_line = lower.contains("api_key")
            || lower.contains("apikey")
            || lower.contains("auth_token")
            || lower.contains("access_token")
            || lower.contains("secret")
            || lower.contains("password")
            || lower.contains("bearer ")
            || lower.contains("authorization:")
            || lower.contains("anthropic_api_key")
            || lower.contains("openai_api_key")
            || lower.contains("scheme_") && lower.contains("_key");
        if is_secret_line {
            if let Some(eq) = line.find('=') {
                let (k, _) = line.split_at(eq + 1);
                out.push(format!("{}[redacted]", k));
                continue;
            }
            if let Some(i) = lower.find("bearer ") {
                out.push(format!("{}[redacted]", &line[..i + 7]));
                continue;
            }
            out.push("[redacted-secret-line]".into());
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

fn mask_sk_patterns(s: String) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i] == 's' && chars[i + 1] == 'k' && chars[i + 2] == '-' {
            let start = i;
            let mut end = i + 3;
            while end < chars.len() {
                let c = chars[end];
                if c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '}' || c == '&' {
                    break;
                }
                end += 1;
            }
            let token: String = chars[start..end].iter().collect();
            if token.chars().count() > 8 {
                out.push_str(&mask_key(&token));
            } else {
                out.push_str(&token);
            }
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    atomic_write_mode(path, content, false)
}

#[cfg(windows)]
pub(crate) fn atomic_replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if from_wide[..from_wide.len() - 1].contains(&0) || to_wide[..to_wide.len() - 1].contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Windows 路径包含 NUL",
        ));
    }
    // SAFETY: 两个 buffer 在调用期间存活且以 NUL 结尾；路径不向 Windows API 外泄。
    let ok = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub(crate) fn atomic_replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)?;
    sync_parent_dir(to)
}

/// Unix rename/remove 只有在父目录也同步后才具备掉电持久性。
/// Windows 的替换路径使用 `MOVEFILE_WRITE_THROUGH`，无需再打开目录。
#[cfg(unix)]
pub(crate) fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径没有父目录"))?;
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn remove_file_durable(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent_dir(path),
        // unlink 已经成功、但上一次父目录 fsync 报错时，重试会看到
        // NotFound。此时仍必须再次同步父目录，不能把“不存在”误当成
        // 已持久提交，否则掉电后目录项可能重新出现。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(parent) = path.parent() else {
                return Ok(());
            };
            match std::fs::symlink_metadata(parent) {
                Ok(_) => sync_parent_dir(path),
                // 父目录本身也不存在时，没有目录项需要持久化。
                Err(parent_error) if parent_error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(parent_error) => Err(parent_error),
            }
        }
        Err(error) => Err(error),
    }
}

/// `strict_private=true`：Unix 必须成功设为 0600（secrets 用）
pub fn atomic_write_mode(path: &Path, content: &str, strict_private: bool) -> Result<(), String> {
    #[cfg(not(unix))]
    let _ = strict_private;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let parent = path.parent().ok_or("原子写路径没有父目录")?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("codecli-state");
    let mut selected = None;
    for _ in 0..32 {
        let sequence = ATOMIC_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.tmp.{}.{}", std::process::id(), sequence));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&candidate) {
            Ok(file) => {
                selected = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("创建私有临时文件失败: {error}")),
        }
    }
    let (tmp, mut file) = selected.ok_or("原子写临时文件连续冲突")?;
    let write_result = (|| -> Result<(), String> {
        file.write_all(content.as_bytes())
            .map_err(|error| format!("写临时文件失败: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
                if strict_private {
                    return Err(format!("无法设置 0600 权限: {error}"));
                }
            }
            let mode = file
                .metadata()
                .map_err(|error| format!("复查临时文件失败: {error}"))?
                .permissions()
                .mode()
                & 0o777;
            if mode != 0o600 && strict_private {
                return Err(format!("临时文件权限为 {mode:03o}，要求严格 600"));
            }
        }
        file.sync_all()
            .map_err(|error| format!("同步临时文件失败: {error}"))?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    atomic_replace_file(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("原子替换失败: {}", e)
    })?;
    #[cfg(unix)]
    if strict_private {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|error| format!("复查写入文件权限失败: {error}"))?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o600 {
            return Err(format!("写入后文件权限为 {mode:03o}，要求严格 600"));
        }
    }
    Ok(())
}

pub fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

pub async fn spawn_blocking_result<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("后台任务失败: {}", e))?
}

pub async fn spawn_blocking_ok<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("后台任务失败: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_https_ok_and_trim() {
        let u = validate_base_url(" https://api.deepseek.com/anthropic/ ").unwrap();
        assert_eq!(u, "https://api.deepseek.com/anthropic");
    }

    #[test]
    fn base_url_http_remote_rejected() {
        assert!(validate_base_url("http://evil.example/api").is_err());
    }

    #[test]
    fn base_url_query_rejected() {
        assert!(validate_base_url("https://host/v1?x=1").is_err());
    }

    #[test]
    fn base_url_localhost_http_ok() {
        assert!(validate_base_url("http://127.0.0.1:8080/v1").is_ok());
    }

    #[test]
    fn base_url_ipv6_loopback_keeps_brackets() {
        let normalized = validate_base_url("http://[::1]:8080/v1/").unwrap();
        assert_eq!(normalized, "http://[::1]:8080/v1");
        assert!(url::Url::parse(&normalized).is_ok());
    }

    #[test]
    fn anthropic_url_no_double_v1() {
        assert_eq!(
            anthropic_messages_url("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://x.com/v1"),
            "https://x.com/v1/messages"
        );
    }

    #[test]
    fn openai_url_no_double_responses() {
        assert_eq!(
            openai_responses_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            openai_responses_url("https://api.openai.com/v1/responses"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn mask_short_and_known_secret() {
        assert_eq!(mask_key("abcd"), "****");
        let s = mask_secrets_with("token=ab12 leaked", &["ab12"]);
        assert!(!s.contains("ab12"));
        let s2 = mask_secrets_with("x=ab y", &["ab"]);
        assert!(!s2.contains("=ab "));
    }

    #[test]
    fn child_env_filter_removes_secret_names_and_keeps_normal_unicode_values() {
        use std::ffi::OsStr;

        assert!(child_env_is_malformed_or_secret(
            OsStr::new("SCHEME_work_KEY"),
            OsStr::new("secret")
        ));
        assert!(child_env_is_malformed_or_secret(
            OsStr::new("codecli_operator_secret"),
            OsStr::new("secret")
        ));
        assert!(!child_env_is_malformed_or_secret(
            OsStr::new("PWD"),
            OsStr::new("/用户/阿海/项目")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn child_env_filter_rejects_non_utf8_name_or_value_without_panicking() {
        use std::ffi::{OsStr, OsString};
        use std::os::unix::ffi::OsStringExt;

        let malformed = OsString::from_vec(vec![0xff, b'x']);
        assert!(child_env_is_malformed_or_secret(
            &malformed,
            OsStr::new("value")
        ));
        assert!(child_env_is_malformed_or_secret(
            OsStr::new("_"),
            &malformed
        ));
    }

    #[test]
    fn secret_value_rejects_shell_meta() {
        assert!(validate_secret_value("k", "sk-abcdefgh").is_ok());
        assert!(validate_secret_value("k", "short").is_err());
        assert!(validate_secret_value("k", "sk-$(whoami)xxxx").is_err());
        assert!(validate_secret_value("k", "sk-abc;rm -rf").is_err());
        assert!(validate_secret_value("k", "sk abcdefgh").is_err());
    }

    #[test]
    fn secret_line_roundtrip_quoted() {
        let line = format_secret_line("ANTHROPIC_API_KEY", "sk-testkey123").unwrap();
        assert!(line.contains("ANTHROPIC_API_KEY='sk-testkey123'"));
        let (k, v) = parse_secret_line(&line).unwrap();
        assert_eq!(k, "ANTHROPIC_API_KEY");
        assert_eq!(v, "sk-testkey123");
    }

    #[test]
    fn sanitize_location_strips_query() {
        let s = sanitize_location("https://evil.com/cb?key=sk-abcdefghijklmnop");
        assert!(!s.contains("key="));
        assert!(!s.contains("sk-abcdefghijklmnop"));
        assert!(s.contains("evil.com"));
    }

    #[test]
    fn shell_single_quote_escapes() {
        assert_eq!(shell_single_quote("$(whoami)"), "'$(whoami)'");
    }

    #[test]
    fn powershell_single_quote_escapes_path_apostrophe() {
        assert_eq!(
            powershell_single_quote(r"C:\Users\O'Brien\node.zip"),
            r"'C:\Users\O''Brien\node.zip'"
        );
    }

    #[test]
    fn atomic_write_replaces_existing_file_privately() {
        let root = std::env::temp_dir().join(format!(
            "codecli-atomic-test-{}-{}",
            std::process::id(),
            ATOMIC_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).expect("create atomic test dir");
        let path = root.join("state.json");
        std::fs::write(&path, "old").expect("seed atomic target");
        atomic_write_mode(&path, "new", true).expect("durable replace");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(root).expect("cleanup atomic test dir");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_replaces_target_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "codecli-atomic-link-test-{}-{}",
            std::process::id(),
            ATOMIC_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).expect("create atomic link test dir");
        let outside = root.join("outside");
        let path = root.join("state.json");
        std::fs::write(&outside, "must-stay").expect("seed outside file");
        symlink(&outside, &path).expect("create target symlink");

        atomic_write_mode(&path, "replacement", true).expect("replace link atomically");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "must-stay");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
        assert!(!std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_dir_all(root).expect("cleanup atomic link test dir");
    }
}
