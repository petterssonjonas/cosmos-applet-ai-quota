name := 'cosmos-applet-ai-quota'
appid := 'dev.jp.CosmosAppletAiQuota'
profile := 'release'
prefix := env('HOME') / '.local'
rootdir := ''
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')
base-dir := absolute_path(clean(rootdir / prefix))
target-profile := if profile == 'debug' { 'debug' } else if profile == 'release' { 'release' } else { error('profile must be debug or release') }
config-home := if env('XDG_CONFIG_HOME', '') == '' { env('HOME') / '.config' } else { env('XDG_CONFIG_HOME') }

[default]
default: build-release

build-debug:
    cargo build --locked --target-dir {{quote(cargo-target-dir)}}

build-release:
    cargo build --release --locked --target-dir {{quote(cargo-target-dir)}}

check:
    cargo clippy --all-targets --locked --target-dir {{quote(cargo-target-dir)}} -- -D warnings

test:
    cargo test --locked --target-dir {{quote(cargo-target-dir)}}

# The COSMIC panel normally launches this applet with its panel environment.
[positional-arguments]
run *args:
    @printf '%s\n' 'Development launch only: add Cosmos AI Quota through COSMIC panel settings for normal use.'
    cargo run --locked --target-dir {{quote(cargo-target-dir)}} --profile {{if target-profile == 'debug' { 'dev' } else { 'release' }}} -- "$@"

# Build separately; installation never changes panel settings or credentials.
install: (_install-file (cargo-target-dir / target-profile / name) (base-dir / 'bin' / name) '0755') (_install-desktop 'resources/app.desktop' (base-dir / 'share/applications' / appid + '.desktop') (absolute_path(prefix / 'bin' / name))) (_install-file 'resources/icon.svg' (base-dir / 'share/icons/hicolor/scalable/apps' / appid + '.svg') '0644')

# Remove the panel entry first. Preferences and keyring items are retained.
uninstall:
    rm -f -- {{quote(base-dir / 'bin' / name)}} {{quote(base-dir / 'share/applications' / appid + '.desktop')}} {{quote(base-dir / 'share/icons/hicolor/scalable/apps' / appid + '.svg')}}

# Copy token-free source preferences only. Legacy Secret Service items stay untouched.
migrate: _migration-preflight (_migrate-directory (config-home / 'cosmic-applet-ai-quota') (config-home / name))
    @printf '%s\n' 'App-managed credentials are not migrated. Reconnect providers using browser sign-in, API key, or session cookie. Legacy keyring items remain untouched. Read-only CLI/OpenCode sources continue to work.'

# An interrupted legacy CLI login can leave transient credentials under .login-*.
# Refuse those directories rather than copying credentials through ordinary migration.
_migration-preflight:
    #!/usr/bin/env bash
    set -euo pipefail
    source={{quote(config-home / 'cosmic-applet-ai-quota')}}
    [[ -e "$source" || -L "$source" ]] || exit 0
    [[ -d "$source" && ! -L "$source" ]] || { printf '%s\n' 'Legacy preferences must be a real directory.' >&2; exit 1; }
    shopt -s nullglob dotglob
    for entry in "$source"/*; do
        case "${entry##*/}" in
            claude.json|openai.json|opencode-go.json|opencode-zen.json|openrouter.json)
                [[ -f "$entry" && ! -L "$entry" ]] || { printf '%s\n' 'Legacy provider preferences must be regular files.' >&2; exit 1; }
                ;;
            *) printf '%s\n' 'Legacy directory contains non-preference files. Stop the old applet and inspect leftover .login-* or temporary files before retrying; no data was migrated.' >&2; exit 1 ;;
        esac
    done
import 'recipes/common.just'

