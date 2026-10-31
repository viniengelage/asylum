# Contributing to Asylum

Thanks for your interest in Asylum. Bug reports, fixes and new features are all welcome.

## Before you start

- For anything larger than a small fix, open an issue or a discussion first so we can agree on
  the approach before you spend time on it.
- If a bug also happens in Zed, it is usually better fixed upstream in
  [zed-industries/zed](https://github.com/zed-industries/zed). Asylum merges upstream regularly.
- Asylum does not depend on any hosted service. Features that need a server should talk to
  services the user configures (their own API keys, their own GitHub account), not to a backend
  run by the project.

## Sending changes

1. Fork the repository and create a branch.
2. Follow the coding guidelines in [CLAUDE.md](CLAUDE.md) (they apply to people too): no
   `unwrap()` in non-test code, no silently discarded errors, full-word variable names, and
   comments only where the *why* is not obvious.
3. Run `./script/clippy` and the tests of the crates you changed (`cargo test -p <crate>`).
4. Open a pull request with a clear, imperative title (for example, `api_client: Add GraphQL
   support`) and a `Release Notes:` section at the end of the description.

Code generated with AI assistance is fine, as long as you have read and understood every line you
submit and tested it yourself.

## Licensing of contributions

Asylum is distributed under GPL-3.0-or-later, with Apache-2.0 for the crates marked as such in
their `Cargo.toml` (see [NOTICE](NOTICE)). By opening a pull request you agree that your
contribution is released under the license of the crate it changes. There is no CLA.

## Resources

- Building: see [Building from source](README.md#building-from-source).
- Zed's developer docs in [docs/src/development](docs/src/development) still describe the
  architecture (GPUI, crates, debugging) accurately.
