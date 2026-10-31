---
title: Asylum on Linux
description: "Build, install, and troubleshoot Asylum on Linux."
---

# Asylum on Linux

## Installation

There are no prebuilt Linux releases of Asylum yet. Build it [from source](./development/linux.md); from a checkout of the repository, `script/install-linux` builds a release bundle and installs it into `~/.local`, making it available as `~/.local/bin/asylum`.

Packages named `zed` or `zed-editor` in Linux distributions and package managers install Zed, not Asylum.

## Uninstalling Asylum

### Standard Uninstall

If Asylum was installed using the default installation script, it can be uninstalled by supplying the `--uninstall` flag to the `asylum` shell command:

```sh
asylum --uninstall
```

Note that this will uninstall the Asylum variant that the symlink points to. If you have multiple, parallel installations (such as both Stable and Preview), then you should instead use its absolute path as described further down in this section.

If there are no errors, the shell will then prompt you whether you'd like to keep your preferences or delete them. After making a choice, you should see a message that Asylum was successfully uninstalled.

In the case that the `asylum` shell command was not found in your PATH, you can try one of the following commands:

```sh
$HOME/.local/bin/asylum --uninstall
```

or the absolute path to your installation, such as

```sh
$HOME/.local/asylum.app/bin/asylum --uninstall
```

The first case might fail if a symlink was not properly established between `$HOME/.local/bin/asylum` and `$HOME/.local/asylum.app/bin/asylum`, or if the symlink was overwritten by having parallel installations of different versions of Asylum. But the second command should always work as long as Asylum was installed to its default location.

If Asylum was installed to a different location (such as the `asylum-preview.app` variant), you must invoke the `asylum` binary stored in that installation directory and pass the `--uninstall` flag to it in the same format as the previous commands.

### Package Manager

If Asylum was installed using a package manager, please consult the documentation for that package manager on how to uninstall a package.

## Troubleshooting

Linux works on a large variety of systems configured in many different ways. We primarily test Asylum on a vanilla Ubuntu setup, as it is the most common distribution our users use. That said, we do expect it to work on a wide variety of machines.

### Asylum fails to start

If you see an error like "/lib64/libc.so.6: version 'GLIBC_2.29' not found" it means that your distribution's version of glibc is too old. You can either upgrade your system, or [install Asylum from source](./development/linux.md).

### Graphics issues

#### Asylum fails to open windows

Asylum requires a GPU to run effectively. Under the hood, we use [Vulkan](https://www.vulkan.org/) to communicate with your GPU. If you are seeing problems with performance, or Asylum fails to load, it is possible that Vulkan is the culprit.

If you see a notification saying `Asylum failed to open a window: NoSupportedDeviceFound` this means that Vulkan cannot find a compatible GPU. You can try running [vkcube](https://github.com/krh/vkcube) (usually available as part of the `vulkaninfo` or `vulkan-tools` package on various distributions) to try to troubleshoot where the issue is coming from like so:

```
vkcube
```

> **_Note_**: Try running in both X11 and wayland modes by running `vkcube -m [x11|wayland]`. Some versions of `vkcube` use `vkcube` to run in X11 and `vkcube-wayland` to run in wayland.

This should output a line describing your current graphics setup and show a rotating cube. If this does not work, you should be able to fix it by installing Vulkan compatible GPU drivers, however in some cases there is no Vulkan support yet.

You can find out which graphics card Asylum is using by looking in the Asylum log (`~/.local/share/asylum/logs/Asylum.log`) for `Using GPU: ...`.

If you see errors like `ERROR_INITIALIZATION_FAILED` or `GPU Crashed` or `ERROR_SURFACE_LOST_KHR` then you may be able to work around this by installing different drivers for your GPU, or by selecting a different GPU to run on. (See [#14225](https://github.com/zed-industries/zed/issues/14225))

On some systems the file `/etc/prime-discrete` can be used to enforce the use of a discrete GPU using [PRIME](https://wiki.archlinux.org/title/PRIME). Depending on the details of your setup, you may need to change the contents of this file to "on" (to force discrete graphics) or "off" (to force integrated graphics).

On others, you may be able to set the environment variable `DRI_PRIME=1` when running Asylum to force the use of the discrete GPU.

If you're using an AMD GPU, you might get a 'Broken Pipe' error. Try using the RADV or Mesa drivers. (See [#13880](https://github.com/zed-industries/zed/issues/13880))

If you are using `amdvlk`, the default open-source AMD graphics driver, you may find that Asylum consistently fails to launch. This is a known issue for some users, for example on Omarchy (see issue [#28851](https://github.com/zed-industries/zed/issues/28851)). To fix this, you will need to use a different driver. We recommend removing the `amdvlk` and `lib32-amdvlk` packages and installing `vulkan-radeon` instead (see issue [#14141](https://github.com/zed-industries/zed/issues/14141)).

For more information, the [Arch guide to Vulkan](https://wiki.archlinux.org/title/Vulkan) has some good steps that translate well to most distributions.

#### Forcing Asylum to use a specific GPU

There are a few different ways to force Asylum to use a specific GPU:

##### Option A

You can use the `ZED_DEVICE_ID={device_id}` environment variable to specify the device ID of the GPU you wish to have Asylum use.

You can obtain the device ID of your GPU by running `lspci -nn | grep VGA` which will output each GPU on one line like:

```
08:00.0 VGA compatible controller [0300]: NVIDIA Corporation GA104 [GeForce RTX 3070] [10de:2484] (rev a1)
```

where the device ID here is `2484`. This value is in hexadecimal, so to force Asylum to use this specific GPU you would set the environment variable like so:

```
ZED_DEVICE_ID=0x2484 asylum
```

Make sure to export the variable if you choose to define it globally in a `.bashrc` or similar.

##### Option B

If you are using Mesa, you can run `MESA_VK_DEVICE_SELECT=list asylum --foreground` to get a list of available GPUs and then export `MESA_VK_DEVICE_SELECT=xxxx:yyyy` to choose a specific device. Furthermore, you can fallback to xwayland with an additional export of `WAYLAND_DISPLAY=""`.

##### Option C

Using [vkdevicechooser](https://github.com/jiriks74/vkdevicechooser).

#### Reporting graphics issues

If Vulkan is configured correctly, and Asylum is still not working for you, please [file an issue](https://github.com/viniengelage/asylum/issues) with as much information as possible.

When reporting issues where Asylum fails to start due to graphics initialization errors on GitHub, it can be impossible to run the {#action zed::CopySystemSpecsIntoClipboard} command like we instruct you to in our issue template. We provide an alternative way to collect the system specs specifically for this situation.

Passing the `--system-specs` flag to Asylum like

```sh
asylum --system-specs
```

will print the system specs to the terminal like so. It is strongly recommended to copy the output verbatim into the issue on GitHub, as it uses markdown formatting to ensure the output is readable.

Additionally, it is extremely beneficial to provide the contents of your Asylum log when reporting such issues. The log is usually located at `~/.local/share/asylum/logs/Asylum.log`. The recommended process for producing a helpful log file is as follows:

```sh
truncate -s 0 ~/.local/share/asylum/logs/Asylum.log # Clear the log file
ZED_LOG=wgpu=info asylum .
cat ~/.local/share/asylum/logs/Asylum.log
# copy the output
```

Or, if you have the Asylum cli setup, you can do

```sh
ZED_LOG=wgpu=info /path/to/zed/cli --foreground .
# copy the output
```

It is also highly recommended when pasting the log into a github issue, to do so with the following template:

> **_Note_**: The whitespace in the template is important, and will cause incorrect formatting if not preserved.

````
<details><summary>Asylum Log</summary>

```
{asylum log contents}
```

</details>
````

This will cause the logs to be collapsed by default, making it easier to read the issue.

### I can't open any files

### Clicking links isn't working

These features are provided by XDG desktop portals, specifically:

- `org.freedesktop.portal.FileChooser`
- `org.freedesktop.portal.OpenURI`

Some window managers, such as `Hyprland`, don't provide a file picker by default. See [this list](https://wiki.archlinux.org/title/XDG_Desktop_Portal#List_of_backends_and_interfaces) as a starting point for alternatives.

### Asylum isn't remembering my API keys

### Asylum isn't remembering my login

This feature also requires XDG desktop portals, specifically:

- `org.freedesktop.portal.Secret` or
- `org.freedesktop.Secrets`

Asylum needs a place to securely store secrets such as your Asylum login cookie or your OpenAI API Keys and we use a system provided keychain to do this. Examples of packages that provide this are `gnome-keyring`, `KWallet` and `keepassxc` among others.

### Could not start inotify

Asylum relies on inotify to watch your filesystem for changes. If you cannot start inotify then Asylum will not work reliably.

If you are seeing "too many open files" then first try `sysctl fs.inotify`.

- You should see that max_user_instances is 128 or higher (you can change the limit with `sudo sysctl fs.inotify.max_user_instances=1024`). Asylum needs only 1 inotify instance.
- You should see that `max_user_watches` is 8000 or higher (you can change the limit with `sudo sysctl fs.inotify.max_user_watches=64000`). Asylum needs one watch per directory in all your open projects + one per git repository + a handful more for settings, themes, keymaps, extensions.

It is also possible that you are running out of file descriptors. You can check the limits with `ulimit` and update them by editing `/etc/security/limits.conf`.

### No sound or wrong output device

If you're not hearing any sound in Asylum or the audio is routed to the wrong device, it could be due to a mismatch between audio systems. Asylum relies on ALSA, while your system may be using PipeWire or PulseAudio. To resolve this, you need to configure ALSA to route audio through PipeWire/PulseAudio.

If your system uses PipeWire:

1. **Install the PipeWire ALSA plugin**

   On Debian-based systems, run:

   ```bash
   sudo apt install pipewire-alsa
   ```

2. **Configure ALSA to use PipeWire**

   Add the following configuration to your ALSA settings file. You can use either `~/.asoundrc` (user-level) or `/etc/asound.conf` (system-wide):

   ```bash
   pcm.!default {
       type pipewire
   }

   ctl.!default {
       type pipewire
   }
   ```

3. **Restart your system**

### Forcing X11 scale factor

On X11 systems, Asylum automatically detects the appropriate scale factor for high-DPI displays. The scale factor is determined using the following priority order:

1. `GPUI_X11_SCALE_FACTOR` environment variable (if set)
2. `Xft.dpi` from X resources database (xrdb)
3. Automatic detection via RandR based on monitor resolution and physical size

If you want to customize the scale factor beyond what Asylum detects automatically, you have several options:

#### Check your current scale factor

You can verify if you have `Xft.dpi` set:

```sh
xrdb -query | grep Xft.dpi
```

If this command returns no output, Asylum is using RandR (X11's monitor management extension) to automatically calculate the scale factor based on your monitor's reported resolution and physical dimensions.

#### Option 1: Set Xft.dpi (X Resources Database)

`Xft.dpi` is a standard X11 setting that many applications use for consistent font and UI scaling. Setting this ensures Asylum scales the same way as other X11 applications that respect this setting.

Edit or create the `~/.Xresources` file:

```sh
vim ~/.Xresources
```

Add this line with your desired DPI:

```sh
Xft.dpi: 96
```

Common DPI values:

- `96` for standard 1x scaling
- `144` for 1.5x scaling
- `192` for 2x scaling
- `288` for 3x scaling

Load the configuration:

```sh
xrdb -merge ~/.Xresources
```

Restart Asylum for the changes to take effect.

#### Option 2: Use the GPUI_X11_SCALE_FACTOR environment variable

This Asylum-specific environment variable directly sets the scale factor, bypassing all automatic detection.

```sh
GPUI_X11_SCALE_FACTOR=1.5 asylum
```

You can use decimal values (e.g., `1.25`, `1.5`, `2.0`) or set `GPUI_X11_SCALE_FACTOR=randr` to force RandR-based detection even when `Xft.dpi` is set.

To make this permanent, add it to your shell profile or desktop entry.

#### Option 3: Adjust system-wide RandR DPI

This changes the reported DPI for your entire X11 session, affecting how RandR calculates scaling for all applications that use it.

Add this to your `.xprofile` or `.xinitrc`:

```sh
xrandr --dpi 192
```

Replace `192` with your desired DPI value. This affects the system globally and will be used by Asylum's automatic RandR detection when `Xft.dpi` is not set.

### Font rendering parameters

On Linux, Asylum reads `ZED_FONTS_GAMMA` and `ZED_FONTS_GRAYSCALE_ENHANCED_CONTRAST` environment variables for the values to use for font rendering.

`ZED_FONTS_GAMMA` corresponds to [getgamma](https://learn.microsoft.com/en-us/windows/win32/api/dwrite/nf-dwrite-idwriterenderingparams-getgamma) values.
Allowed range [1.0, 2.2], other values are clipped.
Default: 1.8

`ZED_FONTS_GRAYSCALE_ENHANCED_CONTRAST` corresponds to [getgrayscaleenhancedcontrast](https://learn.microsoft.com/en-us/windows/win32/api/dwrite_1/nf-dwrite_1-idwriterenderingparams1-getgrayscaleenhancedcontrast) values.
Allowed range: [0.0, ..), other values are clipped.
Default: 1.0
