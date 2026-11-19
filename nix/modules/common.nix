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

  # `address` is null for records no one machine owns, which name their addresses
  recordSet =
    address:
    let
      pointed =
        type:
        if address == null then
          "${type} records, one per address in `addresses`."
        else
          "${type} records; `{ }` points one at this machine's address.";
    in
    types.submodule {
      options = {
        a = mkOption {
          type = types.nullOr (addressSet "ipv4" (address.ipv4 or null));
          default = null;
          example = lib.literalExpression "{ proxied = true; }";
          description = pointed "A";
        };
        aaaa = mkOption {
          type = types.nullOr (addressSet "ipv6" (address.ipv6 or null));
          default = null;
          description = pointed "AAAA";
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

  workers = lib.mapAttrsToList (name: worker: {
    inherit name;
    module = "${worker.module}";
    compatibility_date = worker.compatibilityDate;
    inherit (worker) routes bypass;
  }) cf.fleet.workers;

  # what every machine running norn has to agree on about workers
  sharedWorkers = {
    inherit (cf) account;
    inherit workers;
  };

  desired = pkgs.runCommandLocal "norn-desired.json" {
    raw = builtins.toJSON {
      schema = 4;
      inherit (cfg) host mode;
      inherit (cf) zones account;
      dns = rows cf.dns;
      fleet = {
        inherit (cfg) revision;
        inherit workers;
        dns = rows cf.fleet.dns;
      };
    };
    passAsFile = [ "raw" ];
  } ''
    ${lib.getExe cfg.package} check --in "$rawPath" --out "$out"
  '';

  pushAssertions = [
    {
      assertion = lib.hasPrefix "https://" cfg.push.subject || lib.hasPrefix "mailto:" cfg.push.subject;
      message = "norn: services.norn.push.subject is ${cfg.push.subject}; a VAPID subject is an https: URL or a mailto: address.";
    }
    {
      assertion = lib.hasPrefix "https://" cfg.push.navigate;
      message = "norn: services.norn.push.navigate is ${cfg.push.navigate}; a notice opens an https: URL.";
    }
  ];

  fleetAssertions =
    let
      peers = lib.filterAttrs (_: claims: claims.enabled) cfg.peers;
      claimed = lib.concatLists (
        lib.mapAttrsToList (host: claims: map (key: { inherit key host; }) claims.keys) peers
      );
      owners = lib.mapAttrs (_: entries: lib.sort lib.lessThan (lib.unique (map (entry: entry.host) entries))) (
        lib.groupBy (entry: entry.key) claimed
      );
      contested = lib.filterAttrs (_: hosts: lib.length hosts > 1) owners;
      shared = keys cf.fleet.dns;
      everything = lib.unique (lib.attrNames owners ++ shared);
      nameOf = key: lib.head (lib.splitString " " key);
      cnames = lib.filter (key: lib.hasSuffix " CNAME" key) everything;
      beside = cname: lib.filter (key: key != cname && nameOf key == nameOf cname) everything;
      unaddressed = lib.filter (row: row.content == "" && lib.elem row.type [
        "A"
        "AAAA"
      ]) (rows cf.fleet.dns);
      disagreeing = lib.attrNames (
        lib.filterAttrs (_: claims: (claims.fleet or [ ]) != rows cf.fleet.dns) peers
      );
      workersDisagree = lib.attrNames (
        lib.filterAttrs (_: claims: (claims.workers or null) != sharedWorkers) peers
      );
    in
    [
      {
        assertion = cfg.peers ? ${cfg.host};
        message = "norn: services.norn.peers does not list ${cfg.host}; pass every machine's cloudflare.claims.";
      }
    ]
    ++ lib.mapAttrsToList (key: hosts: {
      assertion = false;
      message = "norn: ${key} is claimed by ${lib.concatStringsSep " and " hosts}; one name and type has one owner.";
    }) contested
    ++ map (key: {
      assertion = !(owners ? ${key});
      message = "norn: ${key} is claimed by ${lib.concatStringsSep " and " (owners.${key} or [ ])} and by the fleet; one name and type has one owner.";
    }) shared
    ++ map (cname: {
      assertion = beside cname == [ ];
      message = "norn: ${cname} shares its name with ${lib.concatStringsSep ", " (beside cname)}; a CNAME shares its name with nothing.";
    }) cnames
    ++ map (row: {
      assertion = false;
      message = "norn: fleet record ${row.name} ${row.type} names no addresses; a shared record lists its own.";
    }) unaddressed
    ++ lib.optional (disagreeing != [ ]) {
      assertion = false;
      message = "norn: ${lib.concatStringsSep ", " disagreeing} declare${lib.optionalString (lib.length disagreeing == 1) "s"} other fleet records than ${cfg.host}; every machine declares the same cloudflare.fleet.dns.";
    }
    ++ lib.optional (workersDisagree != [ ]) {
      assertion = false;
      message = "norn: ${lib.concatStringsSep ", " workersDisagree} declare${lib.optionalString (lib.length workersDisagree == 1) "s"} other fleet workers than ${cfg.host}; every machine declares the same cloudflare.fleet.workers and cloudflare.account.";
    };
in
{
  options.cloudflare = {
    zones = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "example.com" ];
      description = "Zones the fleet manages. norn reads them and writes nothing outside them.";
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
        no other machine may declare the same name and type. A record that
        belongs to no one machine goes in `cloudflare.fleet.dns`.
      '';
    };

    fleet.dns = mkOption {
      type = types.attrsOf (recordSet null);
      default = { };
      example = lib.literalExpression ''{ "tunnel.example.com".cname.target = "<id>.cfargotunnel.com"; }'';
      description = ''
        Records the whole fleet shares, by name, in the shape of
        `cloudflare.dns`, except that A and AAAA records list their
        `addresses`. Every machine declares the same set, usually from one
        shared module, and any machine running the newest configuration
        writes them.
      '';
    };

    account = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "0123456789abcdef0123456789abcdef";
      description = ''
        The account the fleet's workers live in. With it set, norn reads the
        account's scripts and every zone's worker routes on every run, so
        keep it set as long as any worker norn wrote may still be there.
      '';
    };

    fleet.workers = mkOption {
      type = types.attrsOf (
        types.submodule {
          options = {
            module = mkOption {
              type = types.path;
              example = lib.literalExpression ''"''${offline-page}/worker.js"'';
              description = "The worker's main module, one ES module file.";
            };
            compatibilityDate = mkOption {
              type = types.str;
              example = "2026-11-01";
              description = "The Workers runtime's compatibility date, `YYYY-MM-DD`.";
            };
            routes = mkOption {
              type = types.listOf types.str;
              default = [ ];
              example = [ "coeus.example.com/*" ];
              description = "Route patterns that run this worker. Each one's host lies in `cloudflare.zones`.";
            };
            bypass = mkOption {
              type = types.listOf types.str;
              default = [ ];
              example = [ "coeus.example.com/api/*" ];
              description = ''
                Route patterns that run no worker at all, so requests there skip
                this one. The script's tags list them, as they list `routes`, and
                together the lists fit in the 10 tags Cloudflare keeps on a script.
              '';
            };
          };
        }
      );
      default = { };
      description = ''
        Workers the whole fleet shares, by script name. Every machine
        declares the same set, and any machine running the newest
        configuration uploads them. They need `cloudflare.account`.
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

    revision = mkOption {
      type = types.nullOr types.ints.unsigned;
      default = null;
      example = lib.literalExpression "if self ? rev then self.lastModified else null";
      description = ''
        The commit time, in unix seconds, of the commit this configuration is
        built from, or null for an uncommitted tree. A newer revision
        outranks an older one when they disagree on the fleet's shared
        records, and a configuration without one never writes them. Set it
        from the flake: `if self ? rev then self.lastModified else null`.
      '';
    };

    rollout = {
      file = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "/var/lib/fleet/rollout.json";
        description = ''
          A JSON file the fleet's deploy tool writes:
          `{ "committed": <revision>, "checked": <unix seconds> }`, the newest
          revision the whole reachable fleet runs and when that was last
          confirmed. With it, norn deletes a retiring shared record once the
          whole fleet runs the configuration that dropped it, and a machine
          behind the rollout holds. Without it, retiring records are never
          deleted. It must be readable by every user, and a missing or stale
          file only holds the shared records. On NixOS, a file under `/home`,
          `/root` or `/run/user` makes the units see those read-only rather
          than not at all.
        '';
      };

      maxAge = mkOption {
        type = types.ints.positive;
        default = 1800;
        description = "Seconds after `checked` past which the rollout file counts as unknown.";
      };
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
        and deletes this machine's marked records.
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

    maxDeletes = mkOption {
      type = types.ints.unsigned;
      default = 10;
      description = ''
        When a run would delete more records than this, it deletes none of
        them and reports them withheld. Raise it for the one deploy that means
        to remove that many.
      '';
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
        The build fails when two machines claim one name and type, or when a
        CNAME shares its name across machines.
      '';
    };

    push = {
      enable = lib.mkEnableOption ''
        Web Push notices to the owner's devices: one when this machine's runs
        have stayed troubled for `push.after`, and one when that ends. A fault
        that lasts sends nothing more, and nothing about a notice changes a
        run's outcome
      '';

      vapidKeyFile = mkOption {
        type = types.str;
        example = "/run/agenix/push-vapid";
        description = ''
          File holding the VAPID private key: the base64url of a P-256 scalar.
          On NixOS the units get it as a credential, so it may belong to root.
        '';
      };

      subscriptionsFile = mkOption {
        type = types.str;
        example = "/run/agenix/push-subscriptions";
        description = ''
          File holding the devices to notify: a JSON array of the browser's
          `PushSubscription.toJSON()` objects, each with a `name` that logs
          print in its place. It is read at every send, and on NixOS reaches
          the units as a credential.
        '';
      };

      subject = mkOption {
        type = types.str;
        example = "https://fleet.example.com";
        description = "The VAPID subject: an `https:` URL or a `mailto:` address where push services can reach the sender.";
      };

      navigate = mkOption {
        type = types.str;
        example = "https://fleet.example.com/machines/{host}";
        description = "The `https:` URL a tap on a notice opens. `{host}` stands for `services.norn.host`.";
      };

      after = mkOption {
        type = types.ints.positive;
        default = 1800;
        description = ''
          Seconds runs stay troubled before the devices hear of it. The
          default is when `norn status` starts calling the machine troubled.
        '';
      };
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
        fleet = rows cf.fleet.dns;
        workers = sharedWorkers;
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
          "--max-deletes"
          (toString cfg.maxDeletes)
        ]
        ++ lib.optionals (cfg.rollout.file != null) [
          "--rollout"
          cfg.rollout.file
          "--rollout-max-age"
          "${toString cfg.rollout.maxAge}s"
        ]
        ++ lib.optionals cfg.push.enable [
          "--push-subject"
          cfg.push.subject
          "--push-navigate"
          cfg.push.navigate
          "--push-after"
          "${toString cfg.push.after}s"
        ];
      };

      assertions = fleetAssertions ++ lib.optionals cfg.push.enable pushAssertions;

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
              --set-default NORN_STATE_DIR /var/lib/norn \
              --set-default NORN_MAX_DELETES ${toString cfg.maxDeletes} ${
                lib.optionalString (cfg.rollout.file != null)
                  "--set-default NORN_ROLLOUT ${lib.escapeShellArg cfg.rollout.file} --set-default NORN_ROLLOUT_MAX_AGE ${toString cfg.rollout.maxAge}s"
              } ${
                lib.optionalString cfg.push.enable (
                  lib.concatMapStringsSep " " (pair: "--set-default ${lib.head pair} ${lib.escapeShellArg (lib.last pair)}") [
                    [ "NORN_PUSH_VAPID_KEY_FILE" cfg.push.vapidKeyFile ]
                    [ "NORN_PUSH_SUBSCRIPTIONS_FILE" cfg.push.subscriptionsFile ]
                    [ "NORN_PUSH_SUBJECT" cfg.push.subject ]
                    [ "NORN_PUSH_NAVIGATE" cfg.push.navigate ]
                    [ "NORN_PUSH_AFTER" "${toString cfg.push.after}s" ]
                  ]
                )
              }
          '';
        })
      ];
    })

    (lib.mkIf (!cfg.enable) {
      assertions = [
        {
          assertion = cf.dns == { };
          message = "norn: ${cfg.host} declares cloudflare records but services.norn.enable is false, so nothing would apply them.";
        }
      ];
    })
  ];
}
