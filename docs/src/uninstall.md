---
title: Uninstall
description: "This guide covers how to uninstall Asylum on different operating systems."
---

# Uninstall

This guide covers how to uninstall Asylum on different operating systems.

## macOS

1. Quit Asylum if it's running
2. Open Finder and go to your Applications folder
3. Drag Asylum to the Trash (or right-click and select "Move to Trash")
4. Empty the Trash

### Removing User Data (Optional)

To completely remove all Asylum configuration files and data:

1. Open Finder
2. Press `Cmd + Shift + G` to open "Go to Folder"
3. Delete the following directories if they exist:
   - `~/Library/Application Support/Asylum`
   - `~/Library/Saved Application State/dev.asylum.Asylum.savedState`
   - `~/Library/Logs/Asylum`
   - `~/Library/Caches/Asylum`
   - `~/.config/asylum`

## Linux

### Standard Uninstall

There are no prebuilt Linux releases of Asylum. If you built and installed it with `script/install-linux`, run the bundled uninstall command:

```sh
$HOME/.local/bin/asylum --uninstall
```

You'll be prompted whether to keep or delete your preferences.

### Manual Removal

If the uninstall command fails or Asylum was installed to a custom location, you can manually remove:

- Installation directory: `~/.local/asylum.app` (or your custom installation path)
- Binary symlink: `~/.local/bin/asylum`
- Configuration: `~/.config/asylum`
- Data: `$XDG_DATA_HOME/asylum` (usually `~/.local/share/asylum`)

## Windows

### Standard Installation

1. Quit Asylum if it's running
2. Open Settings (Windows key + I)
3. Go to "Apps" > "Installed apps" (or "Apps & features" on Windows 10)
4. Search for "Asylum"
5. Click the three dots menu next to Asylum and select "Uninstall"
6. Follow the prompts to complete the uninstallation

Alternatively, you can:

1. Open the Start menu
2. Right-click on Asylum
3. Select "Uninstall"

### Removing User Data (Optional)

To completely remove all Asylum configuration files and data:

1. Press `Windows key + R` to open Run
2. Type `%APPDATA%` and press Enter
3. Delete the `Asylum` folder if it exists
4. Press `Windows key + R` again, type `%LOCALAPPDATA%` and press Enter
5. Delete the `Asylum` folder if it exists

## Troubleshooting

If you encounter issues during uninstallation:

- **macOS/Windows**: Ensure Asylum is completely quit before attempting to uninstall. Check Activity Manager (macOS) or Task Manager (Windows) for any running Asylum processes.
- **Linux**: If the uninstall script fails, check the error message and consider manual removal of the directories listed above.
- **All platforms**: If you want to start fresh while keeping Asylum installed, you can delete the configuration directories instead of uninstalling the application entirely.

For additional help, see the [Linux-specific documentation](./linux.md) or [open an issue](https://github.com/viniengelage/asylum/issues).
