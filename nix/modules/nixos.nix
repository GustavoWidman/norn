# two units over one program:
#
# - norn.service runs at boot and whenever a switch changes its text. its
#   command line names the checked desired file and the package by store
#   path, so the text changes exactly when the desired state, the mode or the
#   binary does. it remains active after it exits, so switch-to-configuration
#   restarts it on such a change and leaves it alone otherwise. the switch
#   waits for it, within the program's own deadline, and the program exits 0
#   whatever Cloudflare does.
# - norn-sweep.service, on a timer, repairs drift and retries what did not
#   apply.
#
# both run as one dynamic user with one state directory, and the program's
# lock on it keeps them from running at once: a run waits for the lock at
# most 10 seconds, then leaves its work to the next sweep.
{
  config,
  lib,
  ...
}:
let
  cfg = config.services.norn;

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

      DynamicUser = true;
      User = "norn";
      StateDirectory = "norn";
      StateDirectoryMode = "0700";

      # the program gives up and exits 0 on its own watchdog, lock wait plus
      # deadline plus 5 seconds; this only bounds a process that outlives it
      TimeoutStartSec = "${toString (cfg.deadline + 60)}s";

      ProtectSystem = "strict";
      ProtectHome = true;
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
        # spreads the fleet's writes of the same fleet records apart
        RandomizedDelaySec = "30s";
      };
    };
  };
}
