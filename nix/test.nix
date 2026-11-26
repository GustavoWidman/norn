{ self }:
{
  name = "norn";

  nodes.machine =
    { config, pkgs, ... }:
    {
      imports = [ self.nixosModules.default ];

      cloudflare = {
        zones = [ "example.com" ];
        address.ipv4 = "192.0.2.10";
        dns."machine.example.com".a = { };
        fleet.dns."shared.example.com".a.addresses = [ "192.0.2.20" ];
        account = "0123456789abcdef0123456789abcdef";
        fleet.workers.offline = {
          module = pkgs.writeText "worker.js" "export default { fetch: (request) => fetch(request) };";
          compatibilityDate = "2026-11-01";
          routes = [ "shared.example.com/*" ];
          bypass = [ "shared.example.com/api/*" ];
        };
      };

      services.norn = {
        enable = true;
        mode = "report";
        tokenFile = "/run/norn-test/token";
        revision = 1794942959;
        rollout.file = "/home/deploy/rollout.json";
        peers.machine = config.cloudflare.claims;
      };

      services.prometheus.exporters.node = {
        enable = true;
        extraFlags = [ "--collector.textfile.directory=${config.services.norn.metrics.directory}" ];
      };
      environment.systemPackages = [ pkgs.curl ];

      specialisation.restartsNorn.configuration.cloudflare.dns."www.example.com".a = { };
    };

  # the vm has no network, so a run that reaches cloudflare cannot read it
  testScript =
    { nodes, ... }:
    ''
      import json
      from datetime import datetime

      def status():
          return json.loads(machine.succeed("cat /var/lib/norn/status.json"))

      def ran_cleanly(unit):
          result = machine.succeed(f"systemctl show --property=Result --value {unit}").strip()
          code = machine.succeed(f"systemctl show --property=ExecMainStatus --value {unit}").strip()
          assert (result, code) == ("success", "0"), (unit, result, code)

      def healthy_at(status):
          return int(datetime.fromisoformat(status["troubledSince"] or status["finished"]).timestamp())

      # the textfile and its directory belong to the units' dynamic user, not
      # root, and anyone can read them
      def readable_by_all():
          modes = machine.succeed("stat -c '%a %u' /run/norn /run/norn/norn.prom").split()
          assert modes[0::2] == ["755", "644"] and modes[1] == modes[3] != "0", modes

      # what node exporter serves of norn's textfile
      def gauge():
          metrics = machine.succeed("curl -sf http://127.0.0.1:9100/metrics")
          assert "node_textfile_scrape_error 0" in metrics, metrics
          values = [line.split()[1] for line in metrics.splitlines() if line.startswith("norn_healthy_timestamp_seconds ")]
          assert len(values) == 1, metrics
          return float(values[0])

      machine.wait_for_unit("multi-user.target")

      with subtest("without a token, boot runs norn, which skips and fails nothing"):
          machine.wait_for_unit("norn.service")
          ran_cleanly("norn.service")
          assert status()["health"] == "skipped", status()
          machine.succeed("systemctl start norn-sweep.service")
          ran_cleanly("norn-sweep.service")
          failed = machine.succeed("systemctl list-units --failed --plain --no-legend")
          assert failed.strip() == "", failed

      with subtest("node exporter serves when norn was last healthy, after the sweep has exited"):
          machine.wait_for_unit("prometheus-node-exporter.service")
          machine.wait_for_open_port(9100)
          assert status()["troubledSince"] is not None, status()
          since = healthy_at(status())
          assert gauge() == since, (gauge(), status())
          machine.sleep(1)
          machine.succeed("systemctl start norn-sweep.service")
          ran_cleanly("norn-sweep.service")
          machine.fail("systemctl is-active norn-sweep.service")
          assert healthy_at(status()) == since, status()
          assert int(datetime.fromisoformat(status()["finished"]).timestamp()) > since, status()
          readable_by_all()
          assert gauge() == since, (gauge(), status())

      with subtest("each run takes the textfile's directory back, whoever owns it"):
          machine.succeed("chown root:root /run/norn", "chmod 0700 /run/norn", "chown root:root /run/norn/norn.prom")
          machine.succeed("systemctl start norn-sweep.service")
          ran_cleanly("norn-sweep.service")
          readable_by_all()
          assert gauge() == since, (gauge(), status())

      with subtest("without a token, a switch that restarts norn succeeds"):
          before = status()["finished"]
          machine.succeed(
              "${nodes.machine.system.build.toplevel}/specialisation/restartsNorn/bin/switch-to-configuration test"
          )
          ran_cleanly("norn.service")
          assert status()["finished"] != before, "the switch did not restart norn.service"
          assert status()["health"] == "skipped", status()

      with subtest("norn status reports the skipped runs once they go stale"):
          machine.succeed("norn status | grep '^skipped'")
          machine.sleep(2)
          stale = machine.fail("norn status --stale 1s")
          assert stale.startswith("skipped"), stale

      with subtest("with a token, norn runs, exits 0 and stays active"):
          machine.succeed(
              "install -d -m 0700 /run/norn-test",
              "printf test-token > /run/norn-test/token",
              "chmod 0400 /run/norn-test/token",
          )
          machine.succeed("systemctl restart norn.service")
          ran_cleanly("norn.service")
          machine.succeed("systemctl is-active norn.service")
          ran = status()
          assert (ran["health"], ran["mode"]) == ("failing", "report"), ran
          # the zone's records, the account's scripts and the zone's routes;
          # had the unit failed to read the worker's module, nothing would be
          assert ran["counts"]["unread"] == 3, ran
          described = machine.succeed("norn status")
          assert described.startswith("failing") and "unread: dns record example.com" in described, described
          # the streak that began without a token goes on, and so does the gauge
          assert healthy_at(ran) == since, ran
          assert gauge() == since, (gauge(), ran)

      # every zone is unread, so the gate holds before it looks at the rollout
      # file; only the log says whether the unit could read it
      with subtest("the unit reads a rollout file under /home, and a missing one only holds the fleet"):
          def journal():
              invocation = machine.succeed("systemctl show --property=InvocationID --value norn.service").strip()
              return machine.succeed(f"journalctl _SYSTEMD_INVOCATION_ID={invocation} --no-pager -o cat")

          held = "fleet scope: hold: a scope could not be read"
          protected = machine.succeed("systemctl show --property=ProtectHome --value norn.service").strip()
          assert protected == "read-only", protected
          machine.succeed(
              "install -d -m 0755 /home/deploy",
              """printf '{"committed": 1794942959, "checked": %s}' "$(date +%s)" > /home/deploy/rollout.json""",
              "chmod 0644 /home/deploy/rollout.json",
          )
          machine.succeed("systemctl restart norn.service")
          ran_cleanly("norn.service")
          read = journal()
          assert held in read and "rollout is unknown" not in read, read

          machine.succeed("rm /home/deploy/rollout.json")
          machine.succeed("systemctl restart norn.service")
          ran_cleanly("norn.service")
          machine.succeed("systemctl is-active norn.service")
          missing = journal()
          assert held in missing, missing
          assert "the fleet's rollout is unknown: /home/deploy/rollout.json: No such file or directory" in missing, missing

      with subtest("a root norn converge is refused and leaves the service working"):
          refused = machine.fail("norn converge 2>&1")
          assert "systemctl start norn" in refused, refused
          owned = machine.succeed("find /var/lib/private/norn -uid 0")
          assert owned.strip() == "", owned
          before = status()["finished"]
          machine.succeed("systemctl restart norn.service")
          ran_cleanly("norn.service")
          assert status()["finished"] != before
          assert status()["counts"]["unread"] == 3, status()

      with subtest("the sweep timer runs norn-sweep.service"):
          machine.succeed("systemctl is-active norn-sweep.timer")
          unit = machine.succeed("systemctl show --property=Triggers --value norn-sweep.timer").strip()
          assert unit == "norn-sweep.service", unit
          machine.succeed("systemctl start norn-sweep.service")
          ran_cleanly("norn-sweep.service")

      with subtest("the textfile outlives both units"):
          machine.succeed("systemctl stop norn.service")
          machine.fail("systemctl is-active norn.service norn-sweep.service")
          assert gauge() == since == healthy_at(status()), (gauge(), status())
    '';
}
