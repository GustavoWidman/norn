{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkOption types;

  cfg = config.services.norn;
  cf = config.cloudflare;

  ttl = mkOption {
    type = types.either (types.enum [ "auto" ]) types.ints.positive;
    default = "auto";
    description = "Seconds (60 to 86400), or `auto`. A proxied record's ttl is always `auto`.";
  };

  proxied = mkOption {
    type = types.bool;
    default = false;
    description = "Answer through Cloudflare's proxy.";
  };

  addressSet =
    family: fallback:
    types.submodule {
      options = {
        addresses = mkOption {
          type = types.listOf types.str;
          default = lib.optional (fallback != null) fallback;
          defaultText = lib.literalExpression "[ config.cloudflare.address.${family} ]";
          description = "One record per address.";
        };
        inherit proxied ttl;
      };
    };

  recordSet =
    address:
    types.submodule {
      options = {
        a = mkOption {
          type = types.nullOr (addressSet "ipv4" address.ipv4);
          default = null;
          example = lib.literalExpression "{ proxied = true; }";
          description = "A records; `{ }` points one at this machine's address.";
        };
        aaaa = mkOption {
          type = types.nullOr (addressSet "ipv6" address.ipv6);
          default = null;
          description = "AAAA records; `{ }` points one at this machine's address.";
        };
        cname = mkOption {
          type = types.nullOr (
            types.submodule {
              options = {
                target = mkOption { type = types.str; };
                inherit proxied ttl;
              };
            }
          );
          default = null;
        };
        mx = mkOption {
          type = types.listOf (
            types.submodule {
              options = {
                exchange = mkOption { type = types.str; };
                priority = mkOption { type = types.ints.u16; };
                inherit ttl;
              };
            }
          );
          default = [ ];
        };
        txt = mkOption {
          type = types.listOf (
            types.coercedTo types.str (text: { inherit text; }) (
              types.submodule {
                options = {
                  text = mkOption { type = types.str; };
                  inherit ttl;
                };
              }
            )
          );
          default = [ ];
        };
      };
    };

  rows =
    sets:
    lib.concatLists (
      lib.mapAttrsToList (
        name: set:
        let
          record =
            type: fields:
            {
              inherit name type;
              proxied = null;
              priority = null;
              ttl = "auto";
            }
            // fields;
          reach = sub: { inherit (sub) proxied ttl; };
          addressed =
            type: sub:
            lib.optionals (sub != null) (
              map (address: record type ({ content = address; } // reach sub)) (
                if sub.addresses == [ ] then [ "" ] else sub.addresses
              )
            );
        in
        addressed "A" set.a
        ++ addressed "AAAA" set.aaaa
        ++ lib.optional (set.cname != null) (
          record "CNAME" ({ content = set.cname.target; } // reach set.cname)
        )
        ++ map (
          mx:
          record "MX" {
            content = mx.exchange;
            inherit (mx) priority ttl;
          }
        ) set.mx
        ++ map (
          txt:
          record "TXT" {
            content = txt.text;
            inherit (txt) ttl;
          }
        ) set.txt
      ) sets
    );

  keys = sets: lib.unique (map (row: "${row.name} ${row.type}") (rows sets));

  # nixos and nix-darwin merge the same modules in different orders, so the
  # digest every machine compares is over sorted lists
  fleetSection = {
    zones = lib.sort lib.lessThan (lib.unique cf.zones);
    dns = lib.sort (a: b: builtins.toJSON a < builtins.toJSON b) (rows cf.fleet.dns);
  };

  desired = pkgs.runCommandLocal "norn-desired.json" {
    raw = builtins.toJSON {
      schema = 1;
      inherit (cfg) host mode;
      inherit (cf) zones;
      host_owned.dns = rows cf.dns;
      fleet_owned.dns = rows cf.fleet.dns;
    };
    passAsFile = [ "raw" ];
  } ''
    ${lib.getExe cfg.package} check --in "$rawPath" --out "$out"
  '';

  fleetAssertions =
    let
      peers = lib.filterAttrs (_: claims: claims.enabled) cfg.peers;
      claimed = lib.concatLists (
        lib.mapAttrsToList (
          host: claims:
          map (key: { inherit key host; }) claims.keys
          ++ map (key: {
            inherit key;
            host = "fleet";
          }) claims.fleetKeys
        ) peers
      );
      owners = lib.mapAttrs (_: entries: lib.sort lib.lessThan (lib.unique (map (entry: entry.host) entries))) (
        lib.groupBy (entry: entry.key) claimed
      );
      contested = lib.filterAttrs (_: hosts: lib.length hosts > 1) owners;
      nameOf = key: lib.head (lib.splitString " " key);
      cnames = lib.filter (key: lib.hasSuffix " CNAME" key) (lib.attrNames owners);
      beside =
        cname:
        lib.filter (key: key != cname && nameOf key == nameOf cname) (lib.attrNames owners);
      digests = lib.unique (lib.mapAttrsToList (_: claims: claims.fleetDigest) peers);
    in
    [
      {
        assertion = cfg.peers ? ${cfg.host};
        message = "norn: services.norn.peers does not list ${cfg.host}; pass every machine's cloudflare.claims.";
      }
      {
        assertion = lib.length digests <= 1;
        message = "norn: the fleet's zones or fleet records differ between machines (${lib.concatStringsSep ", " (lib.attrNames peers)}); declare them once, in a module every machine that runs norn imports.";
      }
    ]
    ++ lib.mapAttrsToList (key: hosts: {
      assertion = false;
      message = "norn: ${key} is claimed by ${lib.concatStringsSep " and " hosts}; one name and type has one owner.";
    }) contested
    ++ map (cname: {
      assertion = beside cname == [ ];
      message = "norn: ${cname} shares its name with ${lib.concatStringsSep ", " (beside cname)}; a CNAME shares its name with nothing.";
    }) cnames;
in
{
  options.cloudflare = {
    zones = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "example.com" ];
      description = "Zones the fleet manages. Every machine that runs norn declares the same.";
    };

    address = {
      ipv4 = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Where this machine's A records point unless they name addresses.";
      };
      ipv6 = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Where this machine's AAAA records point unless they name addresses.";
      };
    };

    dns = mkOption {
      type = types.attrsOf (recordSet cf.address);
      default = { };
      example = lib.literalExpression ''{ "cache.example.com".a = { }; }'';
      description = ''
        Records this machine owns, by name. Only this machine writes them, and
        no other machine may declare the same name and type.
      '';
    };

    fleet.dns = mkOption {
      type = types.attrsOf (
        recordSet {
          ipv4 = null;
          ipv6 = null;
        }
      );
      default = { };
      description = ''
        Records every machine that runs norn writes, identically. Every such
        machine must declare exactly the same set; declare them in a module all
        of them import.
      '';
    };

    claims = mkOption {
      type = types.raw;
      readOnly = true;
      internal = true;
      description = ''
        What this machine tells the others it declares. Other machines read it
        through `services.norn.peers`, so it depends only on this machine's
        own declarations.
      '';
    };
  };

  options.services.norn = {
    enable = lib.mkEnableOption "norn, which brings Cloudflare in line with `cloudflare.*` on activation and on a timer";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "norn.packages.\${system}.default";
    };

    host = mkOption {
      type = types.str;
      default = config.networking.hostName;
      defaultText = lib.literalExpression "config.networking.hostName";
      description = "This machine's name in the fleet, and in its marks (`norn v1 host=<host>`).";
    };

    mode = mkOption {
      type = types.enum [
        "report"
        "adopt"
        "enforce"
      ];
      default = "report";
      description = ''
        `report` only reads and logs the plan. `adopt` marks unmarked records
        that already equal their declaration. `enforce` also creates, updates
        and deletes this machine's and the fleet's marked records.
      '';
    };

    tokenFile = mkOption {
      type = types.str;
      example = "/run/agenix/cloudflare-token";
      description = "File holding the fleet's Cloudflare API token.";
    };

    interval = mkOption {
      type = types.ints.positive;
      default = 300;
      description = "Seconds between runs that repair drift and retry what did not apply.";
    };

    deadline = mkOption {
      type = types.ints.positive;
      default = 30;
      description = "Seconds a run may spend on Cloudflare; past it, what is left is retried by the next run.";
    };

    requestTimeout = mkOption {
      type = types.ints.positive;
      default = 5;
      description = "Seconds one request may take.";
    };

    peers = mkOption {
      type = types.lazyAttrsOf types.raw;
      example = lib.literalExpression "lib.mapAttrs (name: _: self.\${name}.cloudflare.claims) machines";
      description = ''
        Every fleet machine's `cloudflare.claims`, by name, this one included.
        The build fails when two machines claim one name and type, when the
        fleet section differs between machines, or when a CNAME shares its
        name across machines.
      '';
    };

    desired = mkOption {
      type = types.package;
      readOnly = true;
      description = "The checked desired file the service runs on.";
    };

    arguments = mkOption {
      type = types.listOf types.str;
      readOnly = true;
      internal = true;
      description = "What every unit passes to `norn converge`, store paths included.";
    };
  };

  config = lib.mkMerge [
    {
      cloudflare.claims = {
        enabled = cfg.enable;
        keys = keys cf.dns;
        fleetKeys = keys cf.fleet.dns;
        fleetDigest = builtins.hashString "sha256" (builtins.toJSON fleetSection);
      };
    }

    (lib.mkIf cfg.enable {
      services.norn = {
        inherit desired;
        arguments = [
          "--desired"
          "${desired}"
          "--state-dir"
          "/var/lib/norn"
          "--deadline"
          "${toString cfg.deadline}s"
          "--request-timeout"
          "${toString cfg.requestTimeout}s"
        ];
      };

      assertions = fleetAssertions;

      environment.etc."norn/desired.json".source = desired;
      environment.systemPackages = [
        (pkgs.symlinkJoin {
          name = "norn-${cfg.package.version or "cli"}";
          paths = [ cfg.package ];
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postBuild = ''
            wrapProgram $out/bin/norn \
              --set-default NORN_DESIRED /etc/norn/desired.json \
              --set-default NORN_TOKEN_FILE ${lib.escapeShellArg cfg.tokenFile} \
              --set-default NORN_STATE_DIR /var/lib/norn
          '';
        })
      ];
    })

    (lib.mkIf (!cfg.enable) {
      assertions = [
        {
          assertion = cf.dns == { } && cf.fleet.dns == { };
          message = "norn: ${cfg.host} declares cloudflare records but services.norn.enable is false, so nothing would apply them.";
        }
      ];
    })
  ];
}
