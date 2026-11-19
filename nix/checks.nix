{
  pkgs,
  lib,
  self,
  norn,
}:
let
  fleet =
    machines:
    let
      evaluated = lib.mapAttrs (
        name: declarations:
        lib.evalModules {
          specialArgs = { inherit pkgs; };
          modules = [
            (import ./modules/common.nix { inherit self; })
            {
              options = {
                assertions = lib.mkOption {
                  type = lib.types.listOf lib.types.raw;
                  default = [ ];
                };
                environment = lib.mkOption {
                  type = lib.types.submodule { freeformType = lib.types.lazyAttrsOf lib.types.raw; };
                  default = { };
                };
                networking.hostName = lib.mkOption { type = lib.types.str; };
              };
              config = {
                networking.hostName = name;
                cloudflare.zones = [
                  "example.com"
                  "example.org"
                ];
                services.norn = {
                  package = norn;
                  tokenFile = "/run/agenix/cloudflare-token";
                  peers = lib.mapAttrs (_: machine: machine.config.cloudflare.claims) evaluated;
                };
              };
            }
            declarations
          ];
        }
      ) machines;
    in
    evaluated;

  failures =
    machines:
    lib.sort lib.lessThan (
      lib.unique (
        lib.concatMap (
          machine: map (assertion: assertion.message) (lib.filter (a: !a.assertion) machine.config.assertions)
        ) (lib.attrValues (fleet machines))
      )
    );

  with' = modules: { imports = modules; };

  web-1 = {
    services.norn.enable = true;
    cloudflare.address.ipv4 = "192.0.2.10";
    cloudflare.dns."cache.example.com".a = { };
  };
  web-2 = {
    services.norn.enable = true;
    cloudflare.address.ipv4 = "192.0.2.20";
    cloudflare.dns."example.com" = {
      a.proxied = true;
      mx = [
        {
          exchange = "mail.example.com";
          priority = 10;
        }
      ];
      txt = [ "v=spf1 -all" ];
    };
  };
  desktop.services.norn.enable = false;
  tunnel.cloudflare.fleet.dns."tunnel.example.com".cname.target = "abc.cfargotunnel.com";
  offline = {
    cloudflare.account = "0123456789abcdef0123456789abcdef";
    cloudflare.fleet.workers.coeus-offline = {
      module = pkgs.writeText "worker.js" "export default { fetch: (request) => fetch(request) };";
      compatibilityDate = "2026-11-01";
      routes = [ "coeus.example.com/*" ];
      bypass = [ "coeus.example.com/api/*" ];
    };
  };

  pushing = {
    services.norn.push = {
      enable = true;
      vapidKeyFile = "/run/agenix/push-vapid";
      subscriptionsFile = "/run/agenix/push-subscriptions";
      subject = "https://fleet.example.com";
      navigate = "https://fleet.example.com/machines/{host}";
    };
  };

  cases = {
    clean = {
      fleet = {
        inherit web-1 web-2 desktop;
      };
      expect = [ ];
    };
    collision = {
      fleet = {
        inherit web-1;
        web-2 = with' [
          web-2
          { cloudflare.dns."cache.example.com".a = { }; }
        ];
      };
      expect = [
        "norn: cache.example.com A is claimed by web-1 and web-2; one name and type has one owner."
      ];
    };
    cname-beside-another-machines-record = {
      fleet = {
        web-1 = with' [
          web-1
          {
            cloudflare.dns."app.example.org".cname.target = "example.org";
          }
        ];
        web-2 = with' [
          web-2
          { cloudflare.dns."app.example.org".txt = [ "hello" ]; }
        ];
      };
      expect = [
        "norn: app.example.org CNAME shares its name with app.example.org TXT; a CNAME shares its name with nothing."
      ];
    };
    push-clean = {
      fleet.web-1 = with' [
        web-1
        pushing
      ];
      expect = [ ];
    };
    push-elsewhere = {
      fleet.web-1 = with' [
        web-1
        pushing
        {
          services.norn.push = {
            subject = lib.mkForce "fleet.example.com";
            navigate = lib.mkForce "http://fleet.example.com/machines/{host}";
          };
        }
      ];
      expect = [
        "norn: services.norn.push.navigate is http://fleet.example.com/machines/{host}; a notice opens an https: URL."
        "norn: services.norn.push.subject is fleet.example.com; a VAPID subject is an https: URL or a mailto: address."
      ];
    };
    fleet-clean = {
      fleet = {
        web-1 = with' [
          web-1
          tunnel
        ];
        web-2 = with' [
          web-2
          tunnel
        ];
        inherit desktop;
      };
      expect = [ ];
    };
    fleet-claimed-by-a-machine = {
      fleet = {
        web-1 = with' [
          web-1
          tunnel
          { cloudflare.fleet.dns."cache.example.com".a.addresses = [ "192.0.2.10" ]; }
        ];
      };
      expect = [
        "norn: cache.example.com A is claimed by web-1 and by the fleet; one name and type has one owner."
      ];
    };
    fleet-cname-beside-a-machines-record = {
      fleet = {
        web-1 = with' [
          web-1
          tunnel
        ];
        web-2 = with' [
          web-2
          tunnel
          { cloudflare.dns."tunnel.example.com".txt = [ "hello" ]; }
        ];
      };
      expect = [
        "norn: tunnel.example.com CNAME shares its name with tunnel.example.com TXT; a CNAME shares its name with nothing."
      ];
    };
    fleet-disagrees = {
      fleet = {
        web-1 = with' [
          web-1
          tunnel
        ];
        web-2 = with' [
          web-2
          { cloudflare.fleet.dns."tunnel.example.com".cname.target = "def.cfargotunnel.com"; }
        ];
        desktop = with' [
          desktop
          { cloudflare.fleet.dns."stale.example.com".txt = [ "ignored" ]; }
        ];
      };
      expect = [
        "norn: web-1 declares other fleet records than web-2; every machine declares the same cloudflare.fleet.dns."
        "norn: web-2 declares other fleet records than web-1; every machine declares the same cloudflare.fleet.dns."
      ];
    };
    workers-clean = {
      fleet = {
        web-1 = with' [
          web-1
          offline
        ];
        web-2 = with' [
          web-2
          offline
        ];
        inherit desktop;
      };
      expect = [ ];
    };
    workers-disagree = {
      fleet = {
        web-1 = with' [
          web-1
          offline
        ];
        web-2 = with' [
          web-2
          offline
          { cloudflare.fleet.workers.coeus-offline.bypass = lib.mkForce [ ]; }
        ];
      };
      expect = [
        "norn: web-1 declares other fleet workers than web-2; every machine declares the same cloudflare.fleet.workers and cloudflare.account."
        "norn: web-2 declares other fleet workers than web-1; every machine declares the same cloudflare.fleet.workers and cloudflare.account."
      ];
    };
    fleet-without-addresses = {
      fleet = {
        web-1 = with' [
          web-1
          { cloudflare.fleet.dns."vpn.example.com".a = { }; }
        ];
      };
      expect = [
        "norn: fleet record vpn.example.com A names no addresses; a shared record lists its own."
      ];
    };
    records-without-norn = {
      fleet = {
        inherit web-1;
        desktop = with' [
          desktop
          { cloudflare.dns."pc.example.com".a.addresses = [ "192.0.2.99" ]; }
        ];
      };
      expect = [
        "norn: desktop declares cloudflare records but services.norn.enable is false, so nothing would apply them."
      ];
    };
  };

  report = lib.mapAttrs (_: case: {
    inherit (case) expect;
    actual = failures case.fleet;
  }) cases;
  wrong = lib.filterAttrs (_: case: case.expect != case.actual) report;

  machine = name: (fleet { ${name} = cases.clean.fleet.${name}; }).${name}.config;
  sharing =
    (fleet {
      web-1 = with' [
        web-1
        tunnel
        offline
        {
          services.norn = {
            revision = 1794942959;
            rollout.file = "/var/lib/fleet/rollout.json";
          };
        }
      ];
    }).web-1.config;
  pushed =
    (fleet {
      web-1 = with' [
        web-1
        pushing
      ];
    }).web-1.config;
  broken =
    (fleet {
      web-1 = with' [
        web-1
        { cloudflare.dns."cache.example.net".a = { }; }
      ];
    }).web-1.config;
  strayRoute =
    (fleet {
      web-1 = with' [
        web-1
        offline
        { cloudflare.fleet.workers.coeus-offline.routes = lib.mkForce [ "coeus.example.net/*" ]; }
      ];
    }).web-1.config;
in
{
  module-assertions = pkgs.runCommandLocal "norn-module-assertions" {
    report = builtins.toJSON (if wrong == { } then { } else report);
    passAsFile = [ "report" ];
  } ''
    if [ "$(cat "$reportPath")" != "{}" ]; then
      echo "assertions differ from what the cases expect:" >&2
      cat "$reportPath" >&2
      exit 1
    fi
    touch "$out"
  '';

  module-desired = pkgs.runCommandLocal "norn-module-desired" { } ''
    grep -q '"host": "web-2"' ${(machine "web-2").services.norn.desired}
    grep -q '"content": "v=spf1 -all"' ${(machine "web-2").services.norn.desired}
    grep -q '"content": "192.0.2.10"' ${(machine "web-1").services.norn.desired}
    ${lib.optionalString (!lib.hasInfix "--max-deletes 10" (toString (machine "web-1").services.norn.arguments)) "false"}
    grep -q '"schema": 4' ${sharing.services.norn.desired}
    grep -q '"account": "0123456789abcdef0123456789abcdef"' ${sharing.services.norn.desired}
    grep -q '"name": "coeus-offline"' ${sharing.services.norn.desired}
    grep -q '"coeus.example.com/api/\*"' ${sharing.services.norn.desired}
    grep -q '"module": "/nix/store/.*-worker.js"' ${sharing.services.norn.desired}
    grep -q '"account": null' ${(machine "web-1").services.norn.desired}
    grep -q '"revision": 1794942959' ${sharing.services.norn.desired}
    grep -q '"content": "abc.cfargotunnel.com"' ${sharing.services.norn.desired}
    grep -q '"revision": null' ${(machine "web-1").services.norn.desired}
    ${lib.optionalString (!lib.hasInfix "--rollout /var/lib/fleet/rollout.json --rollout-max-age 1800s" (toString sharing.services.norn.arguments)) "false"}
    ${lib.optionalString (lib.hasInfix "--rollout" (toString (machine "web-1").services.norn.arguments)) "false"}
    ${lib.optionalString (!lib.hasInfix "--push-subject https://fleet.example.com --push-navigate https://fleet.example.com/machines/{host} --push-after 1800s" (toString pushed.services.norn.arguments)) "false"}
    ${lib.optionalString (lib.hasInfix "--push" (toString (machine "web-1").services.norn.arguments)) "false"}
    touch "$out"
  '';

  module-desired-refuses = pkgs.runCommandLocal "norn-module-desired-refuses" { } ''
    grep -q 'cache.example.net is outside every managed zone (example.com, example.org)' \
      ${pkgs.testers.testBuildFailure broken.services.norn.desired}/testBuildFailure.log
    grep -q 'coeus.example.net/\* is outside every managed zone (example.com, example.org)' \
      ${pkgs.testers.testBuildFailure strayRoute.services.norn.desired}/testBuildFailure.log
    touch "$out"
  '';
}
// lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
  nixos = pkgs.testers.runNixOSTest (import ./test.nix { inherit self; });
}
