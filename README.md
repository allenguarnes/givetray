# givetray

`givetray` is a Linux system-tray utility that wraps terminal commands into named profiles.
Each profile can run one command, stream logs, and manage desktop launcher entries.

![givetray icon](assets/icon.png)

## Quick Start

```bash
# Run with a required profile name
cargo run -- -c scrcpy

# Or with release binary
./target/release/givetray -c scrcpy
```

On first run, `givetray` creates the profile config if it does not exist.
Open `Configuration` from the tray menu and set your actual command.

## Dependencies (Linux)

This app uses GTK for windows and AppIndicator for tray integration.

Debian/Ubuntu:

```bash
sudo apt install libgtk-3-dev libxdo-dev libappindicator3-dev
```

Arch/Manjaro:

```bash
sudo pacman -S gtk3 xdotool libappindicator-gtk3
```

## Install

Install from source:

```bash
cargo install --path .
givetray -c default
```

Or build manually:

```bash
cargo build --release
./target/release/givetray -c default
```

## CLI Usage

`-c/--config PROFILE` is required for app mode and desktop-file mode.

```bash
givetray -c PROFILE [--icon ICON_PATH] [--log-file LOG_PATH]
givetray desktop-file -c PROFILE [--output-dir DIR] [--autostart] [--icon ICON_PATH] [--log-file LOG_PATH]
givetray --help
givetray --version
```

Examples:

```bash
# Run profile
givetray -c scrcpy

# Set profile icon (copied into givetray-managed storage)
givetray -c scrcpy --icon /path/to/icon.png

# Enable file logging for this profile
givetray -c scrcpy --log-file ~/.local/share/givetray/logs/scrcpy.log

# Create Applications desktop entry
givetray desktop-file -c scrcpy

# Create autostart desktop entry
givetray desktop-file -c scrcpy --autostart

# Create desktop entry in custom directory
givetray desktop-file -c scrcpy --output-dir /tmp
```

## Profiles

- Profiles are independent configs so multiple instances can run at once.
- Config path: `~/.config/givetray/configs/<profile>.toml`

## Desktop Entries

- Desktop filename format: `givetray_<profile>.desktop`
- Applications location: `~/.local/share/applications`
- Autostart location: `~/.config/autostart`
- CLI default writes to Applications location.
- `--autostart` switches default target to autostart location.
- Configuration window toggles can create/remove entries in both locations.

## Sudo Commands

If the configured command starts with `sudo`, `givetray` prompts for password on each Start.
The password is passed to `sudo` via stdin (`sudo -S`) and is not stored in config.

## Runtime Behavior

- Tray menu order: Start/Stop, Logs, Configuration, About, separator, Exit.
- One running child process is tracked per app instance.
- Logs are buffered in memory (`MAX_LOG_LINES`) and can also be written to file.
- On stop, the process receives `SIGTERM` before a timed kill fallback.

## GUI Features

### Tray Menu

- `Start/Stop`: starts or stops the configured command for the active profile.
- `Logs`: opens a live log window.
- `Configuration`: opens profile configuration controls.
- `About`: opens project information and support links.
- `Exit`: stops any running command for this instance and exits.

### Logs Window

- Live stdout/stderr stream from the running command.
- In-memory rolling buffer with line count indicator.
- `Copy All` button to copy all log text to clipboard.
- `Clear` button to clear the in-app log buffer.
- Optional file logging (if enabled for profile).

### Configuration Window

- Editable command/script text field for the profile.
- `Run command automatically when givetray launches` toggle.
- `Write logs to file` toggle.
- `Create Applications launcher (.desktop)` toggle.
- `Enable desktop session autostart (.config/autostart)` toggle.
- Save status (`Saved` / `Unsaved changes`) and unsaved-change close prompt.

## Development

```bash
cargo build
cargo fmt --all
cargo clippy --all-targets --all-features
cargo test
```

## Support

If `givetray` helps your workflow, you can support development:

https://buymeacoffee.com/allenguarnes

## License

Licensed under either of:

- MIT license (`LICENSE-MIT`)
- Apache License, Version 2.0 (`LICENSE-APACHE`)

at your option.
