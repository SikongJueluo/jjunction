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
#   [workspace]
#   allow = "auto"   # direnv allow secondary workspaces when .envrc matches

jjn apply    # create links + sync all jj workspaces (default→others)
jjn doctor  # check config and link health
```

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
