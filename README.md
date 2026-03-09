# givetray

`givetray` runs terminal commands from the Linux system tray using either named profiles or a temporary profile-free command.
Persistent profiles can run one saved command, show live logs, and manage desktop entries.

![givetray icon](assets/icon.png)

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

## Install and Run

From crates.io (recommended):

```bash
cargo install givetray
givetray -c default
```

From source with Cargo:

```bash
cargo install --path .
givetray -c default
```

Build manually:

```bash
cargo build --release
./target/release/givetray -c default
```

On first run, `givetray` creates `~/.config/givetray/configs/<profile>.toml`.
Then open `Configuration` from the tray menu and set your command/script.
When launched from a terminal, `givetray` detaches to the background and returns control to the shell.

Ephemeral mode launches immediately with `givetray -- <command...>`, does not use a profile, does not write config, keeps tray `Start/Stop` available for the temporary command, and hides `Configuration` in the tray menu.

## CLI Usage

`-c/--config PROFILE` is required for app mode and desktop-file mode.
Profile names support letters, numbers, `-`, and `_`.

```bash
givetray -c PROFILE [-cmd COMMAND|--command COMMAND] [--icon ICON_PATH] [--log-file LOG_PATH]
givetray -- <command...>
givetray desktop-file -c PROFILE [-cmd COMMAND|--command COMMAND] [--output-dir DIR] [--autostart] [--icon ICON_PATH]
givetray --help
givetray --version
```

Examples:

```bash
# Persistent profile mode
givetray -c scrcpy
givetray -c scrcpy -cmd "scrcpy --always-on-top -S -w"
givetray -c scrcpy --icon /path/to/icon.png
givetray -c scrcpy --log-file ~/.local/share/givetray/logs/scrcpy.log

# Ephemeral mode
givetray -- notify-send "Backup complete"
givetray -- sh -lc "while true; do date; sleep 60; done"

# Desktop file generation remains profile-based
givetray desktop-file -c scrcpy
givetray desktop-file -c scrcpy --autostart
```

When `-cmd/--command` is provided, the profile's saved command is overwritten.
Ephemeral mode is temporary and profile-free, so desktop files and saved configuration do not apply even though the tray can still `Start/Stop` the active command.

## Desktop Entries

- Desktop filename format: `givetray_<profile>.desktop`
- Applications location: `~/.local/share/applications`
- Autostart location: `~/.config/autostart`
- `desktop-file` writes to Applications by default
- `--autostart` switches default target to autostart
- Configuration toggles can create/remove entries in both locations

## GUI Features

### Tray Menu

- `Start/Stop`: run or stop the configured command
- `Logs`: open live log window
- `Configuration`: edit profile command and toggles in persistent profile mode only
- `About`: show app info and links
- `Exit`: stop current process and quit this instance

### Logs Window

- Live stdout/stderr streaming
- Rolling in-memory buffer with line count
- `Copy All` and `Clear` actions
- Optional file logging per profile

### Configuration Window

- Command/script editor for the active profile
- Run command on launch toggle
- Write logs to file toggle
- Applications entry toggle
- Session autostart toggle
- Saved/unsaved status with close confirmation

## Sudo Behavior

If the configured command starts with `sudo`, `givetray` prompts for password on each Start.
The password is passed to `sudo` via stdin (`sudo -S`) and is not stored in config.

## Contributing

Contributions are welcome.

- Open an issue first for significant changes so scope and approach can be aligned.
- Keep pull requests focused and include clear reproduction or verification notes.

By submitting a contribution, you agree that your work is licensed under
`MIT OR Apache-2.0`.

## License

Licensed under either of:

- MIT license (`LICENSE-MIT`)
- Apache License, Version 2.0 (`LICENSE-APACHE`)

at your option.

## Support

If `givetray` helps your workflow, you can support development:

https://buymeacoffee.com/allenguarnes
