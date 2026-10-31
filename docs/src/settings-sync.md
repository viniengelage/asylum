---
title: Settings Sync
description: Sign in with GitHub to keep Asylum's settings, keymap and agents in sync across machines.
---

# Settings Sync

Asylum can keep your configuration in sync across machines through your GitHub account. There is no Asylum server involved: the files are stored in a **private gist** on your account, and you can see, edit or delete it on GitHub at any time.

## Signing in

Click **Sign In** in the title bar, or run `settings sync: sign in` from the command palette. Asylum shows a short code; choose **Copy Code and Open GitHub**, paste the code on the page that opens and authorize the Asylum app. The sign-in is saved in your system keychain.

Once signed in, your avatar replaces the button. Its menu shows when the last sync happened and has **Sync Settings Now** and **Sign Out**.

## What is synced

- `settings.json`, `keymap.json`, `tasks.json`, `debug.json` and `AGENTS.md`
- snippets and themes from your config folder
- your personal task agents and flows

API keys and other secrets live in your keychain and are never synced. Each [profile](./configuring-zed.md) syncs to its own gist.

## How changes are merged

Asylum syncs a few seconds after it starts and then every five minutes. For each file it compares both sides with the version from the last sync:

- a file changed on one side only is copied to the other side, including deletions;
- a file changed on both sides keeps this machine's copy, and the other copy is saved in the `settings_sync/backups` folder of Asylum's data directory;
- the first sync on a new machine keeps the copy from GitHub instead, so signing in brings your settings over rather than replacing them with defaults.

The gist keeps every revision, so an older version of any file can also be restored from GitHub.

## Signing out

**Sign Out** removes the saved sign-in from the keychain and stops syncing. Your files and the gist are left untouched. To revoke Asylum's access entirely, remove the app under **Settings → Applications → Authorized GitHub Apps** on GitHub.
