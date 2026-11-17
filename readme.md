# norn

dns records, carved into nix instead of clicked into a dashboard.

norn keeps a nix fleet's cloudflare dns in line with the fleet's own
configuration. records are declared right next to the service that needs
them, and every machine pushes its own records to cloudflare when it
activates, and again every five minutes after that. take a service off a
machine and its records leave with it. records that belong to no one
machine, like a tunnel every server connects to, are shared by the whole
fleet and written by whichever machines run the newest configuration.

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
`norn v1 host=<host>` for a record one machine owns, or
`norn v1 fleet t=<revision>` for a record the whole fleet shares (see
[fleet records](#fleet-records)).

a machine updates and deletes only records carrying its own mark, and the
fleet's shared records under the rules below. everything else is left alone:
records with a note a person wrote, records with another machine's mark, and
anything starting with `norn ` that doesn't parse (a future version's mark,
say). the one exception is adopting: a record with no comment at all, whose
content already matches a declaration exactly, can be claimed by writing the
mark into its comment.

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
  and type, never puts a `CNAME` beside anything, and never deletes a record
  while what should replace it at that name is blocked. a run that would
  delete more than 10 records (`services.norn.maxDeletes`) deletes none of
  them and reports them withheld, so a bad commit can't wipe a zone. raise
  the cap for the one deploy that really means it.

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

a record in `cloudflare.dns` has exactly one machine that owns it. records no
single service claims, like a domain's mail setup, go on the machine that
serves them, or in [`cloudflare.fleet.dns`](#fleet-records) when no one
machine should own them:

```nix
# the mail server's machine
cloudflare.dns."example.com" = {
  mx = [ { exchange = "mail.example.com"; priority = 10; } ];
  txt = [ "v=spf1 -all" ];
};
```

`a` and `aaaa` take `addresses` (this machine's `cloudflare.address` when left
out), `proxied` and `ttl`. `cname` takes
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
    # the commit time of the configuration, for the fleet's shared records
    revision = if self ? rev then self.lastModified else null;
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

- two machines claim the same name and type, or a machine and the fleet do.
- a `CNAME` shares its name with anything, on any machine or in the fleet.
- two machines that run norn declare different fleet records.
- a fleet `A` or `AAAA` record lists no addresses.
- a machine declares records but doesn't run norn, so nothing would apply them.

on top of that, each machine's records go through `norn check` while the
machine builds. a misspelled name, a name outside every zone, an address that
isn't one or a ttl out of range fails that machine's build long before
anything reaches cloudflare.

## fleet records

some records belong to no one machine. a cloudflare tunnel's `CNAME` is the
usual one: every server connects to the tunnel, and none of them should be
the one that owns its name. those go in `cloudflare.fleet.dns`, which takes
the same shape as `cloudflare.dns`, except that `a` and `aaaa` list their
`addresses`, since there is no one machine to point at:

```nix
# a module every machine imports
cloudflare.fleet.dns."coeus.example.com".cname = {
  target = "<tunnel id>.cfargotunnel.com";
  proxied = true;
};
```

every machine declares the same fleet records, and any of them can write
them. the hard part is that they don't all run the same configuration at the
same time: a deploy rolls out one machine after another, a machine that was
off comes back on whatever it ran before, and a rollback puts an older
configuration back. norn sorts this out with the configuration's revision,
the commit time of the commit it was built from
(`services.norn.revision`, set to
`if self ? rev then self.lastModified else null`). a newer commit outranks an
older one.

### the stamp

a shared record's mark says which configuration last wrote it:
`norn v1 fleet t=<revision>`. a machine writes shared records with its own
revision, and leaves alone a shared record that already matches, whatever
its stamp. so every change a configuration makes leaves at least one record
stamped with its revision, and that is what tells older configurations to
stand down.

### the gate

once per run, after reading every zone, norn decides whether this machine may
write the fleet's records. it holds them, in this order, when:

- **the configuration has no revision.** a build from an uncommitted tree
  never writes shared records.
- **a zone could not be read.** a newer configuration's records may be sitting
  there unseen.
- **the rollout file is unknown.** it's configured but missing, unreadable, or
  older than its max age.
- **the machine is behind the rollout.** the whole fleet already runs a newer
  configuration than this one.
- **a shared record carries a newer stamp.** a newer configuration is in
  charge, and its machines write the shared records.

otherwise it writes. a held machine still plans the fleet's records and
reports every step it would take, each marked held with the reason. being
held is the normal state of a machine that is behind or mid-rollout, so a run
whose shared records are held counts as `held`, never as failing, and this
machine's own records go on as usual.

`norn report` and the journal say once per run what the gate decided:

```text
fleet scope: write at t=1794942959; retiring records stamped up to t=1794900000 are deleted
fleet scope: hold: the fleet has rolled out t=1794942959, newer than this configuration
```

two machines on the same revision may create the same record at once.
cloudflare refuses the second (an identical record, or a `CNAME` beside one),
and that machine reports the create as `raced`, not failed. the next run reads
what is there.

### the rollout file

`services.norn.rollout.file` points at a json file your deploy tool writes.
norn reads two fields and ignores the rest:

```json
{ "committed": 1794942959, "checked": 1794943200 }
```

`committed` is the revision of the newest configuration the whole reachable
fleet runs, and `checked` is when (unix seconds) the deploy tool last
confirmed it. past `services.norn.rollout.maxAge` (30 minutes) the file
counts as unknown and the fleet's records are held. a missing file only holds
them: it never fails the unit. the units read it in place as their own
unprivileged user, so it has to be readable by every user, and it holds
nothing secret. the nixos units hide `/home`, `/root` and `/run/user`, unless
the file lives under one of them: then they see those read-only. norn logs a
rollout file it can't use, whatever the gate decides.

### retiring

a shared record the configuration no longer declares isn't deleted on the
spot, because machines still on the older configuration want it back. it's
deleted in two steps:

1. its mark becomes `norn v1 fleet t=<revision> delete`. it keeps serving,
   and the newer stamp makes older configurations stand down.
2. once the rollout file says the whole fleet runs that revision or newer,
   the record is deleted. until then, the report lists its delete as held,
   waiting for the rollout. while the gate holds, it says why instead.

declare it again and it gets its plain mark back. fleet deletes count toward
`services.norn.maxDeletes` along with the machine's own.

without a rollout file, norn never deletes a retiring record, since it can't
know when every machine has moved on, and the report says so. delete it by
hand, in the dashboard, once every machine runs the configuration that
dropped it, and not before: a machine still on an older configuration would
create it again.

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

both run as a dynamic user that only gets the token as a systemd credential,
and reads the rollout file, if there is one, where it is. on macos, a single
launchd daemon does both jobs.

nothing retries inside a run. a run that hits its deadline gives up, writes
down what didn't apply in `/var/lib/norn/status.json`, and leaves it to the
next sweep. a missing or empty token file skips the run, which counts as
trouble if it keeps happening. that covers a secret not decrypted on this
machine yet: the unit still exits 0, so it never holds up a deploy.

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
fleet scope: write at t=1794942959; with no rollout file, no retiring record is deleted

dns record · example.com
  adopt      web-1  A      cache.example.com            192.0.2.10  dns only
  create     web-1  A      s3.cache.example.com         192.0.2.10  dns only
  blocked    web-1  A      grafana.example.com          192.0.2.10  proxied
                    an object norn does not own sits in this slot; adopt or remove it first; there now: A      grafana.example.com 192.0.2.99  proxied
  untouched         TXT    _acme-challenge.example.com  "Zq1mP0aJx4sV8tN2cR6yB3uL9wE5kH7dG0fA2oI4"  ttl 120
  create     fleet  CNAME  tunnel.example.com           abc.cfargotunnel.com  proxied
  untouched         A      grafana.example.com          192.0.2.99  proxied
  untouched         A      status.example.com           192.0.2.1  proxied  # has a comment

dns record · example.net
  untouched         A      example.net                  192.0.2.20  proxied

dns record · example.org
  untouched         A      example.org                  192.0.2.30  dns only

1 to adopt · 0 to update · 0 to delete · 2 to create · 0 to retire · 0 to restore · 0 held · 0 withheld · 1 blocked · 5 untouched · 0 others' · 0 kept
nothing was written: report only reads. adopt writes the mark into a record's comment and changes nothing else
```

`norn converge` is what the units run, and `norn check` is what the build
runs. you shouldn't need either by hand. converge refuses to run for anyone
but the owner of its state directory, so `sudo norn converge` on nixos says
to `systemctl start norn` instead of leaving behind a lock file the service's
own user can't open. on macos the daemon and `sudo` are both root, so there
it just runs.

## sharp edges

a few things norn deliberately leaves to you:

- **moving a record to another machine leaves a gap.** the old machine deletes
  it on its next run, and the new one may only create it on the run after
  that, so the name can answer nothing for up to one sweep (5 minutes), plus
  however long resolvers cache the gap. the same goes for moving a record
  between a machine and the fleet. move records at a quiet moment.
- **rolling back shared records takes a new commit.** a rollback deploys an
  older commit, and an older revision never overrides a newer stamp, so its
  machines hold the shared records as they are. to change them back, revert
  in a new commit and deploy that.
- **a shared type change waits for the rollout.** turning a shared `A` into a
  `CNAME` retires the `A`, and the `CNAME` waits beside it, held, until the
  `A` is deleted once the whole fleet runs the change. the run that deletes
  the `A` creates the `CNAME` right after. without a rollout file, the `CNAME`
  waits until you delete the `A` by hand.
- **dropping a zone from `cloudflare.zones` orphans its records.** norn stops
  reading the zone, so the records it marked there stay exactly as they are,
  unmanaged, until you remove them by hand. add the zone back and norn picks
  them up again.
- **renaming a machine orphans its marks too.** the old records still say
  `host=<old name>`, so they block the new name's declarations (the report
  says who owns what) until you delete them or clear their comments.
- **a type change can still blink.** turning an `A` into a `CNAME` is a delete
  and a create, sent back to back. if cloudflare refuses the create, or the
  run dies between the two, the name stays empty until the next sweep.
- **a failing norn is quiet.** runs always exit 0, so nothing shows up in
  `systemctl --failed`. `sudo norn status` is where to look: it exits 1 once
  runs have been troubled, or silent, for 30 minutes.

## file structure

```text
.
├── .github/           # checks for every pull request, releases to the cache
├── flake.nix          # the package, the modules, the checks and a dev shell
├── nix/
│   ├── package.nix    # the crane build, with the tests and clippy as checks
│   ├── checks.nix     # the module's assertions, over small fake fleets
│   ├── test.nix       # the nixos units, booted in a vm
│   └── modules/
│       ├── common.nix # cloudflare.* and services.norn, for both platforms
│       ├── nixos.nix  # norn.service and the sweep timer
│       └── darwin.nix # the launchd daemon
└── src/
    ├── bin/norn.rs    # the cli
    ├── run.rs         # converge, report, check and status
    ├── plan.rs        # what to adopt, update, delete and create. pure
    ├── gate.rs        # whether a run may write the fleet's shared records
    ├── kind.rs        # what a kind of cloudflare object has to describe
    ├── dns/           # the first kind: record values, marks, the wire format
    ├── cloudflare/    # the api client, read-only or read-write by type
    ├── desired.rs     # what a machine declares, as its module renders it
    ├── model.rs       # host names, owners, revisions, domain names and zones
    ├── report.rs      # the printed plan
    ├── status.rs      # status.json
    └── test/          # an in-memory cloudflare, and the tests that use it
```

dns records are the first kind, not the only one there will ever be.
cloudflare sells plenty of other things to forget about in a dashboard, and
`kind.rs` is where the next one plugs in.

## releases

every pull request builds every check on x86_64-linux, aarch64-linux and
aarch64-darwin. the nixos vm test needs kvm, so it runs on x86_64-linux
alone. its commits have to follow conventional commits. the
version lives in `Cargo.toml`, and a pull request has to raise it unless it's
labeled `no-release`.

once a new version lands on `main`, ci builds norn for all three systems,
pushes the builds to `https://cache.r3dlust.com` and tags the release. a fleet
that would rather not compile norn on every machine can just use the cache:

```nix
nix.settings = {
  substituters = [ "https://cache.r3dlust.com" ];
  trusted-public-keys = [
    "cache.r3dlust.com-1:Wdkf9mes2ued0P7C7CL3Jax1b4KPuJwQpM3h4G/Z+yI="
  ];
};
```

leave norn's own `nixpkgs` alone (no `inputs.nixpkgs.follows`), though. the
cache holds the build of norn's exact inputs, and a different nixpkgs means a
different build, which every machine then gets to compile for itself.

the cache's own address is a dns record too. it was there before norn was, so
the snake only has to eat its tail once.

## hacking on it

```bash
nix develop        # the toolchain
cargo test         # the tests, against a fake in-memory cloudflare
nix flake check    # the tests, clippy, the package and the module checks,
                   # plus a vm test of the nixos units on linux
```

the tests never talk to the real cloudflare. `src/test/fake.rs` answers like
it does, paging, 404s and all, and logs every request, so a test can assert
that `report` sent nothing but reads.

---

you can still edit norn's records in the cloudflare dashboard if you really
want to. once it's enforcing, it'll just put them back within five minutes,
like a disappointed parent.
