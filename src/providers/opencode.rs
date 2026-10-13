// SPDX-License-Identifier: MPL-2.0
//! Read-only OpenCode quota and billing integrations.
//!
//! Sources: https://github.com/steipete/CodexBar/blob/main/docs/opencode.md
//! https://github.com/anomalyco/opencode/blob/dev/services/www/src/docs/content/console/api/index.mdx
//! https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/lib/inference-proxy.ts
//! Legacy Zen keys can read models but do not grant Console billing access. Console
//! service keys with All permissions can read billing; inference-only keys cannot.
//! Explicit Console cookies support workspace discovery; browser stores are never read.

use super::{json, number, remaining, timestamp};
use crate::model::{Credential, CredentialKind, ProviderError, QuotaWindow, Snapshot};
use reqwest::{
    Client, RequestBuilder,
    header::{COOKIE, HeaderValue},
};
use serde_json::Value;

const GO_USAGE: &str = "https://opencode.ai/zen/go/v1/usage";
const BILLING: &str = "https://opencode.ai/console/api/billing/status";
const ORGS: &str = "https://opencode.ai/console/api/orgs";
const CURRENT_ORG: &str = "https://opencode.ai/console/api/orgs/current";
const SUMMARY: &str = "https://opencode.ai/console/api/usage/summary?range=30d";

pub async fn fetch_go(client: &Client, credential: &Credential) -> Result<Snapshot, ProviderError> {
    if credential.kind != CredentialKind::ApiKey {
        return Err(ProviderError("OpenCode Go requires an API key.".into()));
    }
    let key = api_key(credential)?;
    let value = json(client.get(GO_USAGE).bearer_auth(key)).await?;
    parse_go(&value, chrono::Utc::now().timestamp())
}

pub async fn fetch_zen(
    client: &Client,
    credential: &Credential,
) -> Result<Snapshot, ProviderError> {
    match credential.kind {
        CredentialKind::Cookie => {
            let mut cookie = HeaderValue::from_str(credential.secret.trim())
                .map_err(|_| ProviderError("Invalid OpenCode Console Cookie header.".into()))?;
            if cookie.is_empty() {
                return Err(ProviderError(
                    "OpenCode Console Cookie header is empty.".into(),
                ));
            }
            cookie.set_sensitive(true);
            let workspace = if let Some(id) = credential.account_id.as_deref() {
                validate_workspace(id)?.to_owned()
            } else {
                let orgs = json(client.get(ORGS).header(COOKIE, cookie.clone())).await?;
                discover_workspace(&orgs)?.to_owned()
            };
            let billing = json(console_request(client, BILLING, &cookie, &workspace)).await?;
            let mut snapshot = parse_billing(&billing)?;
            // Rolling spend is not a quota, and must never be shown as calendar-month usage.
            if billing["mode"].as_str() == Some("pay-as-you-go") {
                match json(console_request(client, CURRENT_ORG, &cookie, &workspace)).await {
                    Ok(org) if org["hasGoSubscription"].as_bool() == Some(false) => {
                        match json(console_request(client, SUMMARY, &cookie, &workspace)).await {
                            Ok(summary) => match parse_spend(&summary) {
                                Ok(spend) => {
                                    snapshot.details.push(("Last 30 days spend".into(), spend))
                                }
                                Err(error) => snapshot.notes.push(error.0),
                            },
                            Err(error) => {
                                snapshot.notes.push(format!("Spend unavailable: {error}"))
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) => snapshot.notes.push(format!("Spend unavailable: {error}")),
                }
            }
            Ok(snapshot)
        }
        CredentialKind::ApiKey => fetch_zen_key(client, credential).await,
        CredentialKind::OAuth => Err(ProviderError(
            "OpenCode Zen requires an API key or explicit Console Cookie header.".into(),
        )),
    }
}

fn console_request(
    client: &Client,
    endpoint: &'static str,
    cookie: &HeaderValue,
    workspace: &str,
) -> RequestBuilder {
    client
        .get(endpoint)
        .header(COOKIE, cookie.clone())
        .header("x-org-id", workspace)
}

async fn fetch_zen_key(
    client: &Client,
    credential: &Credential,
) -> Result<Snapshot, ProviderError> {
    let key = api_key(credential)?;
    if key.starts_with("oc_sk_") {
        match json(client.get(BILLING).bearer_auth(key)).await {
            Ok(value) => return parse_billing(&value),
            // All permission is required for Console billing. Validate the connection
            // through the documented configuration endpoint before reporting unavailable.
            Err(error) if error.0 == "Provider denied access to this data (HTTP 403)." => {}
            Err(error) => return Err(error),
        }
        let config = json(
            client
                .get("https://opencode.ai/console/api/v2/config")
                .bearer_auth(key),
        )
        .await?;
        if !config["providers"].is_object() {
            return Err(ProviderError(
                "OpenCode returned invalid workspace configuration.".into(),
            ));
        }
        Ok(unavailable_balance(
            "API key connected. Console billing requires an All-permission service key or explicit Console Cookie header.",
        ))
    } else {
        // The public source checks legacy keys against migrated workspaces before
        // forwarding this metadata GET. No inference request or paid operation occurs.
        let models = json(
            client
                .get("https://opencode.ai/zen/v1/models")
                .bearer_auth(key),
        )
        .await?;
        if !models["data"].is_array() {
            return Err(ProviderError(
                "OpenCode returned an invalid model catalog.".into(),
            ));
        }
        Ok(unavailable_balance(
            "API key connected. Legacy Zen keys do not expose Console balance. Connect an explicit Console Cookie header for billing.",
        ))
    }
}

fn unavailable_balance(note: &str) -> Snapshot {
    Snapshot {
        notes: vec![note.into()],
        ..Snapshot::default()
    }
}

fn api_key(credential: &Credential) -> Result<&str, ProviderError> {
    let key = credential.secret.trim();
    if key.is_empty()
        || key == "public"
        || key
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(ProviderError(
            "OpenCode requires a nonempty private API key.".into(),
        ));
    }
    Ok(key)
}

fn validate_workspace(id: &str) -> Result<&str, ProviderError> {
    let suffix = id.strip_prefix("org_").or_else(|| id.strip_prefix("wrk_"));
    if suffix.is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }) {
        Ok(id)
    } else {
        Err(ProviderError(
            "Workspace must be an org_… or wrk_… ID.".into(),
        ))
    }
}

fn discover_workspace(value: &Value) -> Result<&str, ProviderError> {
    let rows = value
        .as_array()
        .ok_or_else(|| ProviderError("OpenCode returned an invalid workspace list.".into()))?;
    match rows.as_slice() {
        [] => Err(ProviderError("No OpenCode Console workspace is accessible.".into())),
        [org] => validate_workspace(org["id"].as_str().ok_or_else(|| ProviderError("OpenCode workspace ID is missing.".into()))?),
        _ => Err(ProviderError("Several OpenCode workspaces are accessible. Set the workspace ID before connecting; no workspace was selected.".into())),
    }
}

fn parse_go(value: &Value, now: i64) -> Result<Snapshot, ProviderError> {
    let usage = value["usage"]
        .as_object()
        .ok_or_else(|| ProviderError("OpenCode Go usage fields are missing.".into()))?;
    if !usage.get("rolling").is_some_and(Value::is_object) {
        return Err(ProviderError(
            "OpenCode Go rolling quota is missing.".into(),
        ));
    }
    let mut snapshot = Snapshot {
        plan: value["plan"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned),
        ..Snapshot::default()
    };
    for (key, label) in [
        ("rolling", "Rolling 5 hours"),
        ("weekly", "Weekly"),
        ("monthly", "Monthly"),
    ] {
        let Some(meter) = usage.get(key).filter(|meter| !meter.is_null()) else {
            continue;
        };
        if !meter.is_object() {
            return Err(ProviderError(format!(
                "OpenCode Go {label} quota is invalid."
            )));
        }
        let remaining_percent = remaining(&meter["percent"]);
        if remaining_percent.is_none() {
            return Err(ProviderError(format!(
                "OpenCode Go {label} percentage is missing or invalid."
            )));
        }
        let resets_at = timestamp(&meter["resetAt"])
            .or_else(|| timestamp(&meter["resetsAt"]))
            .or_else(|| {
                meter["resetInSec"]
                    .as_i64()
                    .filter(|seconds| *seconds >= 0)
                    .and_then(|seconds| now.checked_add(seconds))
            });
        snapshot.windows.push(QuotaWindow {
            label: label.into(),
            remaining_percent,
            resets_at,
        });
    }
    Ok(snapshot)
}

fn parse_billing(value: &Value) -> Result<Snapshot, ProviderError> {
    let billing_mode = value["billingMode"].as_str();
    let mode = value["mode"].as_str();
    if !matches!(billing_mode, Some("prepaid" | "legacy" | "seat" | "credit"))
        || !matches!(mode, Some("pay-as-you-go" | "invoiceable"))
    {
        return Err(ProviderError(
            "OpenCode returned invalid Console billing data.".into(),
        ));
    }
    let mut snapshot = Snapshot {
        plan: mode.map(|mode| {
            if mode == "pay-as-you-go" {
                "Pay as you go"
            } else {
                "Invoiced billing"
            }
            .into()
        }),
        ..Snapshot::default()
    };
    if billing_mode == Some("prepaid") && mode == Some("pay-as-you-go") {
        let balance = signed_microcents(&value["balanceMicroCents"]).ok_or_else(|| {
            ProviderError("OpenCode Console balance is missing or invalid.".into())
        })?;
        snapshot.credits = Some(format!("${:.2} USD", balance / 100_000_000.0));
    } else {
        snapshot
            .notes
            .push("Prepaid Zen balance is unavailable for this billing mode.".into());
    }
    // availableMicroCents is credit availability, not the prepaid balance.
    Ok(snapshot)
}

fn signed_microcents(value: &Value) -> Option<f64> {
    let raw = value.as_str()?;
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number(value)
}

fn parse_spend(value: &Value) -> Result<String, ProviderError> {
    let amount = &value["totalCostMicroCents"];
    let amount = amount
        .as_u64()
        .map(|amount| amount as f64)
        .or_else(|| signed_microcents(amount).filter(|amount| *amount >= 0.0))
        .ok_or_else(|| ProviderError("OpenCode 30-day spend is missing or invalid.".into()))?;
    Ok(format!("${:.2} USD", amount / 100_000_000.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn go_percent_one_is_one_percent_used() {
        let snapshot = parse_go(&json!({"usage": {"rolling": {"percent": 1}, "weekly": {"percent": 0.5}, "monthly": {"percent": 100}}}), 0).unwrap();
        assert_eq!(
            snapshot
                .windows
                .iter()
                .map(|window| window.remaining_percent)
                .collect::<Vec<_>>(),
            vec![Some(99.0), Some(99.5), Some(0.0)]
        );
    }

    #[test]
    fn go_keeps_reported_plan_and_reset_timestamps() {
        let snapshot = parse_go(&json!({"plan":"Go", "usage":{"rolling":{"percent":0,"resetAt":"2026-01-01T00:00:00Z"},"weekly":{"percent":25,"resetInSec":60}}}), 100).unwrap();
        assert_eq!(
            (
                snapshot.plan.as_deref(),
                snapshot.windows[0].resets_at,
                snapshot.windows[1].resets_at
            ),
            (Some("Go"), Some(1767225600), Some(160))
        );
    }

    #[test]
    fn go_missing_reset_stays_unknown() {
        assert_eq!(
            parse_go(&json!({"usage":{"rolling":{"percent":0}}}), 0)
                .unwrap()
                .windows[0]
                .resets_at,
            None
        );
    }

    #[test]
    fn go_rejects_missing_or_invalid_percentages() {
        for value in [
            json!({}),
            json!({"usage":{"rolling":{}}}),
            json!({"usage":{"rolling":{"percent":-1}}}),
            json!({"usage":{"rolling":{"percent":"NaN"}}}),
            json!({"usage":{"rolling":{"percent":0},"weekly":false}}),
        ] {
            assert!(parse_go(&value, 0).is_err());
        }
    }

    #[test]
    fn console_preserves_signed_and_zero_balances() {
        for (raw, expected) in [
            ("-125000000", "$-1.25 USD"),
            ("0", "$0.00 USD"),
            ("2375000000", "$23.75 USD"),
        ] {
            let snapshot = parse_billing(&json!({"billingMode":"prepaid", "mode":"pay-as-you-go", "balanceMicroCents":raw, "availableMicroCents":"9999999999"})).unwrap();
            assert_eq!(snapshot.credits.as_deref(), Some(expected));
        }
    }

    #[test]
    fn console_rejects_missing_and_invalid_balances() {
        for raw in [
            Value::Null,
            json!(0),
            json!(""),
            json!("-"),
            json!("1.5"),
            json!("+1"),
            json!("NaN"),
            json!(" 1"),
        ] {
            assert!(
                parse_billing(
                    &json!({"billingMode":"prepaid","mode":"pay-as-you-go","balanceMicroCents":raw})
                )
                .is_err()
            );
        }
    }

    #[test]
    fn unsupported_billing_mode_does_not_look_like_zero() {
        assert_eq!(
            parse_billing(
                &json!({"billingMode":"credit","mode":"invoiceable","balanceMicroCents":"0"})
            )
            .unwrap()
            .credits,
            None
        );
    }

    #[test]
    fn workspace_discovery_never_guesses_between_accounts() {
        assert!(discover_workspace(&json!([{"id":"org_first"},{"id":"org_second"}])).is_err());
    }

    #[test]
    fn workspace_discovery_accepts_single_valid_workspace() {
        assert_eq!(
            discover_workspace(&json!([{"id":"org_only"}])).unwrap(),
            "org_only"
        );
    }

    #[test]
    fn workspace_discovery_rejects_missing_invalid_or_empty_ids() {
        for value in [
            json!([]),
            json!({}),
            json!([{}]),
            json!([{"id":"org_"}]),
            json!([{"id":"https://example.com"}]),
        ] {
            assert!(discover_workspace(&value).is_err());
        }
    }

    #[test]
    fn spend_uses_microcents_and_rejects_negative_values() {
        assert_eq!(
            parse_spend(&json!({"totalCostMicroCents":"125000000"})).unwrap(),
            "$1.25 USD"
        );
        assert!(parse_spend(&json!({"totalCostMicroCents":"-1"})).is_err());
    }
}
