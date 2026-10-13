name := 'cosmic-applet-ai-quota'
appid := 'io.github.cosmic-applet-ai-quota'
prefix := env('HOME') / '.local'
rootdir := ''
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')
base-dir := absolute_path(clean(rootdir / prefix))

default: build-release

build-release:
    cargo build --release --locked

check:
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked

# Development only; the desktop panel launches the installed applet.
run:
    cargo run --locked

install:
    install -Dm0755 {{cargo-target-dir / 'release' / name}} {{base-dir / 'bin' / name}}
    install -Dm0644 resources/app.desktop {{base-dir / 'share/applications' / appid + '.desktop'}}
    desktop-file-edit --set-key=Exec --set-value='"{{absolute_path(prefix / 'bin' / name)}}"' {{base-dir / 'share/applications' / appid + '.desktop'}}
    install -Dm0644 resources/icon.svg {{base-dir / 'share/icons/hicolor/scalable/apps' / appid + '.svg'}}

# Remove the panel entry first. Saved connections remain until disconnected in the applet.
uninstall:
    rm -f {{base-dir / 'bin' / name}} {{base-dir / 'share/applications' / appid + '.desktop'}} {{base-dir / 'share/icons/hicolor/scalable/apps' / appid + '.svg'}}
