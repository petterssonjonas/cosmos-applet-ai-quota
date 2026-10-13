// SPDX-License-Identifier: MPL-2.0

use crate::model::{Credential, CredentialKind, Provider, ProviderError};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
    time::timeout,
};

const APP: &str = "cosmic-applet-ai-quota";
const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;
const STORE_ERROR: &str = "Secret Service is unavailable or locked. Install libsecret-tools and unlock your desktop keyring, then reconnect. Plaintext fallback is disabled.";
static WRITERS: [Mutex<()>; 5] = [const { Mutex::const_new(()) }; 5];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingSource {
    Cli,
    OpenCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Selection {
    Disconnected,
    Cli,
    OpenCode,
    Keyring,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    source: Selection,
}

fn error(message: &str) -> ProviderError {
    ProviderError(message.into())
}

fn provider_index(provider: Provider) -> usize {
    match provider {
        Provider::Claude => 0,
        Provider::OpenAi => 1,
        Provider::OpenCodeGo => 2,
        Provider::OpenCodeZen => 3,
        Provider::OpenRouter => 4,
    }
}

fn home() -> Result<PathBuf, ProviderError> {
    env_path("HOME")
        .ok_or_else(|| error("HOME is not an absolute path; cannot locate credentials."))
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn source_dir(name: &str, fallback: PathBuf) -> Result<PathBuf, ProviderError> {
    match env::var_os(name) {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(error(
                    "Selected CLI directory override must be an absolute path. Correct the environment and reconnect.",
                ));
            }
            Ok(path)
        }
        None => Ok(fallback),
    }
}

fn config_root() -> Result<PathBuf, ProviderError> {
    Ok(env_path("XDG_CONFIG_HOME")
        .unwrap_or(home()?.join(".config"))
        .join(APP))
}

async fn private_dir(path: &Path) -> Result<(), ProviderError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .await
        .map_err(|_| error("Could not create private app configuration directory."))?;
    let metadata = fs::symlink_metadata(path)
        .await
        .map_err(|_| error("Could not inspect private configuration directory."))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(error(
            "App configuration directory must not be a symbolic link.",
        ));
    }
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|_| error("Could not secure app configuration directory."))
}

async fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, ProviderError> {
    match fs::metadata(path).await {
        Ok(metadata) if !metadata.is_file() => {
            return Err(error("Selected credential source is not a regular file."));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(error("Could not inspect selected credential source.")),
    }
    let file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(error(
                "Could not read selected credential source. Check its file permissions.",
            ));
        }
    };
    if !file
        .metadata()
        .await
        .map_err(|_| error("Could not inspect credential source."))?
        .is_file()
    {
        return Err(error("Selected credential source is not a regular file."));
    }
    let mut bytes = Vec::new();
    file.take((MAX_CREDENTIAL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| error("Could not read selected credential source."))?;
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        return Err(error("Selected credential source is too large."));
    }
    Ok(Some(bytes))
}

async fn selection(provider: Provider) -> Result<Selection, ProviderError> {
    let path = config_root()?.join(format!("{}.json", provider.id()));
    let Some(bytes) = read_bytes(&path).await? else {
        return Ok(Selection::Disconnected);
    };
    let metadata: Metadata = serde_json::from_slice(&bytes)
        .map_err(|_| error("App authentication metadata is invalid. Reconnect this provider."))?;
    Ok(metadata.source)
}

async fn select(provider: Provider, source: Selection) -> Result<(), ProviderError> {
    let root = config_root()?;
    private_dir(&root).await?;
    let bytes = serde_json::to_vec(&Metadata { source })
        .map_err(|_| error("Could not encode authentication metadata."))?;
    let temporary = root.join(format!(
        ".{}-{:032x}.tmp",
        provider.id(),
        rand::random::<u128>()
    ));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .await
            .map_err(|_| error("Could not create private authentication metadata."))?;
        file.write_all(&bytes)
            .await
            .map_err(|_| error("Could not write authentication metadata."))?;
        file.sync_all()
            .await
            .map_err(|_| error("Could not persist authentication metadata."))?;
        fs::rename(&temporary, root.join(format!("{}.json", provider.id())))
            .await
            .map_err(|_| error("Could not replace authentication metadata."))?;
        fs::File::open(&root)
            .await
            .map_err(|_| error("Could not open authentication metadata directory."))?
            .sync_all()
            .await
            .map_err(|_| error("Could not persist authentication metadata directory."))
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    result
}

async fn secret_tool(
    provider: Provider,
    operation: &str,
    input: Option<&[u8]>,
) -> Result<Vec<u8>, ProviderError> {
    let result = timeout(Duration::from_secs(20), async {
        let mut command = Command::new("/usr/bin/secret-tool");
        command.arg(operation);
        if operation == "store" {
            command.args(["--label", "COSMIC AI Quota credential"]);
        }
        command
            .args(["application", APP, "provider", provider.id()])
            .kill_on_drop(true)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| error(STORE_ERROR))?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().ok_or_else(|| error(STORE_ERROR))?;
            stdin
                .write_all(input)
                .await
                .map_err(|_| error(STORE_ERROR))?;
            stdin.shutdown().await.map_err(|_| error(STORE_ERROR))?;
        }
        let mut bytes = Vec::new();
        let stdout = child.stdout.take().ok_or_else(|| error(STORE_ERROR))?;
        stdout
            .take((MAX_CREDENTIAL_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| error(STORE_ERROR))?;
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            return Err(error(
                "Stored credential is too large. Reconnect this provider.",
            ));
        }
        let mut diagnostics = Vec::new();
        child
            .stderr
            .take()
            .ok_or_else(|| error(STORE_ERROR))?
            .take(16 * 1024)
            .read_to_end(&mut diagnostics)
            .await
            .map_err(|_| error(STORE_ERROR))?;
        let status = child.wait().await.map_err(|_| error(STORE_ERROR))?;
        // libsecret's CLI exits 1 without diagnostics when no matching item exists.
        // Actual service failures emit diagnostics; keep those private and retryable.
        let absent = operation == "clear" && status.code() == Some(1) && diagnostics.is_empty();
        if !status.success() && !absent {
            return Err(error(STORE_ERROR));
        }
        Ok(bytes)
    })
    .await;
    result.map_err(|_| error(STORE_ERROR))?
}

fn valid_secret(secret: &str) -> bool {
    !secret.trim().is_empty() && secret.len() <= 64 * 1024 && !secret.chars().any(char::is_control)
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

fn milliseconds(value: &Value) -> Option<i64> {
    value.as_i64().filter(|n| *n >= 0).map(|n| n / 1000)
}

fn jwt_claims(jwt: &str) -> Option<Value> {
    let mut parts = jwt.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    if header.is_empty()
        || payload.len() > 64 * 1024
        || signature.is_empty()
        || parts.next().is_some()
    {
        return None;
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

fn credential(
    kind: CredentialKind,
    secret: &Value,
    source: &str,
) -> Result<Option<Credential>, ProviderError> {
    let Some(secret) = secret.as_str() else {
        return Ok(None);
    };
    if !valid_secret(secret) {
        return Err(error(
            "Selected credential is empty or malformed. Reconnect this provider.",
        ));
    }
    Ok(Some(Credential {
        kind,
        secret: secret.into(),
        account_id: None,
        plan_hint: None,
        source: source.into(),
        expires_at: None,
    }))
}

fn codex(value: &Value, source: &str) -> Result<Option<Credential>, ProviderError> {
    let tokens = &value["tokens"];
    let Some(mut credential) = credential(CredentialKind::OAuth, &tokens["access_token"], source)?
    else {
        if value["OPENAI_API_KEY"].is_string() {
            return Err(error(
                "Codex is signed in with an API key, not a ChatGPT subscription. Use browser sign-in for subscription quota.",
            ));
        }
        return Ok(None);
    };
    // JWT claims are display/account hints only; the provider validates the bearer token.
    let access = jwt_claims(&credential.secret);
    let identity = tokens["id_token"].as_str().and_then(jwt_claims);
    credential.account_id = text(&tokens["account_id"]).or_else(|| {
        access
            .as_ref()
            .and_then(|v| text(&v["https://api.openai.com/auth"]["chatgpt_account_id"]))
    });
    credential.plan_hint = identity
        .as_ref()
        .and_then(|v| text(&v["https://api.openai.com/auth"]["chatgpt_plan_type"]));
    credential.expires_at = access
        .as_ref()
        .and_then(|v| v["exp"].as_i64())
        .filter(|n| *n >= 0);
    Ok(Some(credential))
}

fn claude(value: &Value, source: &str) -> Result<Option<Credential>, ProviderError> {
    let oauth = &value["claudeAiOauth"];
    let Some(mut credential) = credential(CredentialKind::OAuth, &oauth["accessToken"], source)?
    else {
        return Ok(None);
    };
    credential.plan_hint = match (
        text(&oauth["rateLimitTier"]),
        text(&oauth["subscriptionType"]),
    ) {
        (Some(tier), Some(plan)) => Some(format!("{tier} {plan}")),
        (tier, plan) => tier.or(plan),
    };
    credential.expires_at = milliseconds(&oauth["expiresAt"]);
    Ok(Some(credential))
}

fn opencode(provider: Provider, value: &Value) -> Result<Option<Credential>, ProviderError> {
    let key = match provider {
        Provider::Claude => "anthropic",
        Provider::OpenAi => "openai",
        Provider::OpenCodeGo => "opencode-go",
        Provider::OpenCodeZen => "opencode",
        Provider::OpenRouter => "openrouter",
    };
    let entry = &value[key];
    match entry["type"].as_str() {
        Some("oauth") if matches!(provider, Provider::Claude | Provider::OpenAi) => {
            let Some(mut credential) = credential(
                CredentialKind::OAuth,
                &entry["access"],
                "OpenCode (read-only)",
            )?
            else {
                return Ok(None);
            };
            credential.expires_at = milliseconds(&entry["expires"]);
            if provider == Provider::OpenAi {
                let claims = jwt_claims(&credential.secret);
                credential.account_id = text(&entry["accountId"]).or_else(|| {
                    claims
                        .as_ref()
                        .and_then(|v| text(&v["https://api.openai.com/auth"]["chatgpt_account_id"]))
                });
                credential.plan_hint = claims
                    .as_ref()
                    .and_then(|v| text(&v["https://api.openai.com/auth"]["chatgpt_plan_type"]));
            }
            Ok(Some(credential))
        }
        Some("api") if !matches!(provider, Provider::Claude | Provider::OpenAi) => credential(
            CredentialKind::ApiKey,
            &entry["key"],
            "OpenCode (read-only)",
        ),
        Some("api") => Err(error(
            "Selected OpenCode login uses an API key, not a subscription OAuth login.",
        )),
        None => Ok(None),
        _ => Err(error(
            "Selected OpenCode credential type is not supported for this provider.",
        )),
    }
}

async fn existing(
    provider: Provider,
    source: ExistingSource,
) -> Result<Option<Credential>, ProviderError> {
    if source == ExistingSource::OpenCode
        && let Some(content) = env::var_os("OPENCODE_AUTH_CONTENT")
    {
        let content = content
            .to_str()
            .ok_or_else(|| error("OpenCode authentication environment contains invalid text."))?;
        if content.len() > MAX_CREDENTIAL_BYTES {
            return Err(error("OpenCode authentication environment is too large."));
        }
        let value: Value = serde_json::from_str(content)
            .map_err(|_| error("OpenCode authentication environment contains invalid JSON."))?;
        let mut credential = opencode(provider, &value)?;
        if let Some(credential) = &mut credential {
            credential.source = "OpenCode environment (read-only)".into();
        }
        return Ok(credential);
    }
    let (path, label) = match source {
        ExistingSource::Cli => match provider {
            Provider::Claude => (
                source_dir("CLAUDE_CONFIG_DIR", home()?.join(".claude"))?.join(".credentials.json"),
                "Claude CLI (read-only)",
            ),
            Provider::OpenAi => (
                source_dir("CODEX_HOME", home()?.join(".codex"))?.join("auth.json"),
                "Codex CLI (read-only)",
            ),
            _ => {
                return Err(error(
                    "This provider has no supported native CLI credential source. Connect OpenCode or enter a key.",
                ));
            }
        },
        ExistingSource::OpenCode => (
            env_path("XDG_DATA_HOME")
                .unwrap_or(home()?.join(".local/share"))
                .join("opencode/auth.json"),
            "OpenCode (read-only)",
        ),
    };
    let Some(bytes) = read_bytes(&path).await? else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        error("Selected credential file contains invalid JSON. Sign in again using that CLI.")
    })?;
    match source {
        ExistingSource::Cli if provider == Provider::Claude => claude(&value, label),
        ExistingSource::Cli => codex(&value, label),
        ExistingSource::OpenCode => opencode(provider, &value),
    }
}

pub async fn load(provider: Provider) -> Result<Option<Credential>, ProviderError> {
    timeout(Duration::from_secs(30), async {
        let _guard = WRITERS[provider_index(provider)].lock().await;
        match selection(provider).await? {
            Selection::Disconnected => Ok(None),
            Selection::Cli => existing(provider, ExistingSource::Cli).await,
            Selection::OpenCode => existing(provider, ExistingSource::OpenCode).await,
            Selection::Keyring => {
                let bytes = secret_tool(provider, "lookup", None).await?;
                let credential: Credential = serde_json::from_slice(&bytes).map_err(|_| {
                    error("Stored app credential is invalid. Reconnect this provider.")
                })?;
                if !valid_secret(&credential.secret) {
                    return Err(error(
                        "Stored app credential is malformed. Reconnect this provider.",
                    ));
                }
                Ok(Some(credential))
            }
        }
    })
    .await
    .map_err(|_| error("Reading credentials timed out. Reconnect this provider."))?
}

pub async fn connect_existing(
    provider: Provider,
    source: ExistingSource,
) -> Result<(), ProviderError> {
    timeout(Duration::from_secs(30), async {
        let _guard = WRITERS[provider_index(provider)].lock().await;
        let credential = existing(provider, source).await?.ok_or_else(|| error("No compatible credential in the selected source. Sign in using that CLI first."))?;
        if credential.expires_at.is_some_and(|expires| expires <= chrono::Utc::now().timestamp()) {
            return Err(error("Selected CLI access token has expired. Re-authenticate in that CLI, then connect again."));
        }
        select(provider, match source { ExistingSource::Cli => Selection::Cli, ExistingSource::OpenCode => Selection::OpenCode }).await
    }).await.map_err(|_| error("Connecting credentials timed out. Try again."))?
}

async fn store(provider: Provider, credential: &Credential) -> Result<(), ProviderError> {
    let bytes = serde_json::to_vec(credential)
        .map_err(|_| error("Could not encode credential for secure storage."))?;
    secret_tool(provider, "store", Some(&bytes)).await?;
    select(provider, Selection::Keyring).await
}

pub async fn save_key(provider: Provider, key: String) -> Result<(), ProviderError> {
    timeout(Duration::from_secs(30), async {
        let _guard = WRITERS[provider_index(provider)].lock().await;
        if provider == Provider::Claude {
            return Err(error(
                "Claude subscription quota requires browser sign-in or a subscription CLI login.",
            ));
        }
        if !valid_secret(&key) || key.chars().any(char::is_whitespace) {
            return Err(error("API key is empty or malformed."));
        }
        store(
            provider,
            &Credential {
                kind: CredentialKind::ApiKey,
                secret: key,
                account_id: None,
                plan_hint: None,
                source: "App key (Secret Service)".into(),
                expires_at: None,
            },
        )
        .await
    })
    .await
    .map_err(|_| {
        error("Secure credential storage timed out. Unlock your desktop keyring and try again.")
    })?
}

pub async fn save_cookie(
    provider: Provider,
    cookie: String,
    account_id: Option<String>,
) -> Result<(), ProviderError> {
    timeout(Duration::from_secs(30), async {
        let _guard = WRITERS[provider_index(provider)].lock().await;
        if !matches!(provider, Provider::Claude | Provider::OpenCodeZen) {
            return Err(error(
                "Session cookies are supported only for Claude and OpenCode Zen.",
            ));
        }
        if !valid_secret(&cookie) || !cookie.contains('=') {
            return Err(error("Session cookie is empty or malformed."));
        }
        if account_id.as_deref().is_some_and(|id| {
            id.is_empty()
                || id.len() > 256
                || id.chars().any(|c| c.is_control() || c.is_whitespace())
        }) {
            return Err(error("Workspace or organization identifier is malformed."));
        }
        store(
            provider,
            &Credential {
                kind: CredentialKind::Cookie,
                secret: cookie,
                account_id,
                plan_hint: None,
                source: "App session (Secret Service)".into(),
                expires_at: None,
            },
        )
        .await
    })
    .await
    .map_err(|_| {
        error("Secure session storage timed out. Unlock your desktop keyring and try again.")
    })?
}

pub async fn disconnect(provider: Provider) -> Result<(), ProviderError> {
    timeout(Duration::from_secs(30), async {
        let _guard = WRITERS[provider_index(provider)].lock().await;
        // Persist opt-out before deleting: failed/locked keyring cleanup must not reconnect.
        select(provider, Selection::Disconnected).await?;
        secret_tool(provider, "clear", None).await?;
        Ok(())
    })
    .await
    .map_err(|_| error("Disconnect timed out. Try again."))?
}

async fn executable(provider: Provider) -> Result<PathBuf, ProviderError> {
    let name = if provider == Provider::Claude {
        "claude"
    } else {
        "codex"
    };
    let home = home()?;
    let mut candidates = vec![
        home.join(".local/bin").join(name),
        home.join(".npm-global/bin").join(name),
    ];
    if provider == Provider::OpenAi {
        // Desktop sessions commonly omit nvm's Node installation from PATH.
        let versions = env_path("NVM_DIR")
            .unwrap_or(home.join(".nvm"))
            .join("versions/node");
        if let Ok(mut entries) = fs::read_dir(versions).await {
            let mut bins = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                bins.push(entry.path().join("bin/codex"));
            }
            bins.sort();
            candidates.extend(bins.into_iter().rev());
        }
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(
            env::split_paths(&path)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(name)),
        );
    }
    candidates.extend([
        PathBuf::from("/usr/local/bin").join(name),
        PathBuf::from("/usr/bin").join(name),
    ]);
    for path in candidates {
        if fs::metadata(&path)
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return Ok(path);
        }
    }
    Err(error(if provider == Provider::Claude {
        "Install the official Claude CLI to use browser sign-in, or connect an existing OpenCode login."
    } else {
        "Install the official Codex CLI to use browser sign-in, or connect an existing OpenCode login."
    }))
}

struct LoginHome(PathBuf, Option<i32>);

impl Drop for LoginHome {
    fn drop(&mut self) {
        if let Some(group) = self.1.filter(|group| *group > 0) {
            // SAFETY: spawn creates this dedicated process group; it is disarmed immediately
            // after wait reaps its leader. Never signal group 0 or a borrowed CLI process.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        if self.0.as_os_str().is_empty() {
            return;
        }
        // Cancellation kills the CLI via kill_on_drop and also removes its transient files.
        let path = self.0.clone();
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(10), fs::remove_dir_all(path)).await;
        });
    }
}

async fn cli_login(provider: Provider) -> Result<Credential, ProviderError> {
    let executable = executable(provider).await?;
    let root = config_root()?;
    private_dir(&root).await?;
    let mut isolated = LoginHome(
        root.join(format!(
            ".login-{}-{:032x}",
            provider.id(),
            rand::random::<u128>()
        )),
        None,
    );
    private_dir(&isolated.0).await?;
    let codex_home = isolated.0.join("codex");
    let claude_home = isolated.0.join("claude");
    private_dir(&codex_home).await?;
    private_dir(&claude_home).await?;
    let mut command = Command::new(&executable);
    command
        .current_dir(&isolated.0)
        .env("HOME", &isolated.0)
        .env("CODEX_HOME", &codex_home)
        .env("CLAUDE_CONFIG_DIR", &claude_home)
        .env("XDG_CONFIG_HOME", isolated.0.join("config"))
        .env("XDG_DATA_HOME", isolated.0.join("data"))
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DO_NOT_TRACK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
        "CLAUDE_CODE_OAUTH_SCOPES",
        "CLAUDE_CODE_TOKEN",
        "ANTHROPIC_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "CODEX_ACCESS_TOKEN",
        "CODEX_AUTH_JSON",
        "CODEX_API_KEY",
        "CHATGPT_BASE_URL",
        "ANTHROPIC_PROFILE",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        command.env_remove(name);
    }
    for (name, _) in env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("OTEL_") || name == "CLAUDE_CODE_ENABLE_TELEMETRY")
        {
            command.env_remove(name);
        }
    }
    let mut paths = vec![
        executable
            .parent()
            .unwrap_or(Path::new("/usr/bin"))
            .to_owned(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ];
    if let Some(path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&path).filter(|p| p.is_absolute()));
    }
    command.env(
        "PATH",
        env::join_paths(paths)
            .map_err(|_| error("Could not construct CLI executable search path."))?,
    );
    if provider == Provider::OpenAi {
        // Explicit file storage avoids Codex's CLI-global keyring account being overwritten.
        command.args([
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "-c",
            "forced_login_method=\"chatgpt\"",
            "-c",
            "analytics.enabled=false",
            "login",
        ]);
    } else {
        command.args(["auth", "login", "--claudeai"]);
    }
    command.process_group(0);
    let mut child = command.spawn().map_err(|_| error("Could not start official CLI browser sign-in. Update the CLI or connect an existing login."))?;
    isolated.1 = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .filter(|id| *id > 0);
    if isolated.1.is_none() {
        return Err(error(
            "Could not track isolated CLI process for cancellation.",
        ));
    }
    let status = child
        .wait()
        .await
        .map_err(|_| error("Official CLI browser sign-in failed."))?;
    isolated.1 = None;
    if !status.success() {
        return Err(error(
            "Official CLI browser sign-in did not complete. Update the CLI or sign in from a terminal and connect that existing login.",
        ));
    }
    let path = if provider == Provider::OpenAi {
        codex_home.join("auth.json")
    } else {
        claude_home.join(".credentials.json")
    };
    let bytes = read_bytes(&path).await?.ok_or_else(|| {
        error("CLI produced no isolated credential. Update the CLI or connect an existing login.")
    })?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| error("CLI produced an unsupported credential file."))?;
    let source = "App browser login (Secret Service)";
    let credential = if provider == Provider::OpenAi {
        codex(&value, source)?
    } else {
        claude(&value, source)?
    };
    let credential = credential.ok_or_else(|| {
        error("CLI produced no subscription OAuth credential. Use a subscription account.")
    })?;
    timeout(Duration::from_secs(10), fs::remove_dir_all(&isolated.0)).await
        .map_err(|_| error("Removing transient CLI credentials timed out. Check private app directory permissions."))?
        .map_err(|_| error("Could not remove transient CLI credentials. Check private app directory permissions before reconnecting."))?;
    isolated.0.clear();
    Ok(credential)
}

pub async fn login(provider: Provider) -> Result<(), ProviderError> {
    timeout(Duration::from_secs(300), async {
        let _guard = WRITERS[provider_index(provider)].lock().await;
        if !fs::metadata("/usr/bin/secret-tool").await.is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) { return Err(error(STORE_ERROR)); }
        let credential = match provider {
            Provider::Claude | Provider::OpenAi => cli_login(provider).await?,
            Provider::OpenRouter => Credential { kind: CredentialKind::ApiKey, secret: crate::oauth::openrouter().await?, account_id: None, plan_hint: None, source: "App browser login (Secret Service)".into(), expires_at: None },
            _ => return Err(error("This provider uses an existing OpenCode login, API key, or Zen session cookie; browser login is not available here.")),
        };
        store(provider, &credential).await
    }).await.map_err(|_| error("Browser sign-in timed out after 5 minutes. Reconnect to start a new sign-in."))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_source_parses_milliseconds_without_refresh_secret() {
        let input = json!({"claudeAiOauth":{"accessToken":"synthetic-access", "refreshToken":"never-import", "expiresAt": 2000000000123i64, "subscriptionType":"max"}});
        let parsed = claude(&input, "test").unwrap().unwrap();
        assert_eq!(parsed.expires_at, Some(2000000000));
        assert_eq!(parsed.secret, "synthetic-access");
        assert_eq!(parsed.plan_hint.as_deref(), Some("max"));
    }

    #[test]
    fn opencode_selects_only_requested_provider_and_kind() {
        let input = json!({"anthropic":{"type":"oauth", "access":"claude-token", "expires":2000000000000i64}, "openai":{"type":"api", "key":"api-key"}, "opencode-go":{"type":"api","key":"go-key"}});
        assert_eq!(
            opencode(Provider::Claude, &input).unwrap().unwrap().secret,
            "claude-token"
        );
        assert!(opencode(Provider::OpenAi, &input).is_err());
        assert_eq!(
            opencode(Provider::OpenCodeGo, &input)
                .unwrap()
                .unwrap()
                .secret,
            "go-key"
        );
        assert!(opencode(Provider::OpenRouter, &input).unwrap().is_none());
    }

    #[test]
    fn codex_uses_selected_account_and_access_expiry() {
        let claims = URL_SAFE_NO_PAD.encode(br#"{"exp":2000000000,"https://api.openai.com/auth":{"chatgpt_account_id":"hint-account"}}"#);
        let input = json!({"tokens":{"access_token":format!("header.{claims}.signature"), "account_id":"selected-account"}});
        let parsed = codex(&input, "test").unwrap().unwrap();
        assert_eq!(parsed.account_id.as_deref(), Some("selected-account"));
        assert_eq!(parsed.expires_at, Some(2000000000));
    }

    #[test]
    fn malformed_credentials_do_not_echo_secrets() {
        let input = json!({"tokens":{"access_token":"secret\r\ninjected"}});
        let failure = codex(&input, "test").err().unwrap();
        assert!(!failure.0.contains("secret"));
        assert!(!failure.0.contains("injected"));
        assert!(jwt_claims("header.payload.signature.extra").is_none());
    }
}
