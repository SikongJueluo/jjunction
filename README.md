# jjunction

A collection of tools for the [Jujutsu](https://github.com/jj-vcs/jj) (jj) version control system, built on [`jj-lib`](https://crates.io/crates/jj-lib).

## Usage

```sh
# declare links in the repo (untrusted until trusted)
# .jjunction/config.toml:
#   [[link]]
#   source = "assets/shared.txt"   # or absolute / ~/ paths
#   target = "links/shared.txt"     # relative to repo root
#   type = "link"
#
#   [[repo]]
#   url = "https://github.com/org/repo.git"   # vendored sub-repo
#   target = "third_party/repo"                # default: name
#   rev = "main"                               # branch/tag/sha; default: floating
#
#   [workspace]
#   allow = "auto"   # direnv allow secondary workspaces when .envrc matches

jjn trust    # trust this workspace (direnv-allow style)
jjn apply    # materialize repos to locked commits + links + sync workspaces
jjn doctor  # check config, [[repo]], and [[link]] health

jjn repo add <url> [--target p] [--rev r]   # clone + append manifest (atomic)
jjn repo list                               # entries + materialization state
jjn repo sync                               # converge everything to the lock
jjn repo update [name…]                     # fetch, re-resolve revs, advance lock
jjn repo remove <name>                      # drop entries; files stay on disk
```

Sub-repos are **readonly** vendored git checkouts: a detached HEAD at the
commit recorded in `.jjunction/lock.toml` (machine-generated, tracked in the
outer repo for reproducibility; `jjn apply` restores exactly the locked
commits, `jjn repo update` advances them). Dirty working copies are never
touched — `jjn doctor` reports them. To develop a sub-repo, fork it and point
the `url` at your fork. Network operations relay git's own progress onto a
terminal progress bar (silent when piped or `--quiet`).

Trust the repo once (like `direnv allow`) — every gated message points here:

```sh
jjn trust
```

(equivalent to appending the workspace root to `trusted-repos` in
`~/.config/jjunction/config.toml`)

Shell completions (Nix installs these automatically):

```sh
jjn completions fish | source        # fish
source <(jjn completions bash)      # bash
```

Wire the reaction loop once — `jjn init` is idempotent and marker-scoped: it
appends a managed block to `.envrc` (watch_file the jj workspace index and
`.jjunction/config.toml`, guarded so secondary workspaces skip the `.jj/repo`
path) and writes the background `jjn apply --quiet` enterShell hook into
`devenv.local.nix` (auto-loaded by devenv, gitignored, owned by jjn):

```sh
jjn init
```

With that, `jj workspace add …` or a config edit re-syncs everything at the
next prompt — no daemon required.

## Installation

NixOS / Home Manager (flakes):

```nix
# flake input
inputs.jjunction.url = "github:sikongjueluo/jjunction";

# NixOS module
programs.jjunction.enable = true;   # imports jjunction.nixosModules.jjunction

# or Home Manager
programs.jjunction.enable = true;   # imports jjunction.homeManagerModules.jjunction
```

Ad-hoc: `nix run github:sikongjueluo/jjunction`, or `nix build .#jjunction` from
a checkout. The package wraps `git` and `direnv` into `jjn`'s PATH, so it works
in pure shells.

## Development

```sh
devenv shell   # or use direnv
just test      # run tests
just lint      # clippy
```

## License

GPL-3.0-or-later. See [LICENSE](./LICENSE).
