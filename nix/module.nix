# NixOS module: installs gliff and gliff-server and enables the GPU userspace
# (Vulkan and VA-API drivers come from hardware.graphics).
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.gliff;
in
{
  options.programs.gliff = {
    enable = lib.mkEnableOption "gliff, a remote desktop that needs only SSH";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "gliff.packages.\${system}.default";
      description = "The gliff package to install.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    hardware.graphics.enable = lib.mkDefault true;
  };
}
