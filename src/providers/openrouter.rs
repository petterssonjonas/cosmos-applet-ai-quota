// SPDX-License-Identifier: MPL-2.0

use super::{json, number, timestamp};
use crate::model::{Credential, CredentialKind, ProviderError, QuotaWindow, Snapshot};
use reqwest::Client;
use serde_json::Value;

pub async fn fetch(client: &Client, credential: &Credential) -> Result<Snapshot, ProviderError> {
    if credential.kind != CredentialKind::ApiKey {
        return Err(ProviderError(
            "OpenRouter requires an API key or browser authorization.".into(),
        ));
    }
    // These endpoints have independent permissions: /credits can require a management key.
    let key = json(
        client
            .get("https://openrouter.ai/api/v1/key")
            .bearer_auth(&credential.secret),
    )
    .await;
    let credits = json(
        client
            .get("https://openrouter.ai/api/v1/credits")
            .bearer_auth(&credential.secret),
    )
    .await;
    if key.is_err() && credits.is_err() {
        return Err(key
            .err()
            .unwrap_or_else(|| ProviderError("OpenRouter quota is unavailable.".into())));
    }
    let mut snapshot = parse(key.as_ref().ok(), credits.as_ref().ok())?;
    if let Err(failure) = key {
        snapshot
            .notes
            .push(format!("Key quota unavailable: {}", failure.0));
    }
    if let Err(failure) = credits {
        snapshot.notes.push(format!(
            "Prepaid balance unavailable: {} A management key may be required for account credits.",
            failure.0
        ));
    }
    Ok(snapshot)
}

fn nonnegative(value: &Value) -> Option<f64> {
    number(value).filter(|value| *value >= 0.0)
}

fn money(amount: f64) -> String {
    if amount != 0.0 && amount.abs() < 0.01 {
        format!("${amount:.4}")
    } else {
        format!("${amount:.2}")
    }
}

fn interval(value: &Value) -> Option<&'static str> {
    match value.as_str()? {
        "daily" => Some("Daily"),
        "weekly" => Some("Weekly"),
        "monthly" => Some("Monthly"),
        _ => None,
    }
}

fn parse(key: Option<&Value>, credits: Option<&Value>) -> Result<Snapshot, ProviderError> {
    let mut snapshot = Snapshot::default();
    let mut reported = false;
    if let Some(data) = key.and_then(|value| value["data"].as_object()) {
        snapshot.plan = data
            .get("is_free_tier")
            .and_then(Value::as_bool)
            .map(|free| if free { "Free tier" } else { "Pay as you go" }.into());
        reported |= snapshot.plan.is_some();
        let limit = data.get("limit").and_then(nonnegative);
        let remaining = data.get("limit_remaining").and_then(nonnegative);
        if let Some(limit) = limit {
            snapshot
                .details
                .push(("Key spending limit".into(), money(limit)));
            reported = true;
        } else if data.get("limit").is_some_and(Value::is_null) {
            snapshot
                .details
                .push(("Key spending limit".into(), "Unlimited".into()));
            reported = true;
        }
        if let Some(remaining) = remaining {
            snapshot
                .details
                .push(("Key limit remaining".into(), money(remaining)));
            reported = true;
        }
        if limit.is_some() || remaining.is_some() {
            let remaining_percent = limit
                .zip(remaining)
                .filter(|(limit, remaining)| *limit > 0.0 && remaining <= limit)
                .map(|(limit, remaining)| remaining / limit * 100.0);
            let reset_interval = data.get("limit_reset").and_then(interval);
            snapshot.windows.push(QuotaWindow {
                label: reset_interval
                    .map(|interval| format!("{interval} key spending limit"))
                    .unwrap_or_else(|| "Key spending limit".into()),
                remaining_percent,
                resets_at: None,
            });
            if let Some(interval) = reset_interval {
                snapshot
                    .details
                    .push(("Key limit reset interval".into(), interval.into()));
                snapshot.notes.push(
                    "OpenRouter reports the key reset interval, not its exact next reset time."
                        .into(),
                );
            }
        }
        for (field, label) in [
            ("usage", "Key spent total"),
            ("usage_daily", "Key spent today"),
            ("usage_weekly", "Key spent this week"),
            ("usage_monthly", "Key spent this month"),
        ] {
            if let Some(usage) = data.get(field).and_then(nonnegative) {
                snapshot.details.push((label.into(), money(usage)));
                reported = true;
            }
        }
        if let Some(budget) = data
            .get("effective_budget")
            .filter(|budget| budget.is_object())
        {
            let limit = nonnegative(&budget["limit_usd"]);
            let remaining = nonnegative(&budget["remaining_usd"]);
            if let Some(remaining) = remaining {
                snapshot
                    .details
                    .push(("Effective budget remaining".into(), money(remaining)));
                reported = true;
            }
            if let Some(limit) = limit {
                snapshot
                    .details
                    .push(("Effective budget limit".into(), money(limit)));
                reported = true;
            }
            if limit.is_some() || remaining.is_some() {
                snapshot.windows.push(QuotaWindow {
                    label: "Effective spending budget".into(),
                    remaining_percent: limit
                        .zip(remaining)
                        .filter(|(limit, remaining)| *limit > 0.0 && remaining <= limit)
                        .map(|(limit, remaining)| remaining / limit * 100.0),
                    resets_at: timestamp(&budget["resets_at"]),
                });
            }
        }
        if let Some(expiry) = data
            .get("expires_at")
            .and_then(timestamp)
            .and_then(|expiry| chrono::DateTime::from_timestamp(expiry, 0))
        {
            snapshot.details.push((
                "Key expires".into(),
                expiry.format("%Y-%m-%d %H:%M UTC").to_string(),
            ));
        }
    }
    if let Some(data) = credits.and_then(|value| value["data"].as_object()) {
        let purchased = data.get("total_credits").and_then(nonnegative);
        let spent = data.get("total_usage").and_then(nonnegative);
        if let (Some(purchased), Some(spent)) = (purchased, spent) {
            let balance = purchased - spent;
            if balance.is_finite() {
                snapshot.credits = Some(format!("{} prepaid balance", money(balance)));
                snapshot
                    .details
                    .push(("Account credits purchased".into(), money(purchased)));
                snapshot
                    .details
                    .push(("Account credits spent".into(), money(spent)));
                reported = true;
            }
        }
    }
    if !reported {
        return Err(ProviderError(
            "OpenRouter returned no recognized quota or credit data.".into(),
        ));
    }
    // Spending limits and account credits are not banked subscription resets.
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_limit_and_prepaid_balance_are_distinct() {
        let key = json!({"data":{"limit":20, "limit_remaining":5, "usage":15, "usage_daily":1.25, "usage_weekly":7.5, "usage_monthly":15, "limit_reset":"monthly"}});
        let credits = json!({"data":{"total_credits":100, "total_usage":40}});
        let snapshot = parse(Some(&key), Some(&credits)).unwrap();
        assert_eq!(snapshot.windows[0].remaining_percent, Some(25.0));
        assert_eq!(snapshot.windows[0].resets_at, None);
        assert_eq!(snapshot.credits.as_deref(), Some("$60.00 prepaid balance"));
        assert!(
            snapshot
                .details
                .iter()
                .any(|(label, value)| label == "Key spent today" && value == "$1.25")
        );
        assert_eq!(snapshot.resets, None);
    }

    #[test]
    fn unlimited_missing_and_zero_limits_do_not_fabricate_percentage() {
        let unlimited = json!({"data":{"limit":null,"usage":3}});
        assert!(parse(Some(&unlimited), None).unwrap().windows.is_empty());
        let zero = json!({"data":{"limit":0,"limit_remaining":0}});
        assert_eq!(
            parse(Some(&zero), None).unwrap().windows[0].remaining_percent,
            None
        );
        let partial = json!({"data":{"limit":10,"usage":3}});
        let parsed = parse(Some(&partial), None).unwrap();
        assert_eq!(parsed.windows[0].remaining_percent, None);
        assert_eq!(parsed.credits, None);
    }

    #[test]
    fn endpoint_data_can_succeed_independently() {
        let key = json!({"data":{"limit_remaining":1.5}});
        assert!(parse(Some(&key), None).unwrap().credits.is_none());
        let credits = json!({"data":{"total_credits":1,"total_usage":2}});
        assert_eq!(
            parse(None, Some(&credits)).unwrap().credits.as_deref(),
            Some("$-1.00 prepaid balance")
        );
        assert!(
            parse(
                Some(&json!({"data":{}})),
                Some(&json!({"data":{"total_credits":3}}))
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_numeric_fields_remain_unknown() {
        let key = json!({"data":{"is_free_tier":true, "limit":"NaN", "limit_remaining":-1, "usage":"Infinity"}});
        let snapshot = parse(Some(&key), None).unwrap();
        assert!(snapshot.windows.is_empty());
        assert!(snapshot.details.is_empty());
        assert_eq!(snapshot.plan.as_deref(), Some("Free tier"));
    }

    #[test]
    fn effective_budget_keeps_exact_reported_reset() {
        let key = json!({"data":{"effective_budget":{"limit_usd":50,"remaining_usd":17.5,"resets_at":"2026-10-12T00:00:00Z"}}});
        let snapshot = parse(Some(&key), None).unwrap();
        assert_eq!(snapshot.windows[0].remaining_percent, Some(35.0));
        assert!(snapshot.windows[0].resets_at.is_some());
    }
}
