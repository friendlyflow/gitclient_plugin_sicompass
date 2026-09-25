# Project Instructions

gitclient-plugin-sicompass was split out of the
[sicompass](https://github.com/friendlyflow/sicompass) workspace, and its git
history before that point is the history of `lib/lib_gitclient` there. Work on it is
usually driven from a sicompass checkout next to this one (`../sicompass`),
whose `/commit-and-push`, `/release`, `/sync` and `/update-cargo` take this
repo's name as their first argument and then follow the skills in this repo's
`.claude/skills/`.

It is a sicompass **WASM plugin**: a `cdylib` built for `wasm32-wasip2` with
`sicompass-pdk`, installed by the sicompass Store from this repo's GitHub
releases. The plugin platform is described in
`../sicompass/docs/plugin-platform.md` and `../sicompass/docs/wasm-plugins.md`.

- `plugin.json` is the manifest. Its `name` is `gitclient` and its
  `displayName` `git client` is the settings section (`gitAutofetchMinutes`,
  the same key the built-in had, so a saved value carries over). It asks for
  `"filesystem": ["/"]` (the folder listing picks the repository) and
  `"process": ["git"]`, which the user approves at install.
- `locales/<lang>.ftl`, every id prefixed `gitclient-`, in all four
  languages. A test checks every id is used (in the code or the manifest) and
  every one the code asks for exists.

## The sandbox, and what it changes

- **git** runs through the host's `process` interface (`src/git.rs`): the
  same arguments and environment as natively, with `GIT_DIR` and its kind
  removed through `unset`. The host reports an exit only once the output has
  all arrived, so reading until then gets all of it. Arguments cross as strings,
  so a file name that is not valid UTF-8 reaches git with its bad bytes
  replaced. There is no `gitBinary` setting: the host starts only `git`, by
  name, from `PATH` or `~/.local/bin`.
- **fetch, pull and push** run as a host task (`worker::TASK`, a second
  instance of the plugin) so the UI never waits on the network. Natively the
  same job runs on a thread, through the same encode and decode, so the tests
  cover the trip.
- **The watcher** that notices commits made elsewhere stats `.git` from
  `poll`, at most once a second, instead of from a thread.
- **Symlinks** in the folder listing are resolved through `desktop.read-link`
  (`src/fsx.rs`), since WASI never follows an absolute one.
- Opening a repository moves the plugin without a navigation call. The pdk's
  `export_plugin!` tells the host (`host.moved-to`), so nothing here has to.

## Environment (Nix)

The toolchain comes from the flake dev shell in [flake.nix](flake.nix): Rust
from rust-overlay with the `wasm32-wasip2` target (nixpkgs' rustc has no `std`
for it), `wasm-tools` and `jq`. Nothing is installed system-wide.

- **Check once per session**, then stick with the answer: `command -v cargo`.
  - Non-empty: the shell is inside `nix develop`, so run `cargo ...` directly.
  - Empty: prefix every toolchain command with `nix develop -c`.
- `nix develop -c <cmd>` prints a `warning: Git tree ... is dirty` line on
  stderr first. That warning is noise, not a failure.
- Evaluate the flake through `git+file://$PWD`, never a plain path (a plain path
  copies `target/` into the store and hangs), and always under `timeout`.
- The version lives in `plugin.json` and in `[package] version` in `Cargo.toml`.
  Bump both together.

## Generated files that are committed

- `THIRD-PARTY-LICENSES.html`: `cargo about generate about.hbs -o
  THIRD-PARTY-LICENSES.html` (cargo-about 0.9.2, the version the `licenses.yml`
  workflow pins). Regenerate and commit it with any dependency change. The
  workflow fails if it drifts.

## Code Style

Follow standard Rust idioms. Use `#[allow(...)]` sparingly and only when
justified. In `README.md`, do not use em dashes or semicolons. Use commas
instead, or split into separate sentences.

## Testing

- After implementing changes, always run the tests before finishing:
  `cargo test` (natively), and `./scripts/release-plugin.sh --dry-run`, which
  also builds the component and audits its imports.
- When adding new code, write or update tests.
- If tests fail, fix the code. Never leave a task with failing tests.

## Test Integrity

- Never remove or weaken test assertions to make a failing test pass. Fix the
  code instead.
- If a test itself is genuinely wrong and needs changing, **ask the user
  first** before modifying it.

## Releasing

A release is a `vX.Y.Z` tag on `main`, equal to `plugin.json`'s version. See
`.claude/skills/release/SKILL.md`. Before tagging, run
`nix develop -c ./scripts/release-plugin.sh --dry-run` (needs the
`sicompass-plugin` tool: `cargo install --git
https://github.com/friendlyflow/sicompass-plugin-sdk sicompass-plugin`). The
release workflow signs with the `PLUGIN_SIGNING_KEY` secret and checks it
against the `PLUGIN_PUBLIC_KEY` variable, the key the sicompass store list
names. The secret key file is `~/.config/sicompass/plugin-keys/gitclient.key`
on the maintainer's machine. Never print, copy or commit it.

The SDK and the pdk come from crates.io (the source is
`../sicompass-plugin-sdk`). The commented-out `[patch]` in `Cargo.toml` is for
working on them together, and stays commented on main.
