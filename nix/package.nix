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

    meta.mainProgram = "norn";
  }
)
