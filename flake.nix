
{
  description = "wcc Rust command-line tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          wcc = pkgs.rustPlatform.buildRustPackage {
            pname = "wcc";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;

            meta = {
              description = "wcc Rust command-line tools";
              mainProgram = "wcc";
              platforms = [ "x86_64-linux" ];
            };
          };

          default = self.packages.${system}.wcc;
        });
    };
}
