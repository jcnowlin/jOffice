# jOffice home-manager module (marathon 2 cycle 9).
#
# EXACTLY three options — enable, package, makeDefault
# (KAO cycle-9 rounds 1+2; the cut list forbids more).
# No settings, no service, no activation script: cycle 8's
# settings live in a writable state dir the store must
# never own, and defaults stay with the user's own
# officeSuite switch — this module writes mimeApps ONLY
# under makeDefault, which defaults to false, so adopting
# the module changes no default application.
{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.joffice;
  joMimeTypes = [
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    "application/msword"
    "application/vnd.ms-excel"
    "application/vnd.ms-powerpoint"
    "application/vnd.oasis.opendocument.text"
    "application/vnd.oasis.opendocument.spreadsheet"
    "application/vnd.oasis.opendocument.presentation"
    "application/pdf"
  ];
in
{
  options.programs.joffice = {
    enable = lib.mkEnableOption "jOffice (Euro-Office editors in a native shell)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.default;
      defaultText = lib.literalExpression "jOffice.packages.\${system}.default";
      description = "The jOffice package to install.";
    };

    makeDefault = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Make jOffice the default application for the office
        document types and PDF. False (the default) writes no
        mimeApps entries at all: existing defaults — e.g. a
        LibreOffice suite switch elsewhere in the config —
        remain the single owner of default applications.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    # makeDefault is the module's only contact with
    # mimeApps (round 2: "no mimeApps beyond makeDefault").
    # home-manager 25.05 gates the mimeapps.list writer
    # behind xdg.mimeApps.enable (default false), so
    # makeDefault enables it — at mkDefault priority, so
    # an explicit setting in the user's own config (e.g.
    # their suite switch) always wins.
    xdg.mimeApps.enable = lib.mkIf cfg.makeDefault (lib.mkDefault true);
    xdg.mimeApps.defaultApplications = lib.mkIf cfg.makeDefault
      (lib.genAttrs joMimeTypes (_: "joffice.desktop"));
  };
}
