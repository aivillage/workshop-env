# nix/container_upload.nix
#
# Usage from dev shell:
#   upload-workshop-sidecar
#   upload-workshop-hub
#   upload-all-images
#
{ pkgs, version, registry ? "ghcr.io/nbhdai" }:

let
  preamble = ''
    set -euo pipefail

    if [[ -z "''${PROJECT_ROOT:-}" ]]; then
      echo "Error: PROJECT_ROOT is not set. Are you in the dev shell?"
      exit 1
    fi

    if [[ -z "''${GITHUB_USERNAME:-}" ]] || [[ -z "''${GHCR_PAT:-}" ]]; then
      echo "Error: GITHUB_USERNAME and GHCR_PAT must be set (source .envhost)"
      exit 1
    fi
  '';

  containers = [
    { name = "workshop-sidecar"; flakeAttr = "workshop-sidecar"; }
    { name = "workshop-hub";     flakeAttr = "workshop-hub"; }
  ];

  mkUploadScript = { name, flakeAttr }:
    pkgs.writeShellScriptBin "upload-${name}" ''
      ${preamble}

      VERSION="${version}"
      echo "--- Building ${name} v$VERSION via nix ---"
      RESULT_LINK="result-${name}"
      nix build "$PROJECT_ROOT#${flakeAttr}" --out-link "$RESULT_LINK"

      echo "Loading into docker..."
      docker load < "$RESULT_LINK"

      # The nix-built image is tagged name:version
      LOCAL_TAG="${name}:$VERSION"

      SEMVER_TAG="${registry}/${name}:$VERSION"
      LATEST_TAG="${registry}/${name}:latest"

      echo "Pushing $SEMVER_TAG"
      docker tag "$LOCAL_TAG" "$SEMVER_TAG"
      docker push "$SEMVER_TAG"

      echo "Pushing $LATEST_TAG"
      docker tag "$LOCAL_TAG" "$LATEST_TAG"
      docker push "$LATEST_TAG"

      # Also tag with git short sha if available
      if GIT_SHA=$(git -C "$PROJECT_ROOT" rev-parse --short HEAD 2>/dev/null); then
        SHA_TAG="${registry}/${name}:$VERSION-$GIT_SHA"
        echo "Pushing $SHA_TAG"
        docker tag "$LOCAL_TAG" "$SHA_TAG"
        docker push "$SHA_TAG"
      fi

      rm -f "$RESULT_LINK"
      echo "✓ ${name} v$VERSION pushed"
    '';

  uploadScripts = map mkUploadScript containers;

  uploadAll = pkgs.writeShellScriptBin "upload-all-images" ''
    ${preamble}
    echo "=== Uploading all workshop images (v${version}) ==="
    ${builtins.concatStringsSep "\n" (map (c: "upload-${c.name}") containers)}
    echo "=== Done ==="
  '';

in {
  scripts = uploadScripts ++ [ uploadAll ];
  packages = builtins.listToAttrs (
    map (s: { name = s.name; value = s; }) (uploadScripts ++ [ uploadAll ])
  );
}