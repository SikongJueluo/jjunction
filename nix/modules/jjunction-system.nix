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
    environment.systemPackages = [ cfg.package ];
  };
}
