---
title: Code Completions - Asylum
description: Asylum's code completions from language servers and edit predictions. Configure autocomplete behavior, snippets, and documentation display.
---

# Completions

Asylum supports two sources for completions:

1. "Code Completions" provided by Language Servers (LSPs) automatically installed by Asylum or via [Asylum Language Extensions](languages.md).
2. "Edit Predictions" provided by external providers like [GitHub Copilot](./ai/edit-prediction.md#github-copilot) or by a [local model](./ai/edit-prediction.md#local-and-self-hosted-models).

## Language Server Code Completions {#code-completions}

When there is an appropriate language server available, Asylum will provide completions of variable names, functions, and other symbols in the current file. You can disable these by adding the following to your Asylum `settings.json` file:

```json [settings]
"show_completions_on_input": false
```

You can manually trigger completions with `ctrl-space` or by triggering the `editor::ShowCompletions` action from the command palette.

> Note: Using `ctrl-space` in Asylum requires disabling the macOS global shortcut.
> Open **System Settings** > **Keyboard** > **Keyboard Shortcut**s >
> **Input Sources** and uncheck **Select the previous input source**.

For more information, see:

- [Configuring Supported Languages](./configuring-languages.md)
- [List of Asylum Supported Languages](./languages.md)

## Edit Predictions {#edit-predictions}

Asylum supports predicting multiple edits at a time through [edit prediction providers](./ai/edit-prediction.md), including locally hosted [Zeta](https://huggingface.co/zed-industries/zeta), Zed Industries' open-source and open-data model.
Edit predictions appear as you type, and most of the time, you can accept them by pressing `tab`.

See the [edit predictions documentation](./ai/edit-prediction.md) for more information on how to setup and configure Asylum's edit predictions.
