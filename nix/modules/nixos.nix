{
  config,
  lib,
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
    ++ lib.optionals cfg.push.enable [
      "--push-vapid-key-file"
      "%d/push-vapid"
      "--push-subscriptions-file"
      "%d/push-subscriptions"
    ]
  );

  credentials = {
    token = cfg.tokenFile;
  }
  // lib.optionalAttrs cfg.push.enable {
    push-vapid = cfg.push.vapidKeyFile;
    push-subscriptions = cfg.push.subscriptionsFile;
  };

  unit = {
    wants = [ "network-online.target" ];
    after = [ "network-online.target" ];
    serviceConfig = {
      Type = "oneshot";
      ExecStart = command;
      LoadCredential = lib.mapAttrsToList (name: file: "${name}:${file}") credentials;
      # a missing LoadCredential= source fails the unit, and with it the
      # switch. these blank fallbacks reach norn as no token, which skips the
      # run, and as push files it cannot use, which it logs. systemd ignores
      # a SetCredential= with nothing after the colon, so each fallback is
      # one escaped newline
      SetCredential = map (name: "${name}:\\n") (lib.attrNames credentials);

      DynamicUser = true;
      User = "norn";
      StateDirectory = "norn";
      StateDirectoryMode = "0700";

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
  config = lib.mkIf cfg.enable {
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
  };
}
