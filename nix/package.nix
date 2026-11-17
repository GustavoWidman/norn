{
  lib,
  craneLib,
}:
let
  root = ../.;

  args = {
    src = lib.fileset.toSource {
      inherit root;
      fileset = craneLib.fileset.commonCargoSources root;
    };
    strictDeps = true;
  };

  cargoArtifacts = craneLib.buildDepsOnly args;
in
craneLib.buildPackage (
  args
  // {
    inherit cargoArtifacts;

    # the tests run once, in `passthru.checks.test`
    doCheck = false;

    passthru.checks = {
      test = craneLib.cargoTest (args // { inherit cargoArtifacts; });
      clippy = craneLib.cargoClippy (
        args
        // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        }
      );
    };

    meta = {
      description = "Cloudflare DNS for a Nix fleet, declared next to the services that use it";
      homepage = "https://github.com/GustavoWidman/norn";
      license = lib.licenses.mit;
      mainProgram = "norn";
    };
  }
)
