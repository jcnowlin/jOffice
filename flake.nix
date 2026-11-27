{
  description = "jOffice — Euro-Office editors in a native Rust/GTK4 shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      pkgSet = import ./nix/default.nix { inherit pkgs; };
    in
    {
      packages.${system} = {
        inherit (pkgSet) joffice joffice-runtime joffice-fonts;
        default = pkgSet.joffice;
      };
    };
}
