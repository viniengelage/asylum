---
title: Asylum on macOS
description: "Asylum is developed primarily on macOS, making it a first-class platform with full feature support."
---

# Asylum on macOS

Asylum is developed primarily on macOS, making it a first-class platform with full feature support.

## Installing Asylum

Download `Asylum-aarch64.dmg` from the [GitHub releases page](https://github.com/viniengelage/asylum/releases), open it, and drag Asylum to your Applications folder. Prebuilt releases are only available for Apple silicon; on Intel Macs, build from source.

After installation, Asylum checks for updates automatically and prompts you when a new version is available.

### Building from Source

To build Asylum from source, see the [macOS development documentation](./development/macos.md).

## System Requirements

- macOS 12.0 (Monterey) or later
- Apple Silicon (M1/M2/M3/M4) or Intel processor

Asylum uses Metal for GPU-accelerated rendering, which is available on all supported macOS versions.

## Installing the CLI

Asylum includes a command-line tool for opening files and projects from Terminal. To install it:

1. Open Asylum
2. Open the command palette with `Cmd+Shift+P`
3. Run {#action cli::InstallCliBinary}

This creates an `asylum` command in `/usr/local/bin`. You can then open files and folders:

```sh
asylum .                    # Open current folder
asylum file.txt             # Open a file
asylum project/ file.txt    # Open a folder and a file
```

See the [CLI Reference](./reference/cli.md) for all available options.

## Uninstall

1. Quit Asylum if it's running
2. Drag Asylum from Applications to the Trash
3. Optionally, remove your settings and extensions:

```sh
rm -rf ~/.config/asylum
rm -rf ~/Library/Application\ Support/Asylum
rm -rf ~/Library/Caches/Asylum
rm -rf ~/Library/Logs/Asylum
rm -rf ~/Library/Saved\ Application\ State/dev.asylum.Asylum.savedState
```

If you installed the CLI, remove it with:

```sh
rm /usr/local/bin/asylum
```

## Troubleshooting

### Asylum won't open or shows "damaged" warning

If macOS reports that Asylum is damaged or can't be opened, it's likely a Gatekeeper issue. Try:

1. Right-click (or Control-click) on Asylum in Applications
2. Select "Open" from the context menu
3. Click "Open" in the dialog that appears

This tells macOS to trust the application.

If that doesn't work, remove the quarantine attribute:

```sh
xattr -cr /Applications/Asylum.app
```

### CLI command not found

If the `asylum` command isn't available after installation:

1. Check that `/usr/local/bin` is in your PATH
2. Try reinstalling the CLI via {#action cli::InstallCliBinary} in the command palette
3. Open a new terminal window to reload your PATH

### Can't install CLI {#cant-install-cli}

{#action cli::InstallCliBinary} writes an `asylum` symlink to `/usr/local/bin`, which requires administrator privileges. If your macOS account isn't in the `admin` group, Asylum can't create that symlink and will report that it can't install the CLI automatically.

Instead, you can add an alias pointing to the `cli` binary bundled inside the app. The path depends on where Asylum is installed:

```sh
# Default install (Asylum in /Applications)
alias asylum="/Applications/Asylum.app/Contents/MacOS/cli"

# User install (Asylum in ~/Applications)
alias asylum="$HOME/Applications/Asylum.app/Contents/MacOS/cli"

# Preview build (Asylum Preview in ~/Applications)
alias asylum="$HOME/Applications/Asylum Preview.app/Contents/MacOS/cli"
```

Add the line that matches your install to your shell configuration file. Use `~/.zshrc` for Zsh (the default on modern macOS) or `~/.bashrc` for Bash.

After you restart your shell, you will be able to use `asylum` from your terminal:

```sh
asylum .              # Open current folder
asylum file.txt       # Open a file
```

### GPU or rendering issues

Asylum uses Metal for rendering. If you experience graphical glitches:

1. Ensure macOS is up to date
2. Restart your Mac to reset the GPU state
3. Check Activity Monitor for GPU pressure from other apps

### High memory or CPU usage

If Asylum uses more resources than expected:

1. Check for runaway language servers in the terminal output ({#action zed::OpenLog})
2. Try disabling extensions one by one to identify conflicts
3. For large projects, consider using [project settings](./reference/all-settings.md#file-scan-exclusions) to exclude unnecessary folders from indexing

For additional help, see the [Troubleshooting guide](./troubleshooting.md) or [open an issue](https://github.com/viniengelage/asylum/issues).
