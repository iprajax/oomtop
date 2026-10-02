{
  description = "oomtop: see the OOM coming. True memory accounting, attribution and headroom for local AI work.";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages = forAll (pkgs: rec {
        oomtop = pkgs.rustPlatform.buildRustPackage {
          pname = "oomtop";
          inherit (cargoToml.workspace.package) version;

          # Only what cargo needs: edits to docs, media or the site don't trigger a rebuild.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;

          cargoBuildFlags = [
            "-p"
            "oomtop-cli"
          ];
          # The workspace test suite (fmt, clippy, tests, snapshots) runs in CI (.github/workflows/ci.yml); the
          # Nix build stays a pure build of the binary, and the install check below proves it runs.
          doCheck = false;
          doInstallCheck = true;
          installCheckPhase = ''
            runHook preInstallCheck
            $out/bin/oomtop --version | grep -q "oomtop ${cargoToml.workspace.package.version}"
            runHook postInstallCheck
          '';

          meta = {
            inherit (cargoToml.workspace.package) description;
            homepage = "https://github.com/iprajax/oomtop";
            license = pkgs.lib.licenses.mit;
            mainProgram = "oomtop";
            platforms = pkgs.lib.platforms.linux ++ pkgs.lib.platforms.darwin;
          };
        };
        default = oomtop;
      });

      apps = forAll (pkgs: rec {
        oomtop = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.oomtop}/bin/oomtop";
          meta.description = "oomtop TUI and CLI";
        };
        default = oomtop;
      });

      checks = forAll (pkgs: {
        build = self.packages.${pkgs.stdenv.hostPlatform.system}.oomtop;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.oomtop ];
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            cargo-insta
            cargo-nextest
            vhs
          ];
        };
      });

      formatter = forAll (pkgs: pkgs.nixfmt);
    };
}
