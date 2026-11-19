{ self }:
{
  name = "norn";

  nodes.machine =
    { config, pkgs, ... }:
    let
      fakePush = pkgs.writers.writePython3Bin "fake-push" {
        libraries = [ pkgs.python3Packages.cryptography ];
        flakeIgnore = [ "E501" ];
      } (builtins.readFile ./fake-push.py);
    in
    {
      imports = [ self.nixosModules.default ];

      systemd.services.fake-push = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          ExecStart = "${fakePush}/bin/fake-push /var/lib/fake-push/seen.jsonl";
          StateDirectory = "fake-push";
        };
      };

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
        push = {
          enable = true;
          # RFC 8291's appendix A sender key, as a VAPID key no one relies on
          vapidKeyFile = "${pkgs.writeText "vapid" "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"}";
          # the browser of appendix A, whose private key the fake push service holds
          subscriptionsFile = "${pkgs.writeText "subscriptions.json" (
            builtins.toJSON [
              {
                name = "phone";
                endpoint = "http://127.0.0.1:8080/push/phone";
                keys = {
                  p256dh = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
                  auth = "BTBZMqHH6r4Tts7J_aSIgg";
                };
              }
            ]
          )}";
          subject = "https://fleet.example.com";
          navigate = "https://fleet.example.com/machines/{host}";
          after = 20;
        };
      };

      specialisation.restartsNorn.configuration.cloudflare.dns."www.example.com".a = { };
    };

  # the vm has no network, so a run that reaches cloudflare cannot read it
  testScript =
    { nodes, ... }:
    ''
      import json
      import math

      def status():
          return json.loads(machine.succeed("cat /var/lib/norn/status.json"))

      def pushed():
          seen = machine.succeed("cat /var/lib/fake-push/seen.jsonl 2>/dev/null || true")
          return [json.loads(line) for line in seen.splitlines()]

      def ran_cleanly(unit):
          result = machine.succeed(f"systemctl show --property=Result --value {unit}").strip()
          code = machine.succeed(f"systemctl show --property=ExecMainStatus --value {unit}").strip()
          assert (result, code) == ("success", "0"), (unit, result, code)

      machine.wait_for_unit("multi-user.target")

      with subtest("without a token, boot runs norn, which skips and fails nothing"):
          machine.wait_for_unit("norn.service")
          ran_cleanly("norn.service")
          assert status()["health"] == "skipped", status()
          # the streak starts with this run, so it tells no device yet
          assert pushed() == [], pushed()
          machine.succeed("systemctl start norn-sweep.service")
          ran_cleanly("norn-sweep.service")
          failed = machine.succeed("systemctl list-units --failed --plain --no-legend")
          assert failed.strip() == "", failed

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

      with subtest("runs troubled past push.after tell the devices once"):
          machine.wait_for_open_port(8080)
          since = float(machine.succeed(f"date -d {status()['troubledSince']} +%s.%N"))
          now = float(machine.succeed("date +%s.%N"))
          machine.sleep(max(0, math.ceil(since + 21 - now)))
          for _ in range(3):
              machine.succeed("systemctl start norn-sweep.service")
              ran_cleanly("norn-sweep.service")
          seen = pushed()
          assert len(seen) == 1, seen
          told = seen[0]
          assert told["path"] == "/push/phone", told
          assert told["claims"]["sub"] == "https://fleet.example.com", told
          assert (told["urgency"], told["ttl"]) == ("high", "86400"), told
          notification = told["message"]["notification"]
          assert notification["title"] == "norn skipping runs on machine", notification
          assert notification["tag"] == "norn/machine", notification
          assert notification["navigate"] == "https://fleet.example.com/machines/machine", notification
          assert notification["body"].startswith("no cloudflare token yet; skipping this run\ntroubled for "), notification
          assert status()["notifiedAt"] is not None, status()
          machine.succeed("norn status | grep '^notified at '")

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

      with subtest("a streak that turns from skipped to failing tells nothing new"):
          assert status()["health"] == "failing", status()
          assert len(pushed()) == 1, pushed()
    '';
}
