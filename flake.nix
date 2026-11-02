
{
  description = "wcc Rust command-line tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          # Deliberately no cargo/rustc here: Nix evaluates/builds shell
          # dependencies before shellHook, so PATH checks cannot avoid Rust downloads.
          default = pkgs.mkShell {
            packages = with pkgs; [ pkg-config openssl ];
            shellHook = ''
              if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
                echo "wcc: using existing Rust: $(rustc --version)"
                echo "wcc: cargo: $(command -v cargo)"
              else
                echo "wcc: Rust not found in PATH. For Nix-managed Rust run: nix develop .#nix-rust"
              fi
            '';
          };

          # Explicit opt-in fallback, only downloads Nix Rust when requested.
          nix-rust = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc pkg-config openssl ];
            shellHook = ''
              echo "wcc: using Nix Rust: $(rustc --version)"
            '';
          };
        });

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
