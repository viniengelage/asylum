---
title: Building Asylum for Linux
description: "Guide to building zed for linux for Asylum development."
---

# Building Asylum for Linux

## Repository

Clone the [Asylum repository](https://github.com/viniengelage/asylum).

## Dependencies

- Install [rustup](https://www.rust-lang.org/tools/install)

- Install the necessary system libraries:

  ```sh
  script/linux
  ```

  If you prefer to install the system libraries manually, you can find the list of required packages in the `script/linux` file.

## Building from source

Once the dependencies are installed, you can build Asylum using [Cargo](https://doc.rust-lang.org/cargo/).

For a debug build of the editor:

```sh
cargo run
```

And to run the tests:

```sh
cargo test --workspace
```

In release mode, the primary user interface is the `cli` crate. You can run it in development with:

```sh
cargo run -p cli
```

## Installing a development build

You can install a local build on your machine with:

```sh
./script/install-linux
```

This builds `asylum` and the `cli` in release mode, installs the binary at `~/.local/bin/asylum`, and installs `.desktop` files to `~/.local/share`.

## Wayland & X11

Asylum supports both X11 and Wayland. By default, we pick whichever we can find at runtime. If you're on Wayland and want to run in X11 mode, use the environment variable `WAYLAND_DISPLAY=''`.

## Notes for packaging Asylum

This section is for distribution maintainers packaging Asylum.

### Technical requirements

Asylum has two main binaries:

- You will need to build `crates/cli` and make its binary available in `$PATH` with the name `asylum`.
- You will need to build `crates/zed` and put it at `$PATH/to/cli/../../libexec/asylum-editor`. For example, if you are going to put the cli at `~/.local/bin/asylum` put it at `~/.local/libexec/asylum-editor`. As some linux distributions (notably Arch) discourage the use of `libexec`, you can also put this binary at `$PATH/to/cli/../../lib/asylum/asylum-editor` (e.g. `~/.local/lib/asylum/asylum-editor`) instead.
- If you are going to provide a `.desktop` file you can find a template in `crates/zed/resources/zed.desktop.in`, and use `envsubst` to populate it with the values required. This file should also be renamed to `$APP_ID.desktop` so that the file [follows the FreeDesktop standards](https://github.com/zed-industries/zed/issues/12707#issuecomment-2168742761). You should also make this desktop file executable (`chmod 755`).
- You will need to ensure that the necessary libraries are installed. You can get the current list by [inspecting the built binary](https://github.com/zed-industries/zed/blob/935cf542aebf55122ce6ed1c91d0fe8711970c82/script/bundle-linux#L65-L67) on your system.
- For an example of a complete build script, see [script/bundle-linux](https://github.com/zed-industries/zed/blob/935cf542aebf55122ce6ed1c91d0fe8711970c82/script/bundle-linux).
- You can disable Asylum's auto updates and provide instructions for users who try to update Asylum manually by building (or running) Asylum with the environment variable `ASYLUM_UPDATE_EXPLANATION`. For example: `ASYLUM_UPDATE_EXPLANATION="Please use flatpak to update zed."`.
- Make sure to update the contents of the `crates/zed/RELEASE_CHANNEL` file to 'nightly', 'preview', or 'stable', with no newline. This will cause Asylum to use the credentials manager to remember a user's login.

### Other things to note

Asylum moves quickly, and distribution maintainers often have different constraints and priorities. The points below describe current trade-offs:

- Asylum automatically installs versions of common developer tools, similar to rustup/rbenv/pyenv. This behavior is discussed [here](https://github.com/zed-industries/zed/issues/12589).
- Users can install extensions locally and from [zed-industries/extensions](https://github.com/zed-industries/extensions). Extensions may install additional tools such as language servers. Planned safety improvements are tracked [here](https://github.com/zed-industries/zed/issues/12358).
- Because of the points above, Asylum currently does not work well with sandboxes. See [this discussion](https://github.com/zed-industries/zed/pull/12006#issuecomment-2130421220).

## Flatpak

> Asylum's current Flatpak integration exits the sandbox on startup. Workflows that rely on Flatpak's sandboxing may not work as expected.

To build & install the Flatpak package locally follow the steps below:

1. Install Flatpak for your distribution as outlined [here](https://flathub.org/setup).
2. Run the `script/flatpak/deps` script to install the required dependencies.
3. Run `script/flatpak/bundle-flatpak`.
4. Now the package has been installed and has a bundle available at `target/release/{app-id}.flatpak`.

## Memory profiling

[`heaptrack`](https://github.com/KDE/heaptrack) is quite useful for diagnosing memory leaks. To install it:

```sh
$ sudo apt install heaptrack heaptrack-gui
$ cargo install cargo-heaptrack
```

Then, to build and run Asylum with the profiler attached:

```sh
$ cargo heaptrack -b asylum
```

When this zed instance is exited, terminal output will include a command to run `heaptrack_interpret` to convert the `*.raw.zst` profile to a `*.zst` file which can be passed to `heaptrack_gui` for viewing.

## Perf recording

How to get a flamegraph with resolved symbols from a running Asylum instance.
Use this when Asylum is using a lot of CPU. It is not useful for hangs.

### During the incident

- Find the PID (process ID) using:
  `ps -eo size,pid,comm | grep asylum | sort | head -n 1 | cut -d ' ' -f 2`
  Or find the PID of `asylum-editor` with the highest RAM usage in something
  like htop/btop/top.

- Install perf:
  On Ubuntu (derivatives) run `sudo apt install linux-tools`.

- Perf record:
  Run `sudo perf record -g --call-graph dwarf -p <pid you just found>`, wait a few seconds to gather data, then press Ctrl+C. You should now have a `perf.data` file.
  The `--call-graph dwarf` option records callers of every sampled function and unwinds through the `.eh_frame` data kept in stripped release binaries; plain `-g` does not work without frame pointers.

- Make the output file user owned:
  run `sudo chown $USER:$USER perf.data`

- Get build info:
  Run zed again and type {#action zed::About} in the command pallet to get the exact commit.

The `perf.data` file can be attached to a [GitHub issue](https://github.com/viniengelage/asylum/issues) together with the exact commit.

## Troubleshooting

### Cargo errors claiming that a dependency is using unstable features

Try `cargo clean` and `cargo build`.
