// SPDX-License-Identifier: MPL-2.0

use crate::{model::ProviderError, providers};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::TryRngCore;
use sha2::{Digest, Sha256};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
    time::timeout,
};
use url::Url;

const CALLBACK_PATH: &str = "/callback";
const MAX_REQUEST_BYTES: usize = 8192;

fn error(message: &str) -> ProviderError {
    ProviderError(message.into())
}

fn random_token() -> Result<String, ProviderError> {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| error("Could not obtain secure randomness for browser sign-in."))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn same_state(actual: &str, expected: &str) -> bool {
    actual.len() == expected.len()
        && actual
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

// This parser returns only the authorization code, never an error containing the request.
fn callback_code(request: &[u8], expected_host: &str, state: &str) -> Result<String, ()> {
    if request.len() > MAX_REQUEST_BYTES || !request.ends_with(b"\r\n\r\n") {
        return Err(());
    }
    let request = std::str::from_utf8(request).map_err(|_| ())?;
    let mut lines = request[..request.len() - 4].split("\r\n");
    let mut first = lines.next().ok_or(())?.split(' ');
    if first.next() != Some("GET") {
        return Err(());
    }
    let target = first.next().ok_or(())?;
    if first.next() != Some("HTTP/1.1")
        || first.next().is_some()
        || !target.starts_with("/callback?")
        || target.contains('#')
    {
        return Err(());
    }
    let mut host = None;
    for line in lines {
        if line.starts_with(' ')
            || line.starts_with('\t')
            || line.chars().any(|c| c.is_control() && c != '\t')
        {
            return Err(());
        }
        let (name, value) = line.split_once(':').ok_or(())?;
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err(());
        }
        if name.eq_ignore_ascii_case("host") {
            if host.replace(value.trim()).is_some() {
                return Err(());
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            return Err(());
        }
    }
    if host != Some(expected_host) {
        return Err(());
    }
    let url = Url::parse(&format!("http://{expected_host}{target}")).map_err(|_| ())?;
    if url.path() != CALLBACK_PATH
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(());
    }
    let mut code = None;
    let mut received_state = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" => {
                if code.replace(value.into_owned()).is_some() {
                    return Err(());
                }
            }
            "state" => {
                if received_state.replace(value.into_owned()).is_some() {
                    return Err(());
                }
            }
            _ => return Err(()),
        }
    }
    let received_state = received_state.ok_or(())?;
    if !same_state(&received_state, state) {
        return Err(());
    }
    let code = code.ok_or(())?;
    if code.is_empty()
        || code.len() > 4096
        || code.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(());
    }
    Ok(code)
}

async fn read_request(stream: &mut TcpStream) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let size = stream.read(&mut chunk).await.map_err(|_| ())?;
        if size == 0 || bytes.len() + size > MAX_REQUEST_BYTES {
            return Err(());
        }
        bytes.extend_from_slice(&chunk[..size]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            // No body or pipelined second request is accepted.
            return if bytes.ends_with(b"\r\n\r\n") {
                Ok(bytes)
            } else {
                Err(())
            };
        }
    }
}

async fn reply(stream: &mut TcpStream, accepted: bool) {
    let (status, body) = if accepted {
        (
            "200 OK",
            "Authorization received. Return to AI Quota to check secure sign-in completion. You can close this tab.",
        )
    } else {
        (
            "400 Bad Request",
            "Invalid sign-in callback. Return to AI Quota and reconnect if necessary.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\r\n{body}",
        body.len()
    );
    let _ = timeout(Duration::from_secs(2), async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    })
    .await;
}

async fn receive_code(
    listener: &TcpListener,
    host: &str,
    state: &str,
) -> Result<String, ProviderError> {
    loop {
        let (mut stream, peer) = listener
            .accept()
            .await
            .map_err(|_| error("Could not receive browser callback. Reconnect to retry."))?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let code = match timeout(Duration::from_secs(5), read_request(&mut stream)).await {
            Ok(Ok(bytes)) => callback_code(&bytes, host, state),
            _ => Err(()),
        };
        reply(&mut stream, code.is_ok()).await;
        if let Ok(code) = code {
            return Ok(code);
        }
    }
}

async fn open_browser(url: &Url) -> Result<(), ProviderError> {
    let mut child = Command::new("/usr/bin/xdg-open")
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            error("Could not open your browser. Install xdg-utils and configure a default browser.")
        })?;
    let status = timeout(Duration::from_secs(15), child.wait())
        .await
        .map_err(|_| error("Opening your browser timed out. Reconnect to try again."))?
        .map_err(|_| error("Could not open your browser."))?;
    if !status.success() {
        return Err(error(
            "Default browser could not be opened. Configure a default browser, then reconnect.",
        ));
    }
    Ok(())
}

pub async fn openrouter() -> Result<String, ProviderError> {
    timeout(Duration::from_secs(300), async {
        let verifier = random_token()?;
        let state = random_token()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await
            .map_err(|_| error("Could not bind a local browser callback port."))?;
        let port = listener.local_addr().map_err(|_| error("Could not inspect browser callback port."))?.port();
        let host = format!("localhost:{port}");
        let callback = format!("http://{host}{CALLBACK_PATH}");
        let mut authorize = Url::parse("https://openrouter.ai/auth").map_err(|_| error("Invalid official authorization endpoint."))?;
        authorize.query_pairs_mut().append_pair("callback_url", &callback)
            .append_pair("code_challenge", &challenge).append_pair("code_challenge_method", "S256")
            .append_pair("state", &state).append_pair("key_label", "COSMIC AI Quota");
        open_browser(&authorize).await?;
        let code = receive_code(&listener, &host, &state).await?;
        // Closing the listener before exchange rejects replay; verifier remains alive through exchange.
        drop(listener);
        let client = providers::client()?;
        let response = providers::json(client.post("https://openrouter.ai/api/v1/auth/keys")
            .json(&serde_json::json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"}))).await?;
        let key = response["key"].as_str().filter(|key| !key.is_empty() && key.len() <= 64 * 1024 && !key.chars().any(|c| c.is_control() || c.is_whitespace()))
            .ok_or_else(|| error("OpenRouter returned no valid key. Reconnect to authorize again."))?;
        Ok(key.to_owned())
    }).await.map_err(|_| error("Browser sign-in timed out after 5 minutes. Reconnect to start again."))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(target: &str, headers: &str) -> Vec<u8> {
        format!("GET {target} HTTP/1.1\r\n{headers}\r\n\r\n").into_bytes()
    }

    #[test]
    fn callback_accepts_only_bound_host_path_state_and_one_code() {
        let parsed = callback_code(
            &request(
                "/callback?state=expected&code=synthetic-code",
                "Host: localhost:43123",
            ),
            "localhost:43123",
            "expected",
        );
        assert_eq!(parsed.unwrap(), "synthetic-code");
        for target in [
            "/other?state=expected&code=x",
            "/callback/extra?state=expected&code=x",
            "/callback?state=wrong&code=x",
            "/callback?state=expected&state=expected&code=x",
            "/callback?state=expected&code=x&code=y",
            "/callback?state=expected&code=x#fragment",
            "http://localhost:43123/callback?state=expected&code=x",
            "/callback?state=expected&code=",
            "/callback?state=expected&code=x%0A",
        ] {
            assert!(
                callback_code(
                    &request(target, "Host: localhost:43123"),
                    "localhost:43123",
                    "expected"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn callback_rejects_rebinding_duplicate_host_and_body() {
        for headers in [
            "Host: attacker.example",
            "Host: localhost:43124",
            "Host: localhost:43123\r\nHost: localhost:43123",
            "Host: localhost:43123\r\nContent-Length: 1",
            "Host: localhost:43123\r\nTransfer-Encoding: chunked",
            "Host: localhost:43123\r\n Folded: value",
        ] {
            assert!(
                callback_code(
                    &request("/callback?state=expected&code=x", headers),
                    "localhost:43123",
                    "expected"
                )
                .is_err()
            );
        }
        let mut body = request("/callback?state=expected&code=x", "Host: localhost:43123");
        body.extend_from_slice(b"body");
        assert!(callback_code(&body, "localhost:43123", "expected").is_err());
    }

    #[test]
    fn callback_rejects_method_version_and_oversized_requests() {
        for first in [
            "POST /callback?state=expected&code=x HTTP/1.1",
            "GET /callback?state=expected&code=x HTTP/1.0",
            "GET /callback?state=expected&code=x HTTP/1.1 extra",
        ] {
            let bytes = format!("{first}\r\nHost: localhost:43123\r\n\r\n");
            assert!(callback_code(bytes.as_bytes(), "localhost:43123", "expected").is_err());
        }
        let oversized = request(
            "/callback?state=expected&code=x",
            &format!(
                "Host: localhost:43123\r\nX-Padding: {}",
                "a".repeat(MAX_REQUEST_BYTES)
            ),
        );
        assert!(callback_code(&oversized, "localhost:43123", "expected").is_err());
    }

    #[test]
    fn pkce_uses_rfc7636_s256_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
