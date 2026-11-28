# jOffice packaging (marathon 2 cycle 4, step 5).
#
#   nix-build nix/default.nix -A joffice        # the shell (+ runtime)
#   nix-build nix/default.nix -A joffice-runtime
#   nix-build nix/default.nix -A joffice-fonts
#
# A flake.nix wraps the same attributes for flake users.
{ pkgs ? import <nixpkgs> { } }:

{
  joffice = pkgs.callPackage ./shell.nix { };
  joffice-runtime = pkgs.callPackage ./runtime.nix { };
  joffice-fonts = pkgs.callPackage ./fonts.nix { };
  joffice-converter = pkgs.callPackage ./converter.nix { };
}
