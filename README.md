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

jjn apply    # materialize repos to locked commits + links + sync workspaces
jjn doctor  # check config, [[repo]], and [[link]] health

jjn repo add <url> [--target p] [--rev r]   # append manifest entry + clone
jjn repo update [name…]                     # fetch, re-resolve revs, advance lock
jjn repo remove <name>                      # drop entries; files stay on disk
```

Sub-repos are **readonly** vendored git checkouts: a detached HEAD at the
commit recorded in `.jjunction/lock.toml` (machine-generated, tracked in the
outer repo for reproducibility; `jjn apply` restores exactly the locked
commits, `jjn repo update` advances them). Dirty working copies are never
touched — `jjn doctor` reports them. To develop a sub-repo, fork it and point
the `url` at your fork.

Trust the repo once in `~/.config/jjunction/config.toml`:

```toml
trusted-repos = ["/path/to/repo"]
```

Hook it into devenv so every workspace stays ready without blocking the
shell (put this in `devenv.nix`):

```nix
enterShell = ''
  if command -v jjn >/dev/null 2>&1; then
    (jjn apply --quiet >/dev/null 2>&1 &)
  fi
'';
```

And let direnv re-trigger that hook when the workspace set or jjunction
config changes (in the default workspace's `.envrc`, after `use devenv`):

```bash
# only the default workspace has a .jj/repo directory
[ -d .jj/repo ] && watch_file .jj/repo/workspace_store/index
watch_file .jjunction/config.toml
```

With this, `jj workspace add …` at a prompt in the default workspace
re-syncs all workspaces at the next prompt — no daemon required.

## Development

```sh
devenv shell   # or use direnv
just test      # run tests
just lint      # clippy
```

## License

GPL-3.0-or-later. See [LICENSE](./LICENSE).
