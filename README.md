# Cosmos AI Quota

Show quota and balance data for Claude, OpenAI, OpenCode Go, OpenCode Zen, and OpenRouter in the COSMIC panel.
Automatic refresh runs at most once every 15 minutes for each provider. Use **Refresh** for a manual update.
Missing data shows **Unavailable**, not zero. The applet makes no inference requests and spends no tokens to measure usage.
Cosmos is independent of System76. COSMIC libraries and protocols keep their upstream names.

## Requirements

Use Linux with a working COSMIC desktop session. Install the native build packages for your distribution:

```sh
# Fedora
sudo dnf install gcc gcc-c++ cmake git pkgconf-pkg-config just desktop-file-utils expat-devel fontconfig-devel freetype-devel libxkbcommon-devel wayland-devel libsecret xdg-utils

# Debian 13 or newer
sudo apt update
sudo apt install build-essential cmake git pkgconf just desktop-file-utils libexpat1-dev libfontconfig-dev libfreetype-dev libxkbcommon-dev libwayland-dev libsecret-tools xdg-utils

# Arch Linux
sudo pacman -Syu --needed base-devel cmake git pkgconf just desktop-file-utils expat fontconfig freetype2 libxkbcommon wayland libsecret xdg-utils
```

The lists follow [libcosmic's native requirements](https://github.com/pop-os/libcosmic#dependencies) and distribution package metadata.
These are package examples, not a claim that all three distributions were tested.
`secret-tool` comes from Fedora/Arch `libsecret` and Debian `libsecret-tools`.
Run an unlocked Secret Service keyring in your desktop session, such as GNOME Keyring. Installing `secret-tool` alone does not provide one.
Browser sign-in needs a browser. Claude and OpenAI sign-in also need their installed official CLI: Claude Code or Codex.

Install current stable Rust with [rustup](https://rustup.rs), as your normal user. Distribution Rust packages can be too old.
After rustup setup, open a new terminal and run:

```sh
rustup update stable
rustup default stable
```

## Build and install

Use `sudo` only for package prerequisites above, never for these build or install commands:

```sh
git clone https://github.com/petterssonjonas/cosmos-applet-ai-quota
cd cosmos-applet-ai-quota
just build-release
just install
```

Build and install are separate. `just install` uses the existing release build.
The executable goes to `~/.local/bin/cosmos-applet-ai-quota`. The desktop entry and icon go to `~/.local/share`.
The desktop entry uses an absolute executable path. Keep `CARGO_TARGET_DIR` the same for build and install if you set it.
In a development container, use your host-mapped normal user, not root.

Open **COSMIC Settings → Desktop → Panel → Configure panel applets**. Add **Cosmos AI Quota** (`dev.jp.CosmosAppletAiQuota`).
Remove and add the applet again after an upgrade. Installation does not change the panel layout.

## Connect and use

Nothing connects automatically. Open the dropdown, select **Connect**, then select a source:

- Claude: existing Claude/OpenCode OAuth login, official CLI browser sign-in, or a supplied dashboard cookie.
- OpenAI: existing Codex/OpenCode OAuth login, official CLI browser sign-in, or an API key.
- OpenCode Go: API key or existing OpenCode credentials.
- OpenCode Zen: Console service key, legacy key, existing OpenCode credentials, or a supplied Console cookie.
- OpenRouter: API key, existing OpenCode credentials, or browser PKCE authorization.

Existing login files are read-only. Sources are `$CODEX_HOME/auth.json` or `~/.codex/auth.json`,
`$CLAUDE_CONFIG_DIR/.credentials.json` or `~/.claude/.credentials.json`, and
`$XDG_DATA_HOME/opencode/auth.json` or `~/.local/share/opencode/auth.json`.
For the selected OpenCode source, `OPENCODE_AUTH_CONTENT` takes precedence.
A Codex login stored only in Codex's own keyring cannot be imported from a file.
The applet does not scrape browser cookies or other desktop credential stores.

App-owned keys, cookies, and access tokens use Secret Service. There is no plaintext fallback.
Source preferences use `~/.config/cosmos-applet-ai-quota` (or the corresponding `$XDG_CONFIG_HOME` directory).
Browser sign-in uses isolated temporary CLI configuration and removes temporary credentials after import. It times out after five minutes.
Existing credentials need renewal by their owning CLI. App-owned browser tokens need another sign-in when they expire; refresh tokens are not retained.
If the CLI needs terminal input, sign in from a terminal and select the existing login instead.

**Disconnect** removes only this applet's provider credential. If deletion fails, unlock the keyring and retry.
It does not log out another application or revoke a provider-side key. Revoke keys in the provider dashboard when needed.

### Limits

- OpenAI API keys do not expose ChatGPT/Codex subscription quota or a reliable API prepaid balance. Use subscription OAuth for subscription usage.
- Claude credit and reset fields depend on the selected source and account. Extra-usage allowance is not prepaid cash.
- OpenCode Console billing needs a service key with **All** permissions, or a supplied Console cookie. Select a workspace ID when multiple workspaces exist.
- OpenRouter key limits and account credits are different. Account credits can require additional permissions.
- Some dashboard APIs are private and can change. Refresh intervals do not guarantee that a provider permits access.
- Provider access remains account-dependent. Not every provider and browser authorization flow has live-account validation.

## Upgrade from cosmic-applet-ai-quota

Stop the old applet and remove its **AI Quota** panel entry before migration. Do not start the new applet yet.
Run `just migrate` to copy only source preferences from `~/.config/cosmic-applet-ai-quota` to `~/.config/cosmos-applet-ai-quota`.
The paths follow `$XDG_CONFIG_HOME` when set. Migration keeps the source, refuses an existing destination, and rejects symlinks or untrusted paths.
Unexpected files, including `.login-*` directories, stop migration. Inspect these privately before retrying; they can contain credentials.

**Reconnect every app-managed provider. Credentials do not migrate between keyring namespaces.**
Old items use `application=cosmic-applet-ai-quota`; new items use `application=cosmos-applet-ai-quota`, with the same `provider` attributes.
Old items remain untouched. A migrated `keyring` selection shows a reconnect error until you reconnect.
Migration never exports or copies legacy credentials. New **Disconnect** affects only the new namespace.
Keep old items as backups, or remove them with your keyring manager after the new connection works.
Read-only CLI/OpenCode sources continue to use their original files. Disconnected sources stay disconnected.

Build, install, then add the new panel entry. Remove the old executable, desktop entry, and icon separately under the old prefix:
`bin/cosmic-applet-ai-quota`, `share/applications/io.github.cosmic-applet-ai-quota.desktop`, and
`share/icons/hicolor/scalable/apps/io.github.cosmic-applet-ai-quota.svg`.

## Uninstall

Disconnect app-managed providers first if you want to remove their stored credentials.
Remove the applet from the panel, then run `just uninstall` from this source directory, without `sudo`.
Uninstall keeps preferences and keyring items. It does not remove legacy files.

## License and references

Code: MPL-2.0, as declared in `Cargo.toml`.
Built with [libcosmic](https://github.com/pop-os/libcosmic) and the [COSMIC applet template](https://github.com/pop-os/cosmic-applet-template).
Provider references include [CodexBar](https://github.com/steipete/CodexBar),
[OpenCode Console](https://github.com/anomalyco/opencode/blob/dev/services/www/src/docs/content/console/api/index.mdx),
and [OpenRouter PKCE](https://openrouter.ai/docs/guides/overview/auth/oauth).
