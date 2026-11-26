{
  config,
  lib,
  ...
}:
let
  cfg = config.services.norn;
in
{
  config = lib.mkMerge [
    # launchd runs norn as root, which makes the directory, readable by all
    { services.norn.metrics.directory = "/var/run/norn"; }

    (lib.mkIf cfg.enable {
      # nix-darwin reloads a daemon only when its plist changed, and loading runs
      # it (RunAtLoad). the arguments name store paths, so the plist changes
      # exactly when the desired file, the mode or the binary does
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
    })
  ];
}
