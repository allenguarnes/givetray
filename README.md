# givetray

`givetray` runs terminal commands from the Linux system tray using named profiles.
Each profile can run one command, show live logs, and manage desktop entries.

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

## CLI Usage

`-c/--config PROFILE` is required for app mode and desktop-file mode.

```bash
givetray -c PROFILE [--icon ICON_PATH] [--log-file LOG_PATH]
givetray desktop-file -c PROFILE [--output-dir DIR] [--autostart] [--icon ICON_PATH]
givetray --help
givetray --version
```

Examples:

```bash
givetray -c scrcpy
givetray -c scrcpy --icon /path/to/icon.png
givetray -c scrcpy --log-file ~/.local/share/givetray/logs/scrcpy.log
givetray desktop-file -c scrcpy
givetray desktop-file -c scrcpy --autostart
```

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
- `Configuration`: edit profile command and toggles
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
