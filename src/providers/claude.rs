// SPDX-License-Identifier: MPL-2.0
//! Read-only Claude quotas. Schema references: steipete/CodexBar at
//! e92d0ed0ff03b83de91daecd8de4117564f75b0c, docs/claude.md and
//! Sources/CodexBarCore/Providers/Claude/{ClaudeOAuth/ClaudeOAuthUsageFetcher.swift,
//! ClaudeRateLimitResetCredits.swift,ClaudeCloudCreditsSnapshot.swift,
//! ClaudeWeb/ClaudeWebAPIFetcher.swift}. No redemption or inference requests.

use reqwest::{Client, RequestBuilder, header::HeaderValue};
use serde_json::Value;

use super::{json, number, remaining, timestamp};
use crate::model::{BankedReset, Credential, CredentialKind, ProviderError, QuotaWindow, Snapshot};

const OAUTH_USAGE: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_PROFILE: &str = "https://api.anthropic.com/api/oauth/profile";
const WEB: &str = "https://claude.ai/api";
const BETA: &str = "oauth-2025-04-20";
const MAX_BODY: usize = 2 * 1024 * 1024;

pub async fn fetch(client: &Client, credential: &Credential) -> Result<Snapshot, ProviderError> {
    if credential
        .expires_at
        .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp())
    {
        return Err(ProviderError(
            "Claude credential expired. Reconnect Claude.".into(),
        ));
    }
    match credential.kind {
        CredentialKind::OAuth => fetch_oauth(client, credential).await,
        CredentialKind::Cookie => fetch_cookie(client, credential).await,
        CredentialKind::ApiKey => Err(ProviderError(
            "Claude subscription quotas require a Claude Code OAuth sign-in or an explicit claude.ai session cookie, not an API key.".into(),
        )),
    }
}

fn oauth_request(client: &Client, url: &str, credential: &Credential) -> RequestBuilder {
    client
        .get(url)
        .bearer_auth(&credential.secret)
        .header("anthropic-beta", BETA)
        .header("Accept", "application/json")
}

async fn fetch_oauth(client: &Client, credential: &Credential) -> Result<Snapshot, ProviderError> {
    // CodexBar's documented fallback identity; no CLI process or credential refresh is launched.
    let request = oauth_request(client, OAUTH_USAGE, credential)
        .query(&[("cedar_ember", "1")])
        .header("User-Agent", "claude-cli/2.1.0 (external, cli)");
    let (status, body, challenge) = usage_response(request).await?;
    let retried = should_retry(status, &body, challenge);
    let value = if retried {
        let (status, body, _) = usage_response(
            oauth_request(client, OAUTH_USAGE, credential)
                .header("User-Agent", "claude-code/2.1.0"),
        )
        .await?;
        decode_response(status, &body)?
    } else {
        decode_response(status, &body)?
    };
    let mut snapshot = parse_usage(&value, chrono::Utc::now().timestamp())?;
    if retried {
        snapshot.notes.push("Claude rejected the optional reset-inventory query; ordinary usage was fetched without it.".into());
    }
    snapshot.plan = credential.plan_hint.as_deref().and_then(plan_label);
    if snapshot.plan.is_some() {
        snapshot.notes.push(
            "Plan comes from the imported Claude credential, not a live billing response.".into(),
        );
    }
    match json(oauth_request(client, OAUTH_PROFILE, credential)).await {
        Ok(profile) if profile.is_object() => {
            if let Some(email) = profile
                .pointer("/account/email_address")
                .or_else(|| profile.pointer("/account/emailAddress"))
                .or_else(|| profile.get("email_address"))
                .and_then(Value::as_str)
                .filter(|email| !email.trim().is_empty())
            {
                snapshot
                    .details
                    .push(("Account".into(), email.trim().into()));
            }
        }
        _ => snapshot
            .notes
            .push("Claude profile is inaccessible; quota measurements are still available.".into()),
    }
    snapshot.notes.push("Prepaid extra-usage credits require an explicitly supplied claude.ai cookie; OAuth alone cannot read that balance.".into());
    Ok(snapshot)
}

// Unlike the shared JSON helper, this path needs HTTP status and a bounded body to distinguish
// optional-query rejection from missing user:profile scope. Bodies are never returned as errors.
async fn usage_response(request: RequestBuilder) -> Result<(u16, Vec<u8>, bool), ProviderError> {
    let mut response = request
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| {
            ProviderError(if error.is_timeout() {
                "Request timed out".into()
            } else {
                "Network request failed".into()
            })
        })?;
    let status = response.status().as_u16();
    let challenge = response
        .headers()
        .get("cf-mitigated")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("challenge"));
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderError("Could not read provider response".into()))?
    {
        if chunk.len() > MAX_BODY - body.len() {
            return Err(ProviderError("Provider response is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, body, challenge))
}

fn should_retry(status: u16, body: &[u8], challenge: bool) -> bool {
    if status != 400 && status != 403 {
        return false;
    }
    let text = String::from_utf8_lossy(body);
    !challenge
        && !text.contains("user:profile")
        && !text.to_ascii_lowercase().contains("just a moment")
}

fn decode_response(status: u16, body: &[u8]) -> Result<Value, ProviderError> {
    if !(200..300).contains(&status) {
        return Err(ProviderError(match status {
            401 => "Claude sign-in expired or credential rejected. Reconnect Claude.".into(),
            403 if String::from_utf8_lossy(body).contains("user:profile") =>
                "Claude token lacks user:profile usage access. Reconnect with Claude Code sign-in; setup-token does not grant usage access.".into(),
            403 => "Claude denied access (HTTP 403); the session may require browser verification.".into(),
            429 => "Claude rate limit reached. Try again later.".into(),
            _ => format!("Claude returned HTTP {status}"),
        }));
    }
    serde_json::from_slice(body).map_err(|_| ProviderError("Claude returned invalid JSON".into()))
}

fn cookie_header(secret: &str) -> Result<HeaderValue, ProviderError> {
    let secret = secret.trim();
    let secret = if secret
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
    {
        secret[7..].trim()
    } else {
        secret
    };
    let raw = if secret.starts_with("sk-ant-") && !secret.contains([';', '=', '\r', '\n']) {
        format!("sessionKey={secret}")
    } else {
        secret.to_owned()
    };
    if !raw.split(';').any(|part| {
        part.trim()
            .strip_prefix("sessionKey=")
            .is_some_and(|value| !value.is_empty())
    }) {
        return Err(ProviderError(
            "Provide an explicit claude.ai sessionKey cookie or Cookie header.".into(),
        ));
    }
    let mut header = HeaderValue::from_str(&raw)
        .map_err(|_| ProviderError("Claude cookie contains invalid header characters.".into()))?;
    header.set_sensitive(true);
    Ok(header)
}

fn web_request(client: &Client, url: &str, cookie: &HeaderValue) -> RequestBuilder {
    client
        .get(url)
        .header("Cookie", cookie.clone())
        .header("Accept", "application/json")
}

async fn fetch_cookie(client: &Client, credential: &Credential) -> Result<Snapshot, ProviderError> {
    let cookie = cookie_header(&credential.secret)?;
    let organizations = json(web_request(
        client,
        &format!("{WEB}/organizations"),
        &cookie,
    ))
    .await?;
    let org = select_organization(&organizations, credential.account_id.as_deref())?;
    let org_id = org["uuid"]
        .as_str()
        .filter(|uuid| valid_uuid(uuid))
        .ok_or_else(|| ProviderError("Claude organization identifier is invalid.".into()))?;
    let org_url = format!("{WEB}/organizations/{org_id}");
    let (status, body, challenge) = usage_response(web_request(
        client,
        &format!("{org_url}/usage?cedar_ember=1"),
        &cookie,
    ))
    .await?;
    let retried = should_retry(status, &body, challenge);
    let usage = if retried {
        json(web_request(client, &format!("{org_url}/usage"), &cookie)).await?
    } else {
        decode_response(status, &body)?
    };
    let mut snapshot = parse_usage(&usage, chrono::Utc::now().timestamp())?;
    if retried {
        snapshot.notes.push("Claude rejected the optional reset-inventory query; ordinary usage was fetched without it.".into());
    }
    match json(web_request(client, &format!("{WEB}/account"), &cookie)).await {
        Ok(account) => {
            snapshot.plan = web_plan(&account, org_id);
        }
        Err(_) => snapshot.notes.push(
            "Claude account plan is inaccessible; quota measurements are still available.".into(),
        ),
    }
    if snapshot.plan.is_none() {
        snapshot.plan = credential.plan_hint.as_deref().and_then(plan_label);
        if snapshot.plan.is_some() {
            snapshot.notes.push(
                "Plan comes from the supplied credential hint, not a live billing response.".into(),
            );
        }
    }
    if !snapshot
        .details
        .iter()
        .any(|(label, _)| label == "Extra-usage spending allowance")
    {
        match json(web_request(
            client,
            &format!("{org_url}/overage_spend_limit"),
            &cookie,
        ))
        .await
        {
            Ok(spend) => add_spending(&mut snapshot, &spend, "monthly_credit_limit"),
            Err(_) => snapshot
                .notes
                .push("Claude extra-usage spending allowance is inaccessible.".into()),
        }
    }
    let prepaid = json(web_request(
        client,
        &format!("{org_url}/prepaid/credits"),
        &cookie,
    ))
    .await
    .ok()
    .and_then(|value| prepaid_balance(&value));
    if let Some(prepaid) = prepaid {
        append_credit(&mut snapshot, &format!("Prepaid extra usage: {prepaid}"));
    } else {
        snapshot.notes.push("Claude prepaid extra-usage credits are inaccessible or not exposed to this browser session.".into());
    }
    Ok(snapshot)
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn select_organization<'a>(
    value: &'a Value,
    requested: Option<&str>,
) -> Result<&'a Value, ProviderError> {
    let orgs = value
        .as_array()
        .ok_or_else(|| ProviderError("Claude returned invalid organization data.".into()))?;
    if let Some(requested) = requested.filter(|id| !id.trim().is_empty()) {
        return orgs
            .iter()
            .find(|org| org["uuid"].as_str() == Some(requested.trim()))
            .ok_or_else(|| {
                ProviderError(
                    "Selected Claude organization is unavailable. Check the organization ID."
                        .into(),
                )
            });
    }
    let mut chat = None;
    let mut non_api = None;
    let mut ambiguous_non_api = false;
    for org in orgs {
        let capabilities = org["capabilities"].as_array();
        if capabilities.is_some_and(|caps| {
            caps.iter()
                .any(|cap| cap.as_str().is_some_and(|s| s.eq_ignore_ascii_case("chat")))
        }) {
            if chat.is_some() {
                return Err(ProviderError(
                    "Multiple Claude chat organizations are available. Supply an organization ID."
                        .into(),
                ));
            }
            chat = Some(org);
        } else if chat.is_none()
            && !capabilities.is_some_and(|caps| {
                caps.len() == 1
                    && caps[0]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case("api"))
            })
        {
            if non_api.is_some() {
                ambiguous_non_api = true;
            } else {
                non_api = Some(org);
            }
        }
    }
    if let Some(chat) = chat {
        return Ok(chat);
    }
    if ambiguous_non_api {
        return Err(ProviderError(
            "Multiple Claude organizations are available. Supply an organization ID.".into(),
        ));
    }
    non_api.ok_or_else(|| ProviderError("No Claude chat organization is available.".into()))
}

fn plan_label(hint: &str) -> Option<String> {
    let hint = hint.trim().to_ascii_lowercase();
    let words: Vec<_> = hint
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let (index, plan) = words
        .iter()
        .enumerate()
        .find_map(|(index, word)| match *word {
            "max" => Some((index, "Max")),
            "pro" => Some((index, "Pro")),
            "team" => Some((index, "Team")),
            "enterprise" => Some((index, "Enterprise")),
            "ultra" => Some((index, "Ultra")),
            "free" => Some((index, "Free")),
            _ => None,
        })?;
    if plan == "Max"
        && let Some(multiplier) = words
            .get(index + 1)
            .filter(|word| word.ends_with('x') && word[..word.len() - 1].parse::<u32>().is_ok())
    {
        return Some(format!("Claude Max {multiplier}"));
    }
    Some(format!("Claude {plan}"))
}

fn web_plan(account: &Value, org_id: &str) -> Option<String> {
    let membership = account["memberships"]
        .as_array()?
        .iter()
        .find(|membership| {
            membership
                .pointer("/organization/uuid")
                .and_then(Value::as_str)
                == Some(org_id)
        })?;
    let tier = membership
        .pointer("/organization/rate_limit_tier")
        .and_then(Value::as_str);
    let mut plan = tier.and_then(plan_label);
    if plan.is_none()
        && tier.is_some_and(|tier| tier.contains("claude"))
        && membership
            .pointer("/organization/billing_type")
            .and_then(Value::as_str)
            .is_some_and(|billing| billing.contains("stripe"))
    {
        plan = Some("Claude Pro".into());
    }
    if plan.as_deref() == Some("Claude Team") {
        match membership["seat_tier"].as_str() {
            Some("team_standard") => plan = Some("Claude Team Standard".into()),
            Some("team_tier_1") => plan = Some("Claude Team Premium".into()),
            _ => {}
        }
    }
    plan
}

fn parse_usage(value: &Value, now: i64) -> Result<Snapshot, ProviderError> {
    let object = value
        .as_object()
        .ok_or_else(|| ProviderError("Claude returned invalid usage data.".into()))?;
    if ![
        "five_hour",
        "seven_day",
        "seven_day_sonnet",
        "seven_day_opus",
        "limits",
        "extra_usage",
        "cedar_ember",
        "iguana_necktie",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
    {
        return Err(ProviderError(
            "Claude response does not contain recognized usage data.".into(),
        ));
    }
    let mut snapshot = Snapshot::default();
    for (key, label) in [
        ("five_hour", "Session (5 hours)"),
        ("seven_day", "Weekly (all models)"),
        ("seven_day_sonnet", "Sonnet weekly"),
        ("seven_day_opus", "Opus weekly"),
        ("seven_day_oauth_apps", "OAuth apps weekly"),
    ] {
        if let Some(window) = value.get(key).filter(|window| window.is_object()) {
            snapshot
                .windows
                .push(window_from(window, "utilization", label));
        }
    }
    if let Some(limits) = value["limits"].as_array() {
        for limit in limits {
            let kind = limit["kind"].as_str();
            let group = limit["group"].as_str();
            let label = match (kind, group) {
                (Some("session"), Some("session")) => Some("Session (5 hours)".to_owned()),
                (Some("weekly_all"), Some("weekly")) => Some("Weekly (all models)".to_owned()),
                (Some("weekly_scoped"), Some("weekly")) => {
                    let model = limit
                        .pointer("/scope/model/display_name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|model| !model.is_empty());
                    model.map(|model| {
                        let id = limit
                            .pointer("/scope/model/id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .replace('_', "-")
                            .to_ascii_lowercase();
                        if model.eq_ignore_ascii_case("All models")
                            || id == "all-models"
                            || id.ends_with("-all-models")
                        {
                            "Weekly (all models)".into()
                        } else {
                            format!("{model} weekly")
                        }
                    })
                }
                _ => None,
            };
            if let Some(label) = label {
                let window = window_from(limit, "percent", &label);
                if let Some(existing) = snapshot
                    .windows
                    .iter_mut()
                    .find(|window| window.label == label)
                {
                    if existing.remaining_percent.is_none() && window.remaining_percent.is_some() {
                        *existing = window;
                    }
                } else {
                    snapshot.windows.push(window);
                }
            }
        }
    }
    for label in ["Session (5 hours)", "Weekly (all models)"] {
        if !snapshot.windows.iter().any(|window| window.label == label) {
            snapshot.windows.push(QuotaWindow {
                label: label.into(),
                remaining_percent: None,
                resets_at: None,
            });
        }
    }
    for key in ["seven_day_routines", "seven_day_cowork"] {
        if let Some(window) = value.get(key).filter(|window| window.is_object()) {
            snapshot
                .windows
                .push(window_from(window, "utilization", "Daily Routines"));
            break;
        }
    }
    if snapshot
        .windows
        .iter()
        .any(|window| window.remaining_percent.is_none())
    {
        snapshot.notes.push(
            "Some Claude quota measurements are not exposed; unavailable percentages are not zero."
                .into(),
        );
    }
    snapshot.resets = reset_inventory(&value["cedar_ember"], now, &mut snapshot.notes);
    if snapshot.resets.is_none() {
        snapshot.notes.push(
            "Banked reset inventory is inaccessible, ineligible, or not exposed by Claude.".into(),
        );
    }
    add_spending(&mut snapshot, &value["extra_usage"], "monthly_limit");
    if let Some(cloud) = cloud_credit(&value["iguana_necktie"], now) {
        append_credit(&mut snapshot, &cloud);
    }
    Ok(snapshot)
}

fn window_from(value: &Value, percent_key: &str, label: &str) -> QuotaWindow {
    QuotaWindow {
        label: label.into(),
        remaining_percent: remaining(&value[percent_key]),
        resets_at: timestamp(&value["resets_at"]),
    }
}

fn bound(value: &Value, key: &str) -> Result<Option<i64>, ()> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => timestamp(value).map(Some).ok_or(()),
    }
}

fn reset_inventory(value: &Value, now: i64, notes: &mut Vec<String>) -> Option<Vec<BankedReset>> {
    if value["eligible"].as_bool() != Some(true) {
        return None;
    }
    let grants = value["grants"].as_array()?;
    if grants.len() > 200 {
        return None;
    }
    let mut resets = Vec::new();
    let mut malformed = false;
    for grant in grants {
        let parsed = (|| {
            let left = grant["resets_left"].as_u64()?;
            let paused = grant["paused"].as_bool()?;
            if let Some(total) = grant.get("resets_total").filter(|value| !value.is_null())
                && total.as_u64()? < left
            {
                return None;
            }
            Some((
                left,
                paused,
                bound(grant, "starts_at").ok()?,
                bound(grant, "ends_at").ok()?,
            ))
        })();
        let Some((left, paused, starts, ends)) = parsed else {
            malformed = true;
            continue;
        };
        if paused
            || left == 0
            || starts.is_some_and(|start| start > now)
            || ends.is_some_and(|end| end <= now)
        {
            continue;
        }
        if left > (50 - resets.len()) as u64 {
            return None;
        }
        for _ in 0..left {
            resets.push(BankedReset {
                label: format!("Banked reset {}", resets.len() + 1),
                expires_at: ends,
            });
        }
    }
    if malformed {
        notes.push(
            "Malformed banked-reset records were omitted; available inventory may be incomplete."
                .into(),
        );
        if resets.is_empty() {
            return None;
        }
    }
    if resets.iter().any(|reset| reset.expires_at.is_none()) {
        notes.push("Claude did not provide an expiry for every banked reset.".into());
    }
    Some(resets)
}

fn currency(value: &Value) -> Option<String> {
    let code = value.as_str()?.trim();
    (code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_alphabetic()))
        .then(|| code.to_ascii_uppercase())
}

fn add_spending(snapshot: &mut Snapshot, value: &Value, limit_key: &str) {
    if value["is_enabled"].as_bool() != Some(true) {
        return;
    }
    let Some(used) = number(&value["used_credits"]).filter(|used| *used >= 0.0) else {
        return;
    };
    let Some(limit) = number(&value[limit_key]).filter(|limit| *limit >= 0.0) else {
        return;
    };
    // OAuth's extra_usage omits currency in observed fixtures; those amounts are USD cents.
    let code = if value["currency"].is_null() {
        Some("USD".into())
    } else {
        currency(&value["currency"])
    };
    let Some(code) = code else {
        return;
    };
    snapshot.details.push((
        "Extra-usage spending allowance".into(),
        format!(
            "{code} {:.2} remaining of {:.2} monthly cap ({:.2} spent); not prepaid credits",
            (limit - used).max(0.0) / 100.0,
            limit / 100.0,
            used / 100.0
        ),
    ));
}

fn prepaid_balance(value: &Value) -> Option<String> {
    let amount = number(&value["amount"]).filter(|amount| *amount >= 0.0)?;
    let currency = currency(&value["currency"])?;
    Some(format!("{currency} {:.2}", amount / 100.0))
}

fn append_credit(snapshot: &mut Snapshot, credit: &str) {
    if let Some(existing) = &mut snapshot.credits {
        existing.push('\n');
        existing.push_str(credit);
    } else {
        snapshot.credits = Some(credit.into());
    }
}

fn cloud_credit(value: &Value, now: i64) -> Option<String> {
    let limit = number(&value["limit_dollars"]).filter(|limit| *limit > 0.0)?;
    let used = number(&value["used_dollars"]);
    let reported = number(&value["remaining_dollars"]);
    for key in ["used_dollars", "remaining_dollars"] {
        if !value[key].is_null()
            && !number(&value[key]).is_some_and(|amount| (0.0..=limit).contains(&amount))
        {
            return None;
        }
    }
    let balance = reported.or_else(|| used.map(|used| limit - used))?;
    let expiry = bound(value, "resets_at").ok()?;
    let locked = match value.get("locked_reason") {
        None | Some(Value::Null) => false,
        Some(reason) => !reason.as_str()?.trim().is_empty(),
    };
    let mut result = if expiry.is_some_and(|expiry| expiry <= now) {
        "Promotional cloud credits: expired".into()
    } else if locked {
        "Promotional cloud credits: unavailable (locked)".into()
    } else {
        format!("Promotional cloud credits: USD {balance:.2} of {limit:.2} remaining")
    };
    if let Some(date) =
        expiry.and_then(|expiry| chrono::DateTime::<chrono::Utc>::from_timestamp(expiry, 0))
    {
        result.push_str(&format!("; expires {}", date.to_rfc3339()));
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn scoped_fable_keeps_its_own_percentage_and_reset_even_when_inactive() {
        let usage = json!({"five_hour":{"utilization":0},"limits":[
            {"kind":"weekly_scoped","group":"weekly","percent":5,"resets_at":"2026-07-08T09:00:00.283070+00:00","scope":{"model":{"id":null,"display_name":"Fable"}},"is_active":false}
        ]});
        let snapshot = parse_usage(&usage, NOW).unwrap();
        assert_eq!(
            snapshot
                .windows
                .iter()
                .find(|window| window.label == "Fable weekly"),
            Some(&QuotaWindow {
                label: "Fable weekly".into(),
                remaining_percent: Some(95.0),
                resets_at: timestamp(&json!("2026-07-08T09:00:00.283070+00:00")),
            })
        );
    }

    #[test]
    fn zero_utilization_is_known_but_missing_and_negative_are_not() {
        let snapshot = parse_usage(&json!({"five_hour":{"utilization":0},"seven_day":{},"seven_day_opus":{"utilization":-1}}), NOW).unwrap();
        assert_eq!(
            snapshot
                .windows
                .iter()
                .map(|window| window.remaining_percent)
                .collect::<Vec<_>>(),
            vec![Some(100.0), None, None]
        );
    }

    #[test]
    fn all_models_scope_is_weekly_not_a_duplicate_model_window() {
        let snapshot = parse_usage(&json!({"five_hour":null,"limits":[
            {"kind":"weekly_scoped","group":"weekly","percent":100,"scope":{"model":{"display_name":"All models"}}},
            {"kind":"weekly_scoped","group":"weekly","percent":0,"scope":{"model":{"display_name":"Fable"}}}
        ]}), NOW).unwrap();
        assert_eq!(
            snapshot
                .windows
                .iter()
                .map(|window| (window.label.as_str(), window.remaining_percent))
                .collect::<Vec<_>>(),
            vec![
                ("Weekly (all models)", Some(0.0)),
                ("Fable weekly", Some(100.0)),
                ("Session (5 hours)", None),
            ]
        );
    }

    #[test]
    fn banked_resets_expand_each_expiry_and_ignore_redemption_gate() {
        let mut notes = Vec::new();
        let resets = reset_inventory(
            &json!({"eligible":true,"grants":[
                {"resets_left":2,"paused":false,"usable_now":false,"ends_at":NOW+100},
                {"resets_left":1,"paused":false},
                {"resets_left":1,"paused":true},
                {"resets_left":1,"paused":false,"ends_at":NOW},
                {"resets_left":1,"paused":false,"starts_at":NOW+1}
            ]}),
            NOW,
            &mut notes,
        )
        .unwrap();
        assert_eq!(
            resets
                .iter()
                .map(|reset| reset.expires_at)
                .collect::<Vec<_>>(),
            vec![Some(NOW + 100), Some(NOW + 100), None]
        );
    }

    #[test]
    fn bank_expiry_parses_iso8601_into_epoch_seconds() {
        let mut notes = Vec::new();
        let resets = reset_inventory(&json!({"eligible":true,"grants":[
            {"resets_left":1,"paused":false,"starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-01T00:01:40Z"}
        ]}), 50, &mut notes).unwrap();
        assert_eq!(resets[0].expires_at, Some(100));
    }

    #[test]
    fn scoped_missing_percent_remains_unknown_not_full() {
        let snapshot = parse_usage(
            &json!({"limits":[
                {"kind":"weekly_scoped","group":"weekly","scope":{"model":{"display_name":"Fable"}}}
            ]}),
            NOW,
        )
        .unwrap();
        assert_eq!(
            snapshot
                .windows
                .iter()
                .find(|window| window.label == "Fable weekly")
                .unwrap()
                .remaining_percent,
            None
        );
    }

    #[test]
    fn malformed_expiry_never_becomes_an_unbounded_grant() {
        let mut notes = Vec::new();
        assert_eq!(
            reset_inventory(
                &json!({"eligible":true,"grants":[{"resets_left":1,"paused":false,"ends_at":"not-a-date"}]}),
                NOW,
                &mut notes
            ),
            None
        );
    }

    #[test]
    fn confirmed_empty_inventory_is_not_unavailable() {
        let mut notes = Vec::new();
        assert_eq!(
            reset_inventory(&json!({"eligible":true,"grants":[]}), NOW, &mut notes),
            Some(Vec::new())
        );
    }

    #[test]
    fn malformed_optional_inventory_preserves_primary_usage() {
        let snapshot = parse_usage(&json!({"five_hour":{"utilization":25},"cedar_ember":{"eligible":true,"grants":"invalid"}}), NOW).unwrap();
        assert_eq!(
            (snapshot.windows[0].remaining_percent, snapshot.resets),
            (Some(75.0), None)
        );
    }

    #[test]
    fn implausibly_large_reset_count_does_not_allocate_inventory() {
        let mut notes = Vec::new();
        assert_eq!(
            reset_inventory(
                &json!({"eligible":true,"grants":[{"resets_left":u64::MAX,"paused":false}]}),
                NOW,
                &mut notes
            ),
            None
        );
    }

    #[test]
    fn spending_cap_is_not_a_prepaid_credit_balance() {
        let snapshot = parse_usage(
            &json!({"extra_usage":{"is_enabled":true,"monthly_limit":1000,"used_credits":300}}),
            NOW,
        )
        .unwrap();
        assert_eq!(
            (snapshot.credits, snapshot.details[0].1.as_str()),
            (
                None,
                "USD 7.00 remaining of 10.00 monthly cap (3.00 spent); not prepaid credits"
            )
        );
    }

    #[test]
    fn prepaid_minor_units_preserve_real_zero() {
        assert_eq!(
            prepaid_balance(&json!({"amount":0,"currency":"usd"})),
            Some("USD 0.00".into())
        );
    }

    #[test]
    fn missing_prepaid_amount_is_unavailable() {
        assert_eq!(prepaid_balance(&json!({"currency":"USD"})), None);
    }

    #[test]
    fn promotional_cloud_credits_are_dollars_not_cents() {
        assert_eq!(
            cloud_credit(&json!({"limit_dollars":100,"used_dollars":20}), NOW),
            Some("Promotional cloud credits: USD 80.00 of 100.00 remaining".into())
        );
    }

    #[test]
    fn expired_cloud_credit_is_not_a_live_balance() {
        assert!(
            cloud_credit(
                &json!({"limit_dollars":100,"remaining_dollars":20,"resets_at":NOW}),
                NOW
            )
            .unwrap()
            .starts_with("Promotional cloud credits: expired")
        );
    }

    #[test]
    fn malformed_primary_payloads_fail_without_echoing_body() {
        for value in [json!([]), json!({"error":"private body"}), json!(null)] {
            assert!(parse_usage(&value, NOW).is_err());
        }
        assert_eq!(
            decode_response(200, b"private non-JSON body")
                .unwrap_err()
                .0,
            "Claude returned invalid JSON"
        );
    }

    #[test]
    fn only_optional_query_rejection_allows_one_fallback() {
        for status in [400, 403] {
            assert!(should_retry(status, b"{}", false));
            assert!(!should_retry(status, b"Missing scope user:profile", false));
        }
        for status in [200, 401, 429, 500] {
            assert!(!should_retry(status, b"{}", false));
        }
        assert!(!should_retry(403, b"{}", true));
        assert!(!should_retry(403, b"Just a moment", false));
    }

    #[test]
    fn max_multiplier_comes_from_explicit_tier_evidence() {
        assert_eq!(
            plan_label("default_claude_max_20x"),
            Some("Claude Max 20x".into())
        );
        assert_eq!(plan_label("unknown account"), None);
    }

    #[test]
    fn cookie_plan_uses_matching_organization_not_first_membership() {
        assert_eq!(
            web_plan(
                &json!({"memberships":[
                    {"organization":{"uuid":"other","rate_limit_tier":"default_claude_max_20x"}},
                    {"organization":{"uuid":"chosen","rate_limit_tier":"default_claude_pro"}}
                ]}),
                "chosen"
            ),
            Some("Claude Pro".into())
        );
    }

    #[test]
    fn multiple_chat_organizations_require_explicit_selection() {
        let organizations = json!([
            {"uuid":"first","capabilities":["chat"]},
            {"uuid":"second","capabilities":["chat"]}
        ]);
        assert!(
            select_organization(&organizations, None)
                .unwrap_err()
                .0
                .contains("Supply an organization ID")
        );
        assert_eq!(
            select_organization(&organizations, Some("second")).unwrap()["uuid"],
            "second"
        );
    }

    #[test]
    fn multiple_non_api_organizations_are_not_silently_selected() {
        let organizations = json!([{"uuid":"first"}, {"uuid":"second"}]);
        assert!(select_organization(&organizations, None).is_err());
    }

    #[test]
    fn unique_chat_organization_takes_priority_over_non_api_candidates() {
        let organizations = json!([
            {"uuid":"first"}, {"uuid":"second"},
            {"uuid":"chat","capabilities":["chat"]}
        ]);
        assert_eq!(
            select_organization(&organizations, None).unwrap()["uuid"],
            "chat"
        );
    }

    #[test]
    fn api_only_organizations_do_not_supply_browser_quota_context() {
        assert!(
            select_organization(&json!([{"uuid":"api","capabilities":["api"]}]), None).is_err()
        );
    }
}
