// SPDX-License-Identifier: MPL-2.0
//! Read-only OpenAI usage. Subscription schema and reset inventory:
//! https://github.com/steipete/CodexBar/blob/main/docs/codex.md
//! https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Codex/CodexOAuth/CodexOAuthUsageFetcher.swift
//! https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/CreditsModels.swift
//! API-key verification uses the documented, non-inference model listing:
//! https://developers.openai.com/api/reference/resources/models/methods/list
//! API billing is separate from ChatGPT subscriptions:
//! https://help.openai.com/en/articles/9039756-managing-billing-for-chatgpt-and-the-api-platform

use crate::model::{BankedReset, Credential, CredentialKind, ProviderError, QuotaWindow, Snapshot};
use reqwest::{Client, RequestBuilder};
use serde_json::Value;
use std::time::Duration;

use super::{json, number, remaining, timestamp};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const RESETS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const MODELS_URL: &str = "https://api.openai.com/v1/models";
const MAX_RESETS: usize = 10_000;

pub async fn fetch(client: &Client, credential: &Credential) -> Result<Snapshot, ProviderError> {
    if credential.secret.trim().is_empty() {
        return Err(ProviderError(
            "OpenAI credential is empty. Reconnect.".into(),
        ));
    }
    if credential
        .expires_at
        .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp())
    {
        return Err(ProviderError("OpenAI sign-in expired. Reconnect, or renew the original CLI login and import it again.".into()));
    }
    match credential.kind {
        CredentialKind::ApiKey => {
            let response = json(client.get(MODELS_URL).bearer_auth(&credential.secret).header("Accept", "application/json")).await?;
            api_snapshot(&response)
        }
        CredentialKind::OAuth => {
            let usage = json(subscription_request(client, USAGE_URL, credential)).await?;
            check_account(&usage, credential.account_id.as_deref())?;
            let mut snapshot = usage_snapshot(&usage)?;
            let inventory = json(subscription_request(client, RESETS_URL, credential)
                .header("OpenAI-Beta", "codex-1")
                .header("originator", "Codex Desktop")
                .timeout(Duration::from_secs(4))).await;
            attach_inventory(&mut snapshot, inventory.as_ref().ok(), chrono::Utc::now().timestamp());
            Ok(snapshot)
        }
        CredentialKind::Cookie => Err(ProviderError("OpenAI subscription usage requires a Codex OAuth sign-in, not a browser cookie. Reconnect.".into())),
    }
}

fn subscription_request(
    client: &Client,
    url: &'static str,
    credential: &Credential,
) -> RequestBuilder {
    let mut request = client
        .get(url)
        .bearer_auth(&credential.secret)
        .header("Accept", "application/json")
        .header("Cache-Control", "no-cache");
    if let Some(account_id) = credential.account_id.as_deref().filter(|id| !id.is_empty()) {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    request
}

fn check_account(response: &Value, expected: Option<&str>) -> Result<(), ProviderError> {
    let reported = text(&response["account_id"]).or_else(|| text(&response["accountId"]));
    if let (Some(expected), Some(reported)) = (expected.filter(|id| !id.is_empty()), reported)
        && expected != reported
    {
        return Err(ProviderError("OpenAI returned usage for a different account or workspace. Reconnect with the intended account.".into()));
    }
    Ok(())
}

fn api_snapshot(response: &Value) -> Result<Snapshot, ProviderError> {
    if response["object"].as_str() != Some("list") || !response["data"].is_array() {
        return Err(ProviderError(
            "OpenAI returned an unrecognized model-list response.".into(),
        ));
    }
    // Neither model availability nor a successful HTTP request establishes a credit balance.
    Ok(Snapshot {
        details: vec![
            ("Connection".into(), "API key verified with a read-only model listing".into()),
            ("API balance".into(), "Unavailable with the documented API-key endpoints".into()),
        ],
        notes: vec![
            "API billing is separate from ChatGPT subscription quotas. Use a Codex OAuth sign-in to view subscription usage.".into(),
            "No supported API-key balance endpoint was confirmed. Check the API billing dashboard for prepaid credits. No inference request is made.".into(),
        ],
        ..Snapshot::default()
    })
}

fn usage_snapshot(response: &Value) -> Result<Snapshot, ProviderError> {
    let object = response
        .as_object()
        .ok_or_else(|| ProviderError("OpenAI returned an unrecognized usage response.".into()))?;
    if ![
        "plan_type",
        "rate_limit",
        "credits",
        "additional_rate_limits",
        "individual_limit",
        "spend_control",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
    {
        return Err(ProviderError(
            "OpenAI returned no recognized subscription usage fields.".into(),
        ));
    }
    let mut snapshot = Snapshot {
        // A login-time JWT plan can be stale after a subscription change; only use this response.
        plan: text(&response["plan_type"]).map(str::to_owned),
        credits: credit_balance(&response["credits"]),
        ..Snapshot::default()
    };
    append_windows(&mut snapshot.windows, &response["rate_limit"], None);
    if let Some(additional) = response["additional_rate_limits"].as_array() {
        for limit in additional {
            let name = text(&limit["limit_name"])
                .or_else(|| text(&limit["metered_feature"]))
                .unwrap_or("Additional quota");
            append_windows(&mut snapshot.windows, &limit["rate_limit"], Some(name));
        }
    }
    if snapshot.windows.is_empty() {
        snapshot.notes.push(
            "Subscription quota windows were not reported; remaining quota is unavailable.".into(),
        );
    }
    if snapshot.plan.is_none() {
        snapshot
            .notes
            .push("Current subscription plan was not reported.".into());
    }
    if snapshot.credits.is_none() {
        snapshot
            .notes
            .push("Codex credit balance was not reported.".into());
    }
    Ok(snapshot)
}

fn text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn positive_timestamp(value: &Value) -> Option<i64> {
    timestamp(value)
        .filter(|time| *time > 0 && chrono::DateTime::from_timestamp(*time, 0).is_some())
}

fn append_windows(windows: &mut Vec<QuotaWindow>, rate_limit: &Value, prefix: Option<&str>) {
    for (key, fallback) in [
        ("primary_window", "Primary quota (duration unavailable)"),
        ("secondary_window", "Secondary quota (duration unavailable)"),
    ] {
        let window = &rate_limit[key];
        if !window.is_object() {
            continue;
        }
        // Window positions vary by plan. The reported duration, never the plan name, defines its lane.
        let duration = window["limit_window_seconds"]
            .as_i64()
            .filter(|seconds| *seconds > 0);
        let label = match duration {
            Some(18_000) => "5-hour".into(),
            Some(604_800) => "Weekly".into(),
            Some(seconds) if seconds % 86_400 == 0 => format!("{}-day", seconds / 86_400),
            Some(seconds) if seconds % 3_600 == 0 => format!("{}-hour", seconds / 3_600),
            Some(seconds) if seconds % 60 == 0 => format!("{}-minute", seconds / 60),
            Some(seconds) => format!("{seconds}-second"),
            None => fallback.into(),
        };
        windows.push(QuotaWindow {
            label: match prefix {
                Some(prefix) => format!("{prefix} · {label}"),
                None => label,
            },
            remaining_percent: remaining(&window["used_percent"]),
            resets_at: positive_timestamp(&window["reset_at"]),
        });
    }
}

fn credit_balance(credits: &Value) -> Option<String> {
    if credits["unlimited"].as_bool() == Some(true) {
        return Some("Unlimited Codex credits".into());
    }
    if let Some(balance) = number(&credits["balance"]).filter(|balance| *balance >= 0.0) {
        return Some(format!("{balance} Codex credits"));
    }
    // has_credits alone is not a numeric balance, including when false.
    if credits["has_credits"].as_bool() == Some(true) {
        return Some("Codex credits available; balance not reported".into());
    }
    None
}

fn attach_inventory(snapshot: &mut Snapshot, response: Option<&Value>, now: i64) {
    match response.and_then(|response| reset_inventory(response, now)) {
        Some((resets, incomplete)) => {
            snapshot.resets = Some(resets);
            if incomplete {
                snapshot.notes.push(
                    "Some banked reset expiry details were not reported or could not be read."
                        .into(),
                );
            }
        }
        None => {
            snapshot.resets = None;
            snapshot.notes.push(
                "Banked reset inventory is unavailable; subscription quota is still shown.".into(),
            );
        }
    }
}

fn reset_inventory(response: &Value, now: i64) -> Option<(Vec<BankedReset>, bool)> {
    let count = match response.get("available_count") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            usize::try_from(value.as_u64()?)
                .ok()
                .filter(|count| *count <= MAX_RESETS)?,
        ),
    };
    let entries = response["credits"].as_array();
    if count.is_none() && entries.is_none() {
        return None;
    }
    if entries.is_some_and(|entries| entries.len() > MAX_RESETS) {
        return None;
    }
    let mut resets = Vec::new();
    let mut reported_available = 0;
    let mut incomplete = entries.is_none() || count.is_none();
    if let Some(entries) = entries {
        for entry in entries {
            let Some(status) = entry["status"].as_str() else {
                incomplete = true;
                continue;
            };
            if status != "available" {
                if !matches!(status, "redeeming" | "redeemed" | "expired") {
                    incomplete = true;
                }
                continue;
            }
            reported_available += 1;
            let expires_at = positive_timestamp(&entry["expires_at"]);
            // An expired entry must not reappear through available_count reconciliation below.
            if expires_at.is_some_and(|expiry| expiry <= now) {
                continue;
            }
            if expires_at.is_none() {
                incomplete = true;
            }
            resets.push(BankedReset {
                label: text(&entry["title"])
                    .unwrap_or("Banked Codex reset")
                    .to_owned(),
                expires_at,
            });
        }
    }
    if let Some(count) = count {
        for _ in reported_available..count {
            resets.push(BankedReset {
                label: "Banked Codex reset".into(),
                expires_at: None,
            });
            incomplete = true;
        }
    } else if incomplete && resets.is_empty() && entries.is_some_and(|entries| !entries.is_empty())
    {
        // A malformed list without a confirmed count cannot establish a zero balance.
        return None;
    }
    resets.sort_by_key(|reset| reset.expires_at.unwrap_or(i64::MAX));
    Some((resets, incomplete))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn free_weekly_primary_does_not_create_a_five_hour_window() {
        let snapshot = usage_snapshot(&json!({
            "plan_type": "free", "rate_limit": {
                "primary_window": {"used_percent": 23, "limit_window_seconds": 604800, "reset_at": 1800000000},
                "secondary_window": null
            }
        })).unwrap();
        assert_eq!(
            snapshot.windows,
            vec![QuotaWindow {
                label: "Weekly".into(),
                remaining_percent: Some(77.0),
                resets_at: Some(1800000000),
            }]
        );
    }

    #[test]
    fn any_reported_plan_can_have_a_five_hour_secondary_window() {
        let snapshot = usage_snapshot(&json!({
            "plan_type": "new-plan", "rate_limit": {
                "primary_window": {"used_percent": 100, "limit_window_seconds": 604800},
                "secondary_window": {"used_percent": 0, "limit_window_seconds": 18000}
            }
        }))
        .unwrap();
        assert_eq!(
            snapshot
                .windows
                .iter()
                .map(|window| (window.label.as_str(), window.remaining_percent))
                .collect::<Vec<_>>(),
            vec![("Weekly", Some(0.0)), ("5-hour", Some(100.0))]
        );
    }

    #[test]
    fn arbitrary_or_missing_duration_is_not_mislabeled() {
        let snapshot = usage_snapshot(&json!({"rate_limit": {
            "primary_window": {"used_percent": 12, "limit_window_seconds": 7200},
            "secondary_window": {"used_percent": 25}
        }}))
        .unwrap();
        assert_eq!(
            snapshot
                .windows
                .iter()
                .map(|window| window.label.as_str())
                .collect::<Vec<_>>(),
            vec!["2-hour", "Secondary quota (duration unavailable)"]
        );
    }

    #[test]
    fn malformed_optional_fields_keep_valid_quota_and_unknown_values() {
        let snapshot = usage_snapshot(&json!({
            "rate_limit": {
                "primary_window": {"used_percent": 120, "limit_window_seconds": 18000},
                "secondary_window": {"used_percent": -1, "reset_at": "invalid", "limit_window_seconds": 604800}
            }, "credits": "invalid", "additional_rate_limits": [null, {"rate_limit": "invalid"}]
        })).unwrap();
        assert_eq!(
            (
                snapshot.windows[0].remaining_percent,
                snapshot.windows[1].remaining_percent,
                snapshot.windows[1].resets_at,
                snapshot.credits
            ),
            (Some(0.0), None, None, None)
        );
    }

    #[test]
    fn malformed_additional_entry_does_not_erase_its_valid_sibling() {
        let snapshot = usage_snapshot(&json!({"additional_rate_limits": [
            null, {"limit_name": "Model quota", "rate_limit": {"primary_window": {
                "used_percent": 10, "limit_window_seconds": 18000
            }}}
        ]}))
        .unwrap();
        assert_eq!(snapshot.windows[0].label, "Model quota · 5-hour");
    }

    #[test]
    fn numeric_credit_strings_and_unlimited_are_reported_without_currency_invention() {
        assert_eq!(
            credit_balance(&json!({"balance": "42.5", "has_credits": true})),
            Some("42.5 Codex credits".into())
        );
        assert_eq!(
            credit_balance(&json!({"unlimited": true})),
            Some("Unlimited Codex credits".into())
        );
        assert_eq!(credit_balance(&json!({"has_credits": false})), None);
        assert_eq!(credit_balance(&json!({"balance": "NaN"})), None);
    }

    #[test]
    fn inventory_filters_expired_and_consumed_and_sorts_each_expiry() {
        let (resets, _) = reset_inventory(&json!({"available_count": 4, "credits": [
            {"status": "available", "title": "Later", "expires_at": "2027-01-02T00:00:00Z"},
            {"status": "available", "title": "Expired", "expires_at": 100},
            {"status": "redeemed", "expires_at": "2027-01-01T00:00:00Z"},
            {"status": "redeeming", "expires_at": "2027-01-01T00:00:00Z"},
            {"status": "available", "title": "Earlier", "expires_at": "2027-01-01T00:00:00.123Z"},
            {"status": "available", "title": "Unknown expiry", "expires_at": null}
        ]}), 100).unwrap();
        assert_eq!(
            resets
                .iter()
                .map(|reset| (reset.label.as_str(), reset.expires_at))
                .collect::<Vec<_>>(),
            vec![
                ("Earlier", Some(1798761600)),
                ("Later", Some(1798848000)),
                ("Unknown expiry", None)
            ]
        );
    }

    #[test]
    fn count_only_inventory_preserves_each_reset_with_unknown_expiry() {
        let (resets, incomplete) = reset_inventory(&json!({"available_count": 3}), 100).unwrap();
        assert!(
            incomplete
                && resets.len() == 3
                && resets.iter().all(|reset| reset.expires_at.is_none())
        );
    }

    #[test]
    fn partial_inventory_keeps_known_expiry_and_fills_only_missing_entries() {
        let (resets, _) = reset_inventory(
            &json!({"available_count": 3, "credits": [
                {"status": "available", "expires_at": 200}, null
            ]}),
            100,
        )
        .unwrap();
        assert_eq!(
            resets
                .iter()
                .map(|reset| reset.expires_at)
                .collect::<Vec<_>>(),
            vec![Some(200), None, None]
        );
    }

    #[test]
    fn failed_inventory_does_not_erase_subscription_usage_or_credit_balance() {
        let mut snapshot = usage_snapshot(&json!({"plan_type": "pro", "rate_limit": {
            "primary_window": {"used_percent": 25, "limit_window_seconds": 18000}
        }, "credits": {"balance": 0}}))
        .unwrap();
        let quota = snapshot.windows.clone();
        attach_inventory(&mut snapshot, None, 100);
        assert_eq!(
            (
                snapshot.plan.as_deref(),
                snapshot.windows,
                snapshot.credits.as_deref(),
                snapshot.resets
            ),
            (Some("pro"), quota, Some("0 Codex credits"), None)
        );
    }

    #[test]
    fn unknown_inventory_never_becomes_confirmed_zero() {
        for response in [
            json!({}),
            json!({"credits": [null]}),
            json!({"available_count": -1}),
            json!({"available_count": 10001}),
            json!({"available_count": "bad"}),
        ] {
            assert!(reset_inventory(&response, 100).is_none());
        }
        assert_eq!(
            reset_inventory(&json!({"available_count": 0, "credits": []}), 100),
            Some((vec![], false))
        );
    }

    #[test]
    fn invalid_expiry_keeps_a_confirmed_reset_without_fabricating_a_date() {
        let (resets, incomplete) = reset_inventory(
            &json!({"available_count": 1, "credits": [
                {"status": "available", "expires_at": "bad-date"}
            ]}),
            100,
        )
        .unwrap();
        assert!(incomplete && resets.len() == 1 && resets[0].expires_at.is_none());
    }

    #[test]
    fn api_mode_does_not_manufacture_subscription_plan_quotas_or_balance() {
        let snapshot = api_snapshot(&json!({"object": "list", "data": []})).unwrap();
        assert!(
            snapshot.plan.is_none()
                && snapshot.windows.is_empty()
                && snapshot.resets.is_none()
                && snapshot.credits.is_none()
        );
    }

    #[test]
    fn account_mismatch_is_rejected_without_exposing_identifiers() {
        let error = check_account(
            &json!({"account_id": "private-account-b"}),
            Some("private-account-a"),
        )
        .unwrap_err();
        assert!(!error.0.contains("private-account"));
    }
}
