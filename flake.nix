{
  description = "Proxy StatusNotifierItems as XEmbed system tray icons";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.gtk3 ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        trayproxy = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.wrapGAppsHook3 ];
            buildInputs = commonArgs.buildInputs ++ [ pkgs.hicolor-icon-theme ];
            meta = {
              description = "Proxy StatusNotifierItems as XEmbed system tray icons";
              mainProgram = "trayproxy";
              platforms = pkgs.lib.platforms.linux;
            };
          }
        );
      in
      {
        packages = {
          default = trayproxy;
          inherit trayproxy;
        };

        apps.default = flake-utils.lib.mkApp { drv = trayproxy; };

        checks = {
          inherit trayproxy;
          clippy = craneLib.cargoClippy (commonArgs // { inherit cargoArtifacts; });
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = [
            pkgs.rust-analyzer
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      }
    )
    // {
      overlays.default = final: _prev: {
        trayproxy = self.packages.${final.stdenv.hostPlatform.system}.default;
      };
    };
}
