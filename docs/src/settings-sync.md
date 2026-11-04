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

Each [profile](./configuring-zed.md) syncs its own files to its own gist:

- its `settings.json`, `tasks.json`, `debug.json` and `AGENTS.md`
- its snippets
- its personal task agents and flows

The files every profile shares go to one more gist, named "Asylum shared settings sync":

- the shared `settings.json` that sits below every profile's own settings
- `keymap.json` and your themes
- the list of profiles, so a new machine gets the same profiles

Only one running profile syncs the shared gist at a time; the others skip it until their next sync. API keys and other secrets live in your keychain and are never synced.

A profile gist created before the shared gist existed may still hold a copy of `keymap.json` and your themes. Asylum stops syncing those copies there and leaves them untouched, so you can delete them from the gist yourself.

## How changes are merged

Asylum syncs a few seconds after it starts and then every five minutes. For each file it compares both sides with the version from the last sync:

- a file changed on one side only is copied to the other side, including deletions;
- a file changed on both sides keeps this machine's copy, and the other copy is saved in the `settings_sync/backups` folder of Asylum's data directory;
- the first sync on a new machine keeps the copy from GitHub instead, so signing in brings your settings over rather than replacing them with defaults.

The gist keeps every revision, so an older version of any file can also be restored from GitHub.

## Signing out

**Sign Out** removes the saved sign-in from the keychain and stops syncing. Your files and the gist are left untouched. To revoke Asylum's access entirely, remove the app under **Settings → Applications → Authorized GitHub Apps** on GitHub.
