// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};
use std::io::Read;

use super::providers::find_provider;
use super::util::{
    anthropic_messages_url, mask_secrets_with, openai_responses_url, sanitize_location,
    validate_base_url,
};

const MAX_RESPONSE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectivityRequest {
    pub provider_id: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub protocol: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectivityResult {
    pub ok: bool,
    pub status_code: Option<u16>,
    pub message: String,
    pub detail: String,
}

#[tauri::command]
pub async fn test_connectivity(req: ConnectivityRequest) -> Result<ConnectivityResult, String> {
    super::util::spawn_blocking_result(move || test_connectivity_sync(req)).await
}

pub(crate) fn test_connectivity_sync(
    req: ConnectivityRequest,
) -> Result<ConnectivityResult, String> {
    let key = req.api_key.trim().to_string();
    if key.is_empty() {
        return Ok(ConnectivityResult {
            ok: false,
            status_code: None,
            message: "API Key 为空".into(),
            detail: String::new(),
        });
    }

    let provider = find_provider(&req.provider_id);
    let base_raw = req
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| provider.as_ref().map(|p| p.base_url.clone()))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Base URL 为空".to_string())?;

    let base = validate_base_url(&base_raw)?;

    let protocol = req
        .protocol
        .clone()
        .or_else(|| provider.as_ref().map(|p| p.protocol.clone()))
        .unwrap_or_else(|| "anthropic".into());
    if protocol != "anthropic" && protocol != "openai" {
        return Err(format!("不支持的 API 协议: {protocol}"));
    }

    let model = req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| provider.as_ref().and_then(|p| p.default_model.clone()))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "模型名不能为空；自定义服务商必须填写实际模型".to_string())?;

    if protocol == "openai" {
        test_openai_compatible(&base, &key, &model)
    } else {
        test_anthropic_compatible(&base, &key, &model)
    }
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    // 禁止自动重定向：避免 30x 把 x-api-key / Authorization 带到其它域
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))
}

fn test_anthropic_compatible(
    base: &str,
    key: &str,
    model: &str,
) -> Result<ConnectivityResult, String> {
    let url = anthropic_messages_url(base);
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "ping"}]
    });
    let client = http_client()?;
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", key)
        .header("Authorization", format!("Bearer {}", key))
        .json(&body)
        .send();
    map_response(resp, key, "Anthropic 兼容", "anthropic")
}

fn test_openai_compatible(
    base: &str,
    key: &str,
    model: &str,
) -> Result<ConnectivityResult, String> {
    // Codex custom providers use the Responses API.  Probing Chat
    // Completions here would create a false positive: the saved Codex config
    // can only use `wire_api = "responses"`.
    let url = openai_responses_url(base);
    let body = serde_json::json!({
        "model": model,
        "max_output_tokens": 16,
        "input": "ping"
    });
    let client = http_client()?;
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("Authorization", format!("Bearer {}", key))
        .json(&body)
        .send();
    map_response(resp, key, "OpenAI 兼容", "openai")
}

fn map_response(
    resp: Result<reqwest::blocking::Response, reqwest::Error>,
    key: &str,
    label: &str,
    protocol: &str,
) -> Result<ConnectivityResult, String> {
    match resp {
        Ok(mut r) => {
            let status = r.status().as_u16();
            // 3xx：明确提示，不跟随（防 Key 泄漏）
            if (300..400).contains(&status) {
                let loc = r
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("(no location)");
                let safe = sanitize_location(&mask_secrets_with(loc, &[key]));
                return Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: format!(
                        "服务返回重定向 HTTP {}，已拒绝跟随（防 Key 泄漏）。请改用最终 HTTPS Base URL",
                        status
                    ),
                    detail: format!("location={}", safe),
                });
            }
            if r.content_length()
                .is_some_and(|size| size > MAX_RESPONSE_BYTES)
            {
                return Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: format!(
                        "HTTP {} 响应体超过 {} KiB，已停止读取；请检查 Base URL 是否指向网页/WAF",
                        status,
                        MAX_RESPONSE_BYTES / 1024
                    ),
                    detail: "response body too large".into(),
                });
            }
            let mut bytes = Vec::new();
            if let Err(error) = (&mut r)
                .take(MAX_RESPONSE_BYTES + 1)
                .read_to_end(&mut bytes)
            {
                return Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: format!("HTTP {} 响应读取失败", status),
                    detail: mask_secrets_with(&error.to_string(), &[key]),
                });
            }
            if bytes.len() as u64 > MAX_RESPONSE_BYTES {
                return Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: format!(
                        "HTTP {} 响应体超过 {} KiB，已停止读取；请检查 Base URL 是否指向网页/WAF",
                        status,
                        MAX_RESPONSE_BYTES / 1024
                    ),
                    detail: "response body too large".into(),
                });
            }
            let body = String::from_utf8_lossy(&bytes).into_owned();
            let detail = summarize_body(&body, key);
            match status {
                200 | 201 => match validate_success_schema(&body, protocol) {
                    Ok(()) => Ok(ConnectivityResult {
                        ok: true,
                        status_code: Some(status),
                        message: format!("{} 连通成功（响应结构已验证）", label),
                        detail,
                    }),
                    Err(reason) => Ok(ConnectivityResult {
                        ok: false,
                        status_code: Some(status),
                        message: format!(
                            "HTTP {} 但不是有效的 {} API 响应；请检查 Base URL 是否指向网页/WAF: {}",
                            status, label, reason
                        ),
                        detail,
                    }),
                },
                401 | 403 => Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: "鉴权失败：检查 API Key / 额度 / 是否选对服务商".into(),
                    detail,
                }),
                404 => Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: "接口 404：Base URL 或路径不对，可改自定义地址".into(),
                    detail,
                }),
                400 => Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: "请求参数错误：多半是模型名不支持，可改模型名再测".into(),
                    detail,
                }),
                429 => Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: "触发限流，稍后重试".into(),
                    detail,
                }),
                500..=599 => Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(status),
                    message: format!("服务端错误 HTTP {}", status),
                    detail,
                }),
                c => Ok(ConnectivityResult {
                    ok: false,
                    status_code: Some(c),
                    message: format!("HTTP {}，详见摘要", c),
                    detail,
                }),
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let masked = mask_secrets_with(&msg, &[key]);
            let human = if e.is_timeout() {
                "超时：网络不稳或需代理（官方 Anthropic/OpenAI 常见）".into()
            } else if e.is_connect() {
                "连接失败：检查网络 / 代理 / Base URL".into()
            } else if masked.to_lowercase().contains("dns")
                || masked.to_lowercase().contains("resolve")
                || masked.to_lowercase().contains("name")
            {
                "DNS 失败：检查网络或代理".into()
            } else {
                format!("请求失败: {}", masked.chars().take(120).collect::<String>())
            };
            Ok(ConnectivityResult {
                ok: false,
                status_code: None,
                message: human,
                detail: masked.chars().take(300).collect(),
            })
        }
    }
}

fn validate_success_schema(body: &str, protocol: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "响应不是 JSON".to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "JSON 根节点不是对象".to_string())?;
    if object.get("error").is_some() {
        return Err("响应包含 error".into());
    }
    match protocol {
        "anthropic" => {
            let content_ok = object.get("content").is_some_and(|v| v.is_array());
            let identity_ok = object.get("id").is_some_and(|v| v.is_string())
                || object.get("type").and_then(|v| v.as_str()) == Some("message");
            if content_ok && identity_ok {
                Ok(())
            } else {
                Err("缺少 Anthropic message id/type/content".into())
            }
        }
        "openai" => {
            let output_ok = object.get("output").and_then(|v| v.as_array()).is_some();
            let identity_ok = object.get("id").is_some_and(|v| v.is_string())
                && object.get("object").and_then(|v| v.as_str()) == Some("response");
            if output_ok && identity_ok {
                Ok(())
            } else {
                Err("缺少 OpenAI response id/object/output".into())
            }
        }
        _ => Err("未知协议".into()),
    }
}

fn summarize_body(body: &str, key: &str) -> String {
    let masked = mask_secrets_with(body, &[key]);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&masked) {
        if let Some(m) = v
            .pointer("/error/message")
            .or_else(|| v.get("message"))
            .and_then(|x| x.as_str())
        {
            return m.chars().take(200).collect();
        }
        if let Some(t) = v.get("type").and_then(|x| x.as_str()) {
            return format!("type={}", t);
        }
    }
    if masked.trim().is_empty() {
        "(empty body)".into()
    } else {
        masked.chars().take(180).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::validate_success_schema;

    #[test]
    fn accepts_expected_protocol_shapes() {
        assert!(validate_success_schema(
            r#"{"id":"msg_1","type":"message","content":[]}"#,
            "anthropic"
        )
        .is_ok());
        assert!(validate_success_schema(
            r#"{"id":"resp_1","object":"response","output":[]}"#,
            "openai"
        )
        .is_ok());
    }

    #[test]
    fn rejects_html_empty_and_wrong_protocol_shapes() {
        assert!(validate_success_schema("<html>login</html>", "openai").is_err());
        assert!(validate_success_schema("", "anthropic").is_err());
        assert!(validate_success_schema(r#"{"ok":true}"#, "openai").is_err());
        assert!(validate_success_schema(r#"{"error":{"message":"bad"}}"#, "anthropic").is_err());
    }
}
