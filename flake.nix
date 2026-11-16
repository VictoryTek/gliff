{
  description = "gliff: remote desktop over SSH (VictoryTek fork, adds a GNOME backend)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        gliff = pkgs.callPackage ./nix/package.nix { };
        default = gliff;
      });

      nixosModules.default = import ./nix/module.nix self;

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.gliff ];
          packages = [ pkgs.clippy pkgs.rustfmt ];
        };
      });
    };
}
