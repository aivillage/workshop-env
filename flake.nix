{
  description = "workshop-env: Kubernetes workshop environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      flake-parts,
      fenix,
      ...
    }:
    let
      # ── Single source of truth for the release version ────────
      version = "0.1.0";
    in
    flake-parts.lib.mkFlake { inherit inputs; } {

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        {
          pkgs,
          system,
          lib,
          ...
        }:
        let
          # ── Rust toolchain ──────────────────────────────────────
          rustToolchain = fenix.packages.${system}.stable.toolchain;

          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };

          commonBuildInputs = with pkgs; [ openssl ];

          commonNativeBuildInputs = with pkgs; [
            pkg-config
            cmake
          ];

          # ── Rust binaries ───────────────────────────────────────
          mkCrateBin =
            { pname, crate }:
            rustPlatform.buildRustPackage {
              inherit pname version;
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              buildInputs = commonBuildInputs;
              nativeBuildInputs = commonNativeBuildInputs;
              buildAndTestSubdir = crate;
              env.LD_LIBRARY_PATH = "${lib.makeLibraryPath [ pkgs.openssl ]}";
              cargoBuildFlags = [
                "-p"
                (baseNameOf crate)
              ];
              doCheck = false;

              meta.mainProgram = baseNameOf crate;
            };

          sidecar-bin = mkCrateBin {
            pname = "workshop-sidecar";
            crate = "sidecar";
          };
          hub-bin = mkCrateBin {
            pname = "workshop-hub";
            crate = "hub";
          };

          # ── Docker images (nix-built, no daemon needed) ─────────
          mkImage =
            { name, bin }:
            pkgs.dockerTools.buildImage {
              inherit name;
              tag = version;
              config.Cmd = [ "${bin}/bin/${bin.meta.mainProgram}" ];
            };

          workshop-sidecar = mkImage {
            name = "workshop-sidecar";
            bin = sidecar-bin;
          };
          workshop-hub = mkImage {
            name = "workshop-hub";
            bin = hub-bin;
          };

          # ── Upload scripts ──────────────────────────────────────
          containerUpload = import ./nix/container_upload.nix {
            inherit pkgs version;
            registry = "ghcr.io/aivillage";
          };

          devShell = pkgs.mkShell {
            name = "workshop-env-dev";
            packages = containerUpload.scripts ++ [
              rustToolchain
              pkgs.docker
              pkgs.git
              pkgs.cmake
              pkgs.pkg-config
              pkgs.openssl
              pkgs.kubectl
              pkgs.k9s
              pkgs.talosctl
            ];
            shellHook = ''
              export PROJECT_ROOT=$PWD
              export WORKSHOP_VERSION="${version}"
              export TALOS_DIR="''${TALOS_DIR:-$PROJECT_ROOT/.talos}"
              if [ -f "$TALOS_DIR/kubeconfig" ]; then
                export KUBECONFIG="''${KUBECONFIG:-$TALOS_DIR/kubeconfig}"
                export TALOSCONFIG="''${TALOSCONFIG:-$TALOS_DIR/talosconfig}"
              fi
              export LD_LIBRARY_PATH="${lib.makeLibraryPath [ pkgs.openssl ]}:''${LD_LIBRARY_PATH:-}"

              if [ -f .envhost ]; then
                set -a
                source .envhost
                set +a
                if [ -n "''${GITHUB_USERNAME:-}" ] && [ -n "''${GHCR_PAT:-}" ]; then
                  echo "Logging into ghcr.io..."
                  echo "$GHCR_PAT" | docker login ghcr.io -u "$GITHUB_USERNAME" --password-stdin
                fi
              fi
            '';
          };
        in
        {
          devShells = {
            default = devShell;
          };

          packages = containerUpload.packages // {
            inherit
              sidecar-bin
              hub-bin
              workshop-sidecar
              workshop-hub
              ;
          };

          checks = {
            workshop-tests = rustPlatform.buildRustPackage {
              pname = "workshop-tests";
              inherit version;
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;
              buildInputs = commonBuildInputs;
              nativeBuildInputs = commonNativeBuildInputs;
              env.LD_LIBRARY_PATH = "${lib.makeLibraryPath [ pkgs.openssl ]}";
              doCheck = true;
            };
          };
        };
    };
}
