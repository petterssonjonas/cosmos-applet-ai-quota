# COSMIC AI Quota Applet

A native COSMIC panel applet for Claude, OpenAI, OpenCode Go, OpenCode Zen, and OpenRouter. The panel icon is a theme-aware **Q enclosing AI**; its tooltip is **AI Quota**.

Click the icon to open the quota dropdown. Each provider has a **Connect** button. Automatic quota refresh is limited to once per **15 minutes per provider**, with a **Refresh** button for manual updates. A native animated spinner replaces the provider's last-updated timestamp while a refresh is running. Concurrent refreshes of the same provider are suppressed. Opening the dropdown does not bypass the automatic refresh interval.

## Build and install

Requirements: a current Rust toolchain, the [libcosmic build dependencies](https://github.com/pop-os/libcosmic), `just`, `desktop-file-utils`, and a working Linux Secret Service keyring with `/usr/bin/secret-tool` (libsecret). Browser sign-in also needs `xdg-utils`; Claude and OpenAI sign-in require their official CLI.

```sh
cargo build --release --locked
just install
```

The default install prefix is `~/.local`. Installation writes the executable, desktop entry, and icon there; the installed desktop entry uses the absolute executable path, so the panel does not need `~/.local/bin` in its `PATH`.

Add **AI Quota** in **COSMIC Settings → Desktop → Panel → Configure panel applets**. The desktop ID is `io.github.cosmic-applet-ai-quota`. `just install` itself does not change the panel layout. To load an updated executable, remove and re-add the applet.

```sh
cargo test --release --locked
cargo clippy --release --all-targets --locked -- -D warnings
```

## Provider data

| Provider | Connection methods | Data, when returned by the provider |
| --- | --- | --- |
| Claude | Existing Claude/OpenCode OAuth login, isolated official CLI browser sign-in, or explicitly supplied dashboard cookie | Plan, session and weekly percentage remaining, model-scoped weekly limits including Fable, reset timestamps, available banked resets with individual expiry, extra-usage allowance and credits |
| OpenAI | Existing Codex/OpenCode OAuth login, isolated official CLI browser sign-in, or API key | Subscription plan, weekly and actual five-hour windows when present, banked resets with individual expiry, Codex credits |
| OpenCode Go | API key or existing OpenCode credentials | Rolling five-hour, weekly, and monthly quota windows returned by the Go usage API |
| OpenCode Zen | Console service API key, legacy API key, existing OpenCode credentials, or explicitly supplied Console cookie | Prepaid Console balance; available spending information is kept separate from quota |
| OpenRouter | API key, existing OpenCode credentials, or browser PKCE authorization | Key budget and remaining amount, reported spending windows, available account credits |

Missing data is **Unavailable**, never an invented zero. Zero is shown only when the provider reports exhaustion, no resets, or a zero balance. Expired banked resets are excluded. Reset/expiry timestamps use local time and include the UTC offset. Provider plan identifiers are displayed as reported rather than guessed from a billing tier.

### Important API distinctions

- **OpenAI API billing is not ChatGPT/Codex subscription quota.** An API key can be validated, but it cannot reveal a ChatGPT plan's weekly/five-hour quota or reliably expose an API prepaid balance through a supported public endpoint. Use Codex/ChatGPT OAuth for subscription usage.
- OpenAI window names come from their reported duration, not an assumed plan mapping. Five-hour windows are not fabricated for plans that do not report one.
- Claude OAuth plan information may come from the selected local credential and is labelled as such. Some credit/reset fields are available only through the dashboard session or particular account capabilities. Extra-usage spending allowance is not presented as prepaid cash.
- **OpenCode Console service keys need All permissions for billing.** Inference-only keys and legacy Zen keys may validate without exposing a balance. An explicitly supplied Console cookie is an alternative. Multiple workspaces require an explicit workspace ID; no workspace is silently chosen.
- OpenRouter key limits are not the same as the account's prepaid balance. Its account-credits endpoint may require additional permissions; key usage can still be shown if that request is denied.
- Some subscription/dashboard endpoints are private APIs and may change. The applet makes no inference calls and does not spend tokens to measure usage. A 15-minute interval reduces request frequency; it is not a guarantee against provider restrictions. Errors remain visible, with no rapid automatic retry loop.

## Credentials and privacy

Nothing is connected automatically on first launch. Choose **Connect → Use existing CLI login**, **Use existing OpenCode credentials**, **Sign in with browser**, or enter the appropriate API key.

Existing credentials are read-only and re-read on each refresh:

- Codex: `$CODEX_HOME/auth.json`, otherwise `~/.codex/auth.json`.
- Claude: `$CLAUDE_CONFIG_DIR/.credentials.json`, otherwise `~/.claude/.credentials.json`.
- OpenCode: `$XDG_DATA_HOME/opencode/auth.json`, otherwise `~/.local/share/opencode/auth.json`; an explicit `OPENCODE_AUTH_CONTENT` value takes precedence for that selected source.

The applet does not scrape browser cookies or credential stores belonging to desktop applications. Native/OpenCode OpenAI credential import expects subscription OAuth; enter API keys using the API-key field instead. A Codex login stored only in Codex's own keyring is not imported from a file: use an app-specific browser login or another supported source.

New app-owned API keys, session cookies, and OAuth access credentials are stored in **Secret Service**, never as plaintext configuration. There is no insecure fallback if the keyring is unavailable. Source-selection metadata contains no token and uses private directories/files under `$XDG_CONFIG_HOME/cosmic-applet-ai-quota` (normally `~/.config/cosmic-applet-ai-quota`).

Claude/OpenAI browser sign-in runs the installed official CLI in an isolated temporary home/configuration. It does not replace the CLI account you already use. Temporary credential files are removed after import. OpenRouter uses a bounded loopback callback with state verification and S256 PKCE. Browser sign-in times out after five minutes.

**Token renewal:** borrowed credentials are refreshed by their owning CLI, not this applet. App-owned browser access tokens currently require another browser sign-in when they expire; refresh tokens are not retained. If an official CLI cannot finish its browser flow without terminal input, sign in from a terminal and select that existing login instead.

**Disconnect** persists an opt-out and removes this applet's provider credential from Secret Service. It never logs out or edits another application's login. If the keyring is locked or deletion fails, unlock it and retry Disconnect; cleanup remains retryable. Local deletion does not revoke a provider-side API key—revoke that separately in the provider dashboard if required.

## Verification and API references

Release build, regression tests, strict Clippy, desktop-entry validation, and native COSMIC panel/popup checks were exercised locally. Secret Service storage/lookup/deletion was exercised with a temporary synthetic secret. A live OpenAI quota read using existing Codex credentials succeeded. The existing OpenCode Go key received HTTP 403 from its usage endpoint; the applet correctly reports that denial rather than inventing usage. Claude, Zen, and OpenRouter were not live-authenticated during these checks. Interactive provider browser authorization still requires the account owner to complete sign-in.

UI loading/form checks use a temporary, network-free harness; no smoke harness or sample quota data is shipped in the applet.

Implementation references:

- [COSMIC applet template](https://github.com/pop-os/cosmic-applet-template)
- [CodexBar provider integrations](https://github.com/steipete/CodexBar)
- [OpenCode Console API](https://github.com/anomalyco/opencode/blob/dev/services/www/src/docs/content/console/api/index.mdx)
- [OpenRouter OAuth PKCE](https://openrouter.ai/docs/guides/overview/auth/oauth)
