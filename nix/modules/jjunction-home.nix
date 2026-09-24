{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.jjunction;
in
{
  meta.maintainers = [ ];

  options.programs.jjunction = {
    enable = lib.mkEnableOption "jjunction (jjn, tools for Jujutsu workspaces)";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../packages/jjunction.nix { root = ../..; };
      defaultText = lib.literalExpression "pkgs.callPackage ../packages/jjunction.nix { }";
      description = "The jjunction package providing the `jjn` CLI.";
    };

    enableBashIntegration = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to enable bash completions.";
    };

    enableFishIntegration = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to enable fish completions.";
    };

    enableZshIntegration = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to enable zsh completions.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    programs.bash.initExtra = lib.mkIf cfg.enableBashIntegration ''
      if [[ $- == *i* ]]; then
        source <(${lib.getExe cfg.package} completions bash)
      fi
    '';

    programs.zsh.initExtra = lib.mkIf cfg.enableZshIntegration ''
      source <(${lib.getExe cfg.package} completions zsh)
    '';

    programs.fish.interactiveShellInit = lib.mkIf cfg.enableFishIntegration ''
      ${lib.getExe cfg.package} completions fish | source
    '';
  };
}
