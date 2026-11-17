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
  broken =
    (fleet {
      web-1 = with' [
        web-1
        { cloudflare.dns."cache.example.net".a = { }; }
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
    touch "$out"
  '';

  module-desired-refuses = pkgs.runCommandLocal "norn-module-desired-refuses" { } ''
    grep -q 'cache.example.net is outside every managed zone (example.com, example.org)' \
      ${pkgs.testers.testBuildFailure broken.services.norn.desired}/testBuildFailure.log
    touch "$out"
  '';
}
