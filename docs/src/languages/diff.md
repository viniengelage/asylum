---
title: Diff
description: "Configure Diff language support in Asylum, including language servers, formatting, and debugging."
---

# Diff

Diff support is available natively in Asylum.

- Tree-sitter: [zed-industries/the-mikedavis/tree-sitter-diff](https://github.com/the-mikedavis/tree-sitter-diff)

## Configuration

Asylum will not attempt to format diff files and has [`remove_trailing_whitespace_on_save`](../reference/all-settings.md#remove-trailing-whitespace-on-save) and [`ensure-final-newline-on-save`](../reference/all-settings.md#ensure-final-newline-on-save) set to false.

Asylum will automatically recognize files with `patch` and `diff` extensions as Diff files. To recognize other extensions, add them to `file_types` in your Asylum settings.json:

```json [settings]
  "file_types": {
    "Diff": ["dif"]
  },
```
