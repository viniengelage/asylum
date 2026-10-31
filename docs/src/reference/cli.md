---
title: CLI Reference
description: "Reference for Asylum's command-line interface (CLI), including opening files and directories, integrating with tools, and controlling Asylum from scripts."
---

# CLI Reference

Use Asylum's command-line interface (CLI) to open files and directories, integrate with other tools, and control Asylum from scripts.

## Installation

**macOS:** Run the {#action cli::InstallCliBinary} command from the command palette ({#kb command_palette::Toggle}) to install the `asylum` CLI to `/usr/local/bin/asylum`.

**Linux:** The CLI is included in the Asylum bundle as `bin/asylum`.

**Windows:** The CLI is included with Asylum. Add the `bin` folder in Asylum's installation directory to your PATH.

## Usage

```sh
asylum [OPTIONS] [PATHS]...
```

## Opening Files and Directories

Open a file:

```sh
asylum myfile.txt
```

Open a directory as a workspace:

```sh
asylum ~/projects/myproject
```

Open multiple files or directories:

```sh
asylum file1.txt file2.txt ~/projects/myproject
```

Open a file at a specific line and column:

```sh
asylum myfile.txt:42        # Open at line 42
asylum myfile.txt:42:10     # Open at line 42, column 10
```

## Options

### `-w`, `--wait`

Wait for all opened files to be closed before the CLI exits. When opening a directory, waits until the window is closed.

This is useful for integrating Asylum with tools that expect an editor to block until editing is complete (e.g., `git commit`):

```sh
export EDITOR="asylum --wait"
git commit  # Opens Asylum and waits for you to close the commit message file
```

### `-n`, `--new`

Open paths in a new workspace window, even if the paths are already open in an existing window:

```sh
asylum -n ~/projects/myproject
```

### `-a`, `--add`

Add paths to the currently focused workspace instead of opening a new window. When multiple workspace windows are open, files open in the focused window:

```sh
asylum -a newfile.txt
```

### `-r`, `--reuse`

Reuse an existing window, replacing its current workspace with the new paths:

```sh
asylum -r ~/projects/different-project
```

### `-e`, `--existing`

Open paths in an existing Asylum window instead of creating a new one:

```sh
asylum -e myfile.txt
```

By default (without `-n`, `-a`, `-r`, or `-e`), directories open in the current window's sidebar. You can change this default with the `cli_default_open_behavior` setting. See [Windows & Projects](../windows-and-projects.md) for more details.

### `--diff <OLD_PATH> <NEW_PATH>`

Open a diff view comparing two files. Can be specified multiple times:

```sh
asylum --diff file1.txt file2.txt
asylum --diff old.rs new.rs --diff old2.rs new2.rs
```

### `--foreground`

Run Asylum in the foreground, keeping the terminal attached. Useful for debugging:

```sh
asylum --foreground
```

### `--user-data-dir <DIR>`

Use a custom directory for all user data (database, extensions, logs) instead of the default location:

```sh
asylum --user-data-dir ~/.asylum-custom
```

Default locations:

- **macOS:** `~/Library/Application Support/Asylum`
- **Linux:** `$XDG_DATA_HOME/asylum` (typically `~/.local/share/asylum`)
- **Windows:** `%LOCALAPPDATA%\Asylum`

### `-v`, `--version`

Print Asylum's version and exit:

```sh
asylum --version
```

### `--completions <SHELL>`

Generate shell completions for the `asylum` CLI:

#### Bash

Add to `~/.bashrc`:

```bash
eval "$(asylum --completions bash)"
```

#### Elvish

Add to `~/.config/elvish/rc.elv`:

```elvish
set edit:completion:arg-completer[zed] = { |@args|
    eval (asylum --completions elvish | slurp)
    $edit:completion:arg-completer[zed] $@args
}
```

#### Fish

Add to `~/.config/fish/config.fish`:

```fish
asylum --completions fish | source
```

#### Nushell

Add to `~/.config/nushell/config.nu`:

```nu
mkdir ($nu.data-dir | path join "vendor/autoload")
^asylum --completions nushell | save --force ($nu.data-dir | path join "vendor/autoload/asylum.nu")
```

#### Powershell

Add to `$PROFILE`:

```powershell
(&asylum --completions powershell) | Out-String | Invoke-Expression
```

#### Zsh

Add to `~/.zshrc`:

```zsh
eval "$(asylum --completions zsh)"
```

### `--uninstall`

Uninstall Asylum and remove all related files (macOS and Linux only):

```sh
asylum --uninstall
```

### `--zed <PATH>`

Specify a custom path to the Asylum application or binary:

```sh
asylum --zed /path/to/Asylum.app myfile.txt
```

## Reading from Standard Input

Read content from stdin by passing `-` as the path:

```sh
echo "Hello, World!" | asylum -
cat myfile.txt | asylum -
ps aux | asylum -
```

This creates a temporary file with the stdin content and opens it in Asylum.

## URL Handling

The CLI can open `asylum://`, `file://`, and `ssh://` URLs:

```sh
asylum asylum://settings
asylum file:///Users/whatever/.zshrc
asylum ssh://me@example.com/abs/path
asylum ssh://me@example.com:/abs/path
asylum ssh://me@example.com/~/project
asylum ssh://me@example.com:~/project
```

## Using Asylum as Your Default Editor

Set Asylum as your default editor for Git and other tools:

```sh
export EDITOR="asylum --wait"
export VISUAL="asylum --wait"
```

Add these lines to your shell configuration file (e.g., `~/.bashrc`, `~/.zshrc`).

## macOS: Switching Release Channels

On macOS, you can launch a specific release channel by passing the channel name as the first argument:

```sh
asylum --stable myfile.txt
asylum --preview myfile.txt
asylum --nightly myfile.txt
```

## WSL Integration (Windows)

On Windows, the CLI supports opening paths from WSL distributions. This is handled automatically when launching Asylum from within WSL.

## Exit Codes

| Code | Meaning                           |
| ---- | --------------------------------- |
| `0`  | Success                           |
| `1`  | Error (details printed to stderr) |

When using `--wait`, the exit code reflects whether the files were saved before closing.
