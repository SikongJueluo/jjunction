{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  # https://devenv.sh/basics/
  env.GREET = "devenv";

  # https://devenv.sh/packages/
  packages = with pkgs; [
    git
    gitnr
    jujutsu
    just
  ];

  # https://devenv.sh/languages/
  languages.rust = {
    enable = true;
    channel = "stable";
    # version = "latest";
  };

  # https://devenv.sh/processes/
  # processes.dev.exec = "${lib.getExe pkgs.watchexec} -n -- ls -la";

  # https://devenv.sh/services/
  # services.postgres.enable = true;

  # https://devenv.sh/scripts/
  scripts.hello.exec = ''
    echo hello from $GREET
  '';

  # https://devenv.sh/basics/
  enterShell = ''
    hello         # Run scripts directly
    git --version # Use packages

    # keep all jj workspaces in sync (links + direnv allow) without blocking
    if command -v jjn >/dev/null 2>&1; then
      (jjn apply --quiet >/dev/null 2>&1 &)
    fi
  '';

  # https://devenv.sh/tasks/
  # tasks = {
  #   "myproj:setup".exec = "mytool build";
  #   "devenv:enterShell".after = [ "myproj:setup" ];
  # };

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  integrations.gitnr.".gitignore" = {
    templates = [
      "gh:Rust"
      "gh:Nix"
    ];

    content = [
      # Devenv
      ".devenv*"
      "devenv.local.nix"
      "devenv.local.yaml"

      # direnv
      ".direnv"

      # pre-commit
      ".pre-commit-config.yaml"

      # others
      ".env"
      "result"

      # keep lockfile tracked (tool project)
      "!Cargo.lock"
    ];
  };

  # See full reference at https://devenv.sh/reference/options/
}
