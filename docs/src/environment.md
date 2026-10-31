---
title: Environment Variables - Asylum
description: How Asylum detects and uses environment variables. Shell integration, dotenv support, and troubleshooting.
---

# Environment Variables

_**Note**: The following only applies to Zed 0.152.0 and later._

Multiple features in Asylum are affected by environment variables:

- [Tasks](./tasks.md)
- [Built-in terminal](./terminal.md)
- Look-up of language servers
- Language servers

To make the best use of these features, it helps to understand where Asylum gets environment variables and how it uses them.

## Where does Asylum get its environment variables from?

How Asylum starts affects which environment variables it can use. That includes launching from the macOS Dock, a Linux window manager, or the `asylum` CLI.

### Launched from the CLI

If Asylum is opened via the CLI (`asylum`), it will inherit the environment variables from the surrounding shell session.

That means if you do

```
$ export MY_ENV_VAR=hello
$ asylum .
```

the environment variable `MY_ENV_VAR` is now available inside Asylum. For example, in the built-in terminal.

Starting with Zed 0.152.0, the CLI `asylum` will _always_ pass along its environment to Asylum, regardless of whether an Asylum instance was previously running or not. Prior to Zed 0.152.0 this was not the case and only the first Asylum instance would inherit the environment variables.

### Launched via window manager, Dock, or launcher

When Asylum has been launched via the macOS Dock, or a GNOME or KDE icon on Linux, or an application launcher like Alfred or Raycast, it has no surrounding shell environment from which to inherit its environment variables.

To still have a useful environment, Asylum spawns a login shell in the user's home directory and reads its environment. This environment is then set on the Asylum _process_, so all Asylum windows and projects inherit it.

Since that can lead to problems for users who need different environment variables per project (for example with `direnv`, `asdf`, or `mise`), Asylum spawns another login shell when opening a project. This second shell runs in the project's directory. The environment from that shell is _not_ set on the process, because opening a new project would otherwise change the environment for all Asylum windows. Instead, that environment is stored and passed along when running tasks, opening terminals, or spawning language servers.

## Where and how are environment variables used?

There are two sets of environment variables:

1. Environment variables of the Asylum process
2. Environment variables stored per project

The variables from (1) are always used, since they are stored on the process itself and every spawned process (tasks, terminals, language servers, ...) will inherit them by default.

The variables from (2) are used explicitly, depending on the feature.

### Tasks

Tasks are spawned with a combined environment. In order of precedence (low to high, with the last overwriting the first):

- the Asylum process environment
- if the project was opened from the CLI: the CLI environment
- if the project was not opened from the CLI: the project environment variables obtained by running a login shell in the project's root folder
- optional, explicitly configured environment in settings

### Built-in terminal

Built-in terminals, like tasks, are spawned with a combined environment. In order of precedence (low to high):

- the Asylum process environment
- if the project was opened from the CLI: the CLI environment
- if the project was not opened from the CLI: the project environment variables obtained by running a login shell in the project's root folder
- optional, explicitly configured environment in settings

### Look-up of language servers

For some languages the language server adapters lookup the binary in the user's `$PATH`. Examples:

- Go
- Zig
- Rust (if [configured to do so](./languages/rust.md#binary))
- C
- TypeScript

For this look-up, Asylum uses the following environment:

- if the project was opened from the CLI: the CLI environment
- if the project was not opened from the CLI: the project environment variables obtained by running a login shell in the project's root folder

### Language servers

After looking up a language server, Asylum starts it.

These language server processes always inherit Asylum's process environment. But, depending on the language server look-up, additional environment variables might be set or overwrite the process environment.

- If the language server was found in the project environment's `$PATH`, then that project environment is passed along to the language server process. Where the project environment comes from depends on how the project was opened (via CLI or not). See the previous section on language server look-up.
- If the language server was not found in the project environment, Asylum tries to install and start it globally. In that case, the process inherits Asylum's process environment and, if the project was opened via CLI, the CLI environment.
