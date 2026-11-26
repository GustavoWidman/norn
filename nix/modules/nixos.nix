{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.norn;

  # the unit hides these from itself, so a rollout file in one of them would
  # read as missing and hold the fleet's records for good
  homes = [
    "/home/"
    "/root/"
    "/run/user/"
  ];
  rolloutInHome =
    cfg.rollout.file != null && lib.any (home: lib.hasPrefix home cfg.rollout.file) homes;

  command = lib.escapeShellArgs (
    [
      (lib.getExe cfg.package)
      "converge"
    ]
    ++ cfg.arguments
    ++ [
      "--token-file"
      "%d/token"
      "--lock-wait"
      "10s"
    ]
  );

  unit = {
    wants = [ "network-online.target" ];
    after = [ "network-online.target" ];
    serviceConfig = {
      Type = "oneshot";
      ExecStart = command;
      LoadCredential = "token:${cfg.tokenFile}";
      # a missing LoadCredential= source fails the unit, and with it the
      # switch. this blank fallback reaches norn as no token, which skips the
      # run. systemd ignores a SetCredential= with nothing after the colon, so
      # the fallback is one escaped newline
      SetCredential = "token:\\n";

      DynamicUser = true;
      User = "norn";
      StateDirectory = "norn";
      StateDirectoryMode = "0700";
      # node exporter reads norn.prom here after the unit is gone. a
      # RuntimeDirectory= kept with RuntimeDirectoryPreserve= sits under
      # /run/private when DynamicUser= is on, where no other user can reach
      # it, so root makes the directory before each run, owned by whichever
      # uid the dynamic user has this time, and nothing removes it. the
      # namespace of that first command is set up before the directory
      # exists, hence the "-"
      ExecStartPre = "+${pkgs.coreutils}/bin/install -d -m 0755 -o norn -g norn ${cfg.metrics.directory}";
      ReadWritePaths = [ "-${cfg.metrics.directory}" ];

      TimeoutStartSec = "${toString (cfg.deadline + 60)}s";

      ProtectSystem = "strict";
      ProtectHome = if rolloutInHome then "read-only" else true;
      PrivateTmp = true;
      PrivateDevices = true;
      NoNewPrivileges = true;
      CapabilityBoundingSet = "";
      RestrictAddressFamilies = [
        "AF_UNIX"
        "AF_INET"
        "AF_INET6"
      ];
      SystemCallFilter = [ "@system-service" ];
    };
  };
in
{
  config = lib.mkMerge [
    { services.norn.metrics.directory = "/run/norn"; }

    (lib.mkIf cfg.enable {
      systemd.services.norn = lib.recursiveUpdate unit {
        description = "Bring Cloudflare in line with this configuration";
        wantedBy = [ "multi-user.target" ];
        restartTriggers = [
          cfg.desired
          cfg.package
        ];
        # switch-to-configuration restarts a changed unit only while it is active,
        # so the unit stays active after it exits to rerun exactly when it changed
        serviceConfig.RemainAfterExit = true;
      };

      systemd.services.norn-sweep = unit // {
        description = "Bring Cloudflare back in line with this configuration";
      };

      systemd.timers.norn-sweep = {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "${toString cfg.interval}s";
          OnUnitInactiveSec = "${toString cfg.interval}s";
        };
      };
    })
  ];
}
