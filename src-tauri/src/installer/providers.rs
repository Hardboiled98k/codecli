// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub group: String, // domestic | official | custom
    /// anthropic | openai
    pub protocol: String,
    pub base_url: String,
    pub key_url: Option<String>,
    pub default_model: Option<String>,
    pub note: Option<String>,
}

/// 预置 Provider（2026-07 文档对齐；URL 变了可改此表或后续远程 JSON）
/// 参考:
/// - DeepSeek: https://api-docs.deepseek.com/quick_start/agent_integrations/claude_code/
/// - 通义: https://help.aliyun.com/zh/model-studio/anthropic-api-messages
/// - Kimi: https://platform.kimi.com/docs/guide/claude-code-kimi
/// - 智谱: https://docs.bigmodel.cn/cn/guide/develop/claude/introduction
/// - MiniMax: https://platform.minimax.io/docs/api-reference/text-anthropic-api
pub fn catalog() -> Vec<Provider> {
    vec![
        // —— Claude Code / Anthropic 兼容 ——
        Provider {
            id: "deepseek-anthropic".into(),
            name: "DeepSeek".into(),
            group: "domestic".into(),
            protocol: "anthropic".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            key_url: Some("https://platform.deepseek.com/api_keys".into()),
            default_model: Some("deepseek-v4-pro".into()),
            note: Some("推荐 · 国内直连 · Anthropic 兼容".into()),
        },
        Provider {
            id: "qwen-anthropic".into(),
            name: "通义千问".into(),
            group: "domestic".into(),
            protocol: "anthropic".into(),
            base_url: "https://dashscope.aliyuncs.com/apps/anthropic".into(),
            key_url: Some("https://bailian.console.aliyun.com/?apiKey=1".into()),
            default_model: Some("qwen3.7-plus".into()),
            note: Some("DashScope · Coding 套餐可用 coding.dashscope.aliyuncs.com".into()),
        },
        Provider {
            id: "kimi-anthropic".into(),
            name: "Kimi".into(),
            group: "domestic".into(),
            protocol: "anthropic".into(),
            // 国内用户优先 .cn；国际站为 api.moonshot.ai/anthropic
            base_url: "https://api.moonshot.cn/anthropic".into(),
            key_url: Some("https://platform.moonshot.cn/console/api-keys".into()),
            default_model: Some("kimi-k2.7-code".into()),
            note: Some("国内 .cn；国际可改 api.moonshot.ai/anthropic".into()),
        },
        Provider {
            id: "zhipu-anthropic".into(),
            name: "智谱 GLM".into(),
            group: "domestic".into(),
            protocol: "anthropic".into(),
            base_url: "https://open.bigmodel.cn/api/anthropic".into(),
            key_url: Some("https://open.bigmodel.cn/usercenter/apikeys".into()),
            default_model: Some("glm-5.2".into()),
            note: Some("官方 Claude 兼容端点".into()),
        },
        Provider {
            id: "minimax-anthropic".into(),
            name: "MiniMax".into(),
            group: "domestic".into(),
            protocol: "anthropic".into(),
            base_url: "https://api.minimaxi.com/anthropic".into(),
            key_url: Some(
                "https://platform.minimaxi.com/user-center/basic-information/interface-key".into(),
            ),
            default_model: Some("MiniMax-M3".into()),
            note: Some("国内 minimaxi.com；国际可改 api.minimax.io/anthropic".into()),
        },
        Provider {
            id: "anthropic-official".into(),
            name: "Anthropic 官方".into(),
            group: "official".into(),
            protocol: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            key_url: Some("https://console.anthropic.com/settings/keys".into()),
            default_model: Some("claude-sonnet-5".into()),
            note: Some("国内通常需代理 / 官方订阅".into()),
        },
        // —— Codex / OpenAI 兼容 ——
        Provider {
            id: "qwen-openai".into(),
            name: "通义 (Codex)".into(),
            group: "domestic".into(),
            protocol: "openai".into(),
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
            key_url: Some("https://bailian.console.aliyun.com/?apiKey=1".into()),
            default_model: Some("qwen3.7-plus".into()),
            note: Some(
                "DashScope 按量付费 Responses API；Coding Plan 不适用当前 Codex 版本".into(),
            ),
        },
        Provider {
            id: "openai-official".into(),
            name: "OpenAI 官方".into(),
            group: "official".into(),
            protocol: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            key_url: Some("https://platform.openai.com/api-keys".into()),
            default_model: Some("gpt-5.6".into()),
            note: Some("国内通常需代理".into()),
        },
        Provider {
            id: "custom-anthropic".into(),
            name: "其他 / 自建 (Claude)".into(),
            group: "custom".into(),
            protocol: "anthropic".into(),
            base_url: "".into(),
            key_url: None,
            default_model: None,
            note: Some("手动填 Base URL".into()),
        },
        Provider {
            id: "custom-openai".into(),
            name: "其他 / 自建 (Codex)".into(),
            group: "custom".into(),
            protocol: "openai".into(),
            base_url: "".into(),
            key_url: None,
            default_model: None,
            note: Some("仅适用于真实支持 POST /responses 的服务".into()),
        },
    ]
}

#[tauri::command]
pub fn list_providers() -> Vec<Provider> {
    catalog()
}

pub fn find_provider(id: &str) -> Option<Provider> {
    catalog().into_iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewed_provider_defaults_match_current_protocol_catalog() {
        let expected = [
            ("deepseek-anthropic", "anthropic", "deepseek-v4-pro"),
            ("qwen-anthropic", "anthropic", "qwen3.7-plus"),
            ("kimi-anthropic", "anthropic", "kimi-k2.7-code"),
            ("zhipu-anthropic", "anthropic", "glm-5.2"),
            ("minimax-anthropic", "anthropic", "MiniMax-M3"),
            ("anthropic-official", "anthropic", "claude-sonnet-5"),
            ("qwen-openai", "openai", "qwen3.7-plus"),
            ("openai-official", "openai", "gpt-5.6"),
        ];
        let providers = catalog();
        assert_eq!(
            providers.len(),
            expected.len() + 2,
            "unexpected preset added/removed"
        );
        let unique_ids = providers
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique_ids.len(), providers.len(), "duplicate provider id");
        for (id, protocol, model) in expected {
            let provider = providers
                .iter()
                .find(|provider| provider.id == id)
                .unwrap_or_else(|| panic!("missing reviewed provider {id}"));
            assert_eq!(provider.protocol, protocol, "protocol drift for {id}");
            assert_eq!(
                provider.default_model.as_deref(),
                Some(model),
                "default model drift for {id}"
            );
        }

        for id in ["custom-anthropic", "custom-openai"] {
            let provider = providers
                .iter()
                .find(|provider| provider.id == id)
                .unwrap_or_else(|| panic!("missing custom provider {id}"));
            assert_eq!(
                provider.default_model, None,
                "custom model must stay user-supplied"
            );
        }
    }
}
