{
  description = "workshop-env: Kubernetes workshop environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        { pkgs, system, ... }:
        let
          cliTools = with pkgs; [
            curl
            talosctl
            kubectl
            kubernetes-helm
            tilt
            openssl
            zsh
            k9s
            cilium-cli
            hubble
            sops
            ssh-to-age
          ];

          deployShell = pkgs.mkShell {
            name = "workshop-env-deploy";
            packages = cliTools;
            shellHook = ''
              export PROJECT_ROOT=$PWD
              export TALOS_DIR="$PROJECT_ROOT/.talos"
              export KUBECONFIG="$TALOS_DIR/kubeconfig"
              export TALOSCONFIG="$TALOS_DIR/talosconfig"
            '';
          };
        in
        {
          devShells.default = deployShell;
        };
    };
}
