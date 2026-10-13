// SPDX-License-Identifier: MPL-2.0

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Claude,
    OpenAi,
    OpenCodeGo,
    OpenCodeZen,
    OpenRouter,
}

impl Provider {
    pub const ALL: [Self; 5] = [
        Self::Claude,
        Self::OpenAi,
        Self::OpenCodeGo,
        Self::OpenCodeZen,
        Self::OpenRouter,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::OpenAi => "OpenAI Quota",
            Self::OpenCodeGo => "OpenCode Go",
            Self::OpenCodeZen => "OpenCode Zen",
            Self::OpenRouter => "OpenRouter",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::OpenCodeGo => "opencode-go",
            Self::OpenCodeZen => "opencode-zen",
            Self::OpenRouter => "openrouter",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialKind {
    OAuth,
    ApiKey,
    Cookie,
}

// Deliberately no Debug: credentials must not appear in GUI message logs.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub kind: CredentialKind,
    pub secret: String,
    pub account_id: Option<String>,
    pub plan_hint: Option<String>,
    pub source: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    pub label: String,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BankedReset {
    pub label: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    // None means unavailable, Some(empty) means a confirmed zero balance.
    pub resets: Option<Vec<BankedReset>>,
    pub credits: Option<String>,
    pub details: Vec<(String, String)>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError(pub String);

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProviderError {}
