---
title: Install Asylum - macOS, Linux, Windows
description: Download and install Asylum on macOS or Windows, or build it from source.
---

# Installing Asylum

## Download Asylum

Prebuilt releases are published on the [GitHub releases page](https://github.com/viniengelage/asylum/releases):

- **macOS** (Apple silicon): `Asylum-aarch64.dmg`
- **Windows** (x86_64): `Asylum-x86_64.exe`, when available for that release

After the first manual installation, Asylum periodically checks the GitHub releases page for updates.

There are no prebuilt Linux or Intel Mac builds yet. On those platforms, build Asylum [from source](./development.md).

## System Requirements

### macOS

Asylum requires macOS 12.0 (Monterey) or later.

| Version            | Codename    | Apple Status   | Asylum Status          |
| ------------------ | ----------- | -------------- | ------------------- |
| macOS 27.x         | Golden Gate | Supported      | Supported           |
| macOS 26.x         | Tahoe       | Supported      | Supported           |
| macOS 15.x         | Sequoia     | Supported      | Supported           |
| macOS 14.x         | Sonoma      | EOL 2026-09-14 | Supported           |
| macOS 13.x         | Ventura     | EOL 2025-09-15 | Supported           |
| macOS 12.x (12.3+) | Monterey    | EOL 2024-09-16 | Supported           |
| macOS 12.0–12.2    | Monterey    | EOL 2024-09-16 | Partially Supported |

The macOS releases labelled "Partially Supported" (Monterey 12.0–12.2) do not support features that use [ScreenCaptureKit.framework](https://developer.apple.com/documentation/screencapturekit/), which is available on macOS 12.3 and newer.

Apple support status and EOL dates are from [endoflife.date](https://endoflife.date/macos).

#### Mac Hardware

Asylum supports machines with Intel (x86_64) or Apple (aarch64) processors that meet the above macOS requirements:

- MacBook Pro (Early 2015 and newer)
- MacBook Air (Early 2015 and newer)
- MacBook (Early 2016 and newer)
- Mac Mini (Late 2014 and newer)
- Mac Pro (Late 2013 or newer)
- iMac (Late 2015 and newer)
- iMac Pro (all models)
- Mac Studio (all models)

### Linux

Asylum supports 64-bit Intel/AMD (x86_64) and 64-bit Arm (aarch64) processors.

Asylum requires a Vulkan 1.3 driver and the following desktop portals:

- `org.freedesktop.portal.FileChooser`
- `org.freedesktop.portal.OpenURI`
- `org.freedesktop.portal.Secret` or `org.freedesktop.Secrets`

### Windows

Asylum supports the following Windows releases:
| Version | Asylum Status |
| ------------------------- | ------------------- |
| Windows 11, version 22H2 and later | Supported |
| Windows 10, version 1903 and later | Supported |

A 64-bit operating system is required to run Asylum.

#### Windows Hardware

Asylum supports machines with x64 (Intel, AMD) or Arm64 (Qualcomm) processors that meet the following requirements:

- Graphics: A GPU that supports DirectX 11 (most PCs from 2012+).
- Driver: Current NVIDIA/AMD/Intel/Qualcomm driver (not the Microsoft Basic Display Adapter).

### FreeBSD

Not available as a prebuilt download. Can be built [from source](./development/freebsd.md).

### Web

Not supported at this time.
