# norn

dns records, carved into nix instead of clicked into a dashboard.

norn keeps a nix fleet's cloudflare dns in line with the fleet's own
configuration. records are declared right next to the service that needs
them, and every machine pushes its own records to cloudflare when it
activates, and again every five minutes after that. take a service off a
machine and its records leave with it.

named after the norns, who carve what will be into the roots of yggdrasil.
this one mostly carves `A` records, but the spirit is there.

## why

the cloudflare dashboard is where dns records go to rot. nobody remembers who
made `s3.cache`, whether anything still uses it, or why it points at a box
that has been reinstalled twice since. the fleet already knows which machine
serves what, so the fleet should be the one saying so.

terraform could do this, but terraform wants a state file, a plan/apply ritual
and somewhere to run from. norn keeps no state about records at all. who owns
a record is written on the record itself, in its comment, so every machine
works out what's its own straight from cloudflare, during its own activation,
without talking to any other machine.

## ownership

every record norn writes carries a mark, and the mark is the whole comment:

- `norn v1 host=<host>` for a record only that machine writes.
- `norn v1 fleet` for a record every machine running norn writes, identically.

a machine updates and deletes only records carrying its own mark or the
fleet's. everything else is left alone: records with a note a person wrote,
records with another machine's mark, and anything starting with `norn ` that
doesn't parse (a future version's mark, say). the one exception is adopting:
a record with no comment at all, whose content already matches a declaration
exactly, can be claimed by writing the mark into its comment.

norn manages `A`, `AAAA`, `CNAME`, `MX` and `TXT` records. anything else in
the zone (`NS`, `SRV`, `CAA` and friends) is never touched. the longest mark is
76 bytes, comfortably inside the 100-byte comments of cloudflare's free plan.

## modes

a fleet moves through three modes, in order, set by `services.norn.mode`:

- **`report`** (the default) reads cloudflare and logs what it would do. it
  couldn't write even if it wanted to: the client it builds has no way to send
  anything but a read, and the compiler checks that.
- **`adopt`** claims the records you already have. a record with no comment
  whose content equals a declaration gets the machine's mark, and nothing else
  about it changes.
- **`enforce`** does everything: it creates what's missing, updates what
  drifted, deletes marked records nobody declares anymore, and adopts along the
  way. it never creates a record where an unowned one sits on the same name
  and type, never puts a `CNAME` beside anything, and deletes at most 10
  records per run, so a bad commit can't wipe a zone in one go.

bringing an existing zone over goes like this: deploy everything in `report`,
read the plan, switch to `adopt` so norn claims what's already there, and move
to `enforce` once the plan only says what you expect it to.

## declaring records

the flake exports `nixosModules.default` and `darwinModules.default`. records
go next to the service that needs them:

```nix
# this machine runs the cache, so the cache's records live here too
cloudflare.dns."cache.example.com".a = { }; # points at this machine's address
cloudflare.dns."s3.cache.example.com".a = { };
cloudflare.dns."grafana.example.com".a.proxied = true;
```

records that don't belong to any one machine, like a domain's mail setup, go
under `cloudflare.fleet.dns` in a module every machine imports:

```nix
cloudflare.fleet.dns."example.com" = {
  mx = [ { exchange = "mail.example.com"; priority = 10; } ];
  txt = [ "v=spf1 -all" ];
};
```

`a` and `aaaa` take `addresses` (this machine's `cloudflare.address` when left
out, so a fleet record has to name its own), `proxied` and `ttl`. `cname` takes
a `target`, `mx` is a list of `{ exchange, priority }`, and `txt` is a list of
strings. a ttl is `"auto"` (the default) or 60 to 86400 seconds, and a proxied
record's ttl is always `"auto"`.

the rest is wired up once, in a module every machine imports:

```nix
{ config, lib, self, ... }:
{
  cloudflare.zones = [ "example.com" "example.org" ];

  services.norn = {
    enable = true;
    mode = "report";
    tokenFile = config.age.secrets.cloudflare-token.path;
    # every machine's claims, by host name, so each build can check the others
    peers = lib.mapAttrs (_: machine: machine.config.cloudflare.claims) (
      self.nixosConfigurations // self.darwinConfigurations
    );
  };
}
```

plus each machine's own address, for the records that point at it:

```nix
cloudflare.address.ipv4 = "203.0.113.7";
```

`peers` is how the machines check each other without ever talking to each
other. every build sees what every other machine claims, so a machine's build
fails when:

- two machines claim the same name and type.
- the zones or the fleet records differ between machines.
- a `CNAME` shares its name with anything, on any machine.
- a machine declares records but doesn't run norn, so nothing would apply them.

on top of that, each machine's records go through `norn check` while the
machine builds. a misspelled name, a name outside every zone, an address that
isn't one or a ttl out of range fails that machine's build long before
anything reaches cloudflare.

## the token

norn needs a cloudflare api token with zone read and dns edit on the zones it
manages. keep it in a secret (agenix, sops-nix, whatever you like) and point
`services.norn.tokenFile` at it. only the path ends up in the nix store.

## running it

on nixos, two units run the same binary:

- `norn.service` runs at boot and whenever a switch changes the machine's
  records, the mode or the package. the switch waits for it, but it gives up
  after its deadline (30 seconds by default) and always exits 0, so cloudflare
  having a bad day never fails a deploy.
- `norn-sweep.timer` runs it again every 5 minutes (`services.norn.interval`)
  to repair drift and retry whatever didn't apply.

both run as a dynamic user that only gets the token as a systemd credential.
on macos, a single launchd daemon does both jobs.

nothing retries inside a run. a run that hits its deadline gives up, writes
down what didn't apply in `/var/lib/norn/status.json`, and leaves it to the
next sweep. a missing token file skips the run, which counts as trouble if it
keeps happening.

## commands

the module puts `norn` on the path, already pointed at the machine's own
records, token and state:

```bash
# what would this machine do right now? reads only, writes nothing
sudo norn report

# the same, plus records already in line and other machines' records
sudo norn report --verbose

# how did the last run go? exits 1 once runs have been failing, or haven't
# happened at all, for more than 30 minutes
sudo norn status
```

a report looks like this:

```text
norn report · web-1 · configured mode report · 3 scopes, 6 objects read in 1.8ms

dns record · example.com
  adopt      web-1  A      cache.example.com            192.0.2.10  dns only
  create     web-1  A      s3.cache.example.com         192.0.2.10  dns only
  blocked    web-1  A      grafana.example.com          192.0.2.10  proxied
                    an object norn does not own sits in this slot; adopt or remove it first; there now: A      grafana.example.com 192.0.2.99  proxied
  untouched         TXT    _acme-challenge.example.com  "Zq1mP0aJx4sV8tN2cR6yB3uL9wE5kH7dG0fA2oI4"  ttl 120
  untouched         A      grafana.example.com          192.0.2.99  proxied
  untouched         A      status.example.com           192.0.2.1  proxied  # has a comment

dns record · example.net
  untouched         A      example.net                  192.0.2.20  proxied

dns record · example.org
  untouched         A      example.org                  192.0.2.30  dns only

1 to adopt · 0 to update · 0 to delete · 1 to create · 0 withheld · 1 blocked · 5 untouched · 0 others' · 0 kept
nothing was written: report only reads. adopt writes the mark into a record's comment and changes nothing else
```

`norn converge` is what the units run, and `norn check` is what the build
runs. you shouldn't need either by hand.

## file structure

```text
.
├── flake.nix          # the package, the modules, the checks and a dev shell
├── nix/
│   ├── package.nix    # the crane build, with the tests and clippy as checks
│   ├── checks.nix     # the module's assertions, over small fake fleets
│   └── modules/
│       ├── common.nix # cloudflare.* and services.norn, for both platforms
│       ├── nixos.nix  # norn.service and the sweep timer
│       └── darwin.nix # the launchd daemon
└── src/
    ├── bin/norn.rs    # the cli
    ├── run.rs         # converge, report, check and status
    ├── plan.rs        # what to adopt, update, delete and create. pure
    ├── kind.rs        # what a kind of cloudflare object has to describe
    ├── dns/           # the first kind: record values, marks, the wire format
    ├── cloudflare/    # the api client, read-only or read-write by type
    ├── desired.rs     # what a machine declares, as its module renders it
    ├── model.rs       # host names, owners, domain names and zones
    ├── report.rs      # the printed plan
    ├── status.rs      # status.json
    └── test/          # an in-memory cloudflare, and the tests that use it
```

dns records are the first kind, not the only one there will ever be.
cloudflare sells plenty of other things to forget about in a dashboard, and
`kind.rs` is where the next one plugs in.

## hacking on it

```bash
nix develop        # the toolchain
cargo test         # the tests, against a fake in-memory cloudflare
nix flake check    # the tests, clippy, the package and the module checks
```

the tests never talk to the real cloudflare. `src/test/fake.rs` answers like
it does, paging, 404s and all, and logs every request, so a test can assert
that `report` sent nothing but reads.

---

you can still edit norn's records in the cloudflare dashboard if you really
want to. once it's enforcing, it'll just put them back within five minutes,
like a disappointed parent.
