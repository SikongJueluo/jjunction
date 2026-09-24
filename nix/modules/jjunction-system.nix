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
  };

  config = lib.mkIf cfg.enable {
    # Completions ship in the standard share dirs; on NixOS they activate
    # through the shells' own options (programs.fish.enable links
    # /share/fish, programs.bash.completion.enable links bash-completion).
    environment.systemPackages = [ cfg.package ];
  };
}
