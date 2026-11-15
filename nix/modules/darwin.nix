# one daemon does both jobs. its arguments name the binary's and the checked
# desired file's store paths, so the plist changes exactly when the desired
# state, the mode or the binary does; nix-darwin reloads a daemon only when
# its plist changed, and loading runs it (RunAtLoad). StartInterval is the
# timer. launchd never runs two copies of one job, and activation does not
# wait for it.
{
  config,
  lib,
  ...
}:
let
  cfg = config.services.norn;
in
{
  config = lib.mkIf cfg.enable {
    launchd.daemons.norn.serviceConfig = {
      ProgramArguments = [
        (lib.getExe cfg.package)
        "converge"
      ]
      ++ cfg.arguments
      ++ [
        "--token-file"
        cfg.tokenFile
      ];
      RunAtLoad = true;
      StartInterval = cfg.interval;
      ProcessType = "Background";
      StandardOutPath = "/var/log/norn.log";
      StandardErrorPath = "/var/log/norn.log";
    };
  };
}
