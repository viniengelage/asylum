---
title: Asylum on Windows
description: "Install, uninstall, and troubleshoot Asylum on Windows."
---

# Asylum on Windows

## Installing Asylum

Download `Asylum-x86_64.exe` from the [GitHub releases page](https://github.com/viniengelage/asylum/releases), when it is available for a release, and run the installer. After the first manual installation, Asylum will periodically check for updates.

You can also build Asylum from source, see [these docs](./development/windows.md) for instructions.

## Uninstall

- Installed via installer: Use `Settings` → `Apps` → `Installed apps`, search for Asylum, and click Uninstall.
- Built from source: Remove the build output directory you created (e.g., your target/install folder).

Your settings and extensions live in your user profile. When uninstalling, you can choose to keep or remove them.

## Remote Development (SSH)

Asylum supports remote development on Windows through both SSH and WSL. You can connect to remote servers via SSH or work with files inside WSL distributions directly from Asylum.

For detailed instructions on setting up and using remote development features, including SSH configuration, WSL setup, and troubleshooting, see the [Remote Development documentation](./remote-development.md).

## Troubleshooting

### Asylum fails to start or shows a blank window

- Check that your hardware and operating system version are compatible with Asylum. See our [installation guide](./installation.md) for more information.
- Update your GPU drivers from your GPU vendor (Intel/AMD/NVIDIA/Qualcomm).
- Ensure hardware acceleration is enabled in Windows and not blocked by third‑party software.
- Try launching Asylum with no extensions or custom settings to isolate conflicts.

### Terminal issues

If activation scripts don’t run, update to the latest version and verify your shell profile files are not exiting early. For Git operations, confirm Git Bash or PowerShell is available and on PATH.

### SSH remoting problems

When prompted for credentials, use the graphical askpass dialog. If it doesn’t appear, check for credential manager conflicts and that GUI prompts aren’t blocked by your terminal.

### Graphics issues

#### Asylum fails to open / degraded performance

Asylum requires a DirectX 11 compatible GPU to run. If Asylum fails to open, your GPU may not meet the minimum requirements.

To check if your GPU supports DirectX 11, run the following command:

```
dxdiag
```

This will open the DirectX Diagnostic Tool, which shows the DirectX version your GPU supports under `System` → `System Information` → `DirectX Version`.

If you're running Asylum inside a virtual machine, it will use the emulated adapter provided by your VM. While Asylum will work in this environment, performance may be degraded.
