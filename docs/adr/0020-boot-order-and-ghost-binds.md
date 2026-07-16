# ADR-0020 — Skip the boot race (nothing auto-starts); `create_host_path:false` is not a boot-race guard

- **Status:** Accepted
- **Date:** 2026-07-16
- **Deciders:** owner
- **Amends:** [ADR-0019](./0019-vault-mount-verified-not-created.md) (guard 1 and the self-heal claim)

## Context

On 2026-07-16 the host rebooted. This was the first real test of [ADR-0019](./0019-vault-mount-verified-not-created.md),
and it was not a drill: the boot race ran, `docker.service` became active at 06:40:27 and
`home-john-dump.mount` at 06:40:41 — Docker won by 14 seconds, as ADR-0019 predicted it always would.

**ADR-0019's app-layer guards worked exactly as designed, and this is the part worth keeping.** From
the boot log, unedited:

```
06:40:16 ERROR vault directory is empty and carries no .idea-vault-root marker — if this is not a
               brand-new vault, IDEA_VAULT_VAULT_DIR is wrong or the vault filesystem is not
               mounted yet; refusing to treat it as authoritative  dir=/vault
06:40:16 ERROR index preserved; the vault is empty but the index is not
               error=refusing to reindex: vault /vault has no ideas but the index holds 3
```

`Suspect` fired and wrote no marker. `RefusingEmptyRebuild` fired and preserved `index.db` byte-for-byte.
`/admin/health` returned 503 `{"status":"vault-unusable","vault":"unwritable"}` and the container went
`unhealthy`. Guards 2, 3 and 4 turned a silent two-day data-loss-shaped outage into a loud, correctly
diagnosed, zero-data-loss one. **That is the ADR-0019 design working, and it stands.**

But the container was still ghosted, which ADR-0019 said could not happen. Two of its claims were
measured false:

### 1. `create_host_path: false` does not stop a ghost bind — only a ghost *creation*

The vault bind used the long syntax with `create_host_path: false`, exactly as ADR-0019 mandates, and
was ghosted anyway. Its `/vault` resolved to `dev=8:2` (the `/home` filesystem) with root
`/home/john/dump/…/vault`, while the real volume is `dev=254:0`.

The option only refuses to **create** a missing source. It cannot tell a real directory from a stale
one, and the stale ghost from the 2026-07-13 incident — the very ghost ADR-0019 was written about —
was still sitting on the underlying filesystem, unremoved. At 06:40:27 the daemon asked *"does
`./vault` exist?"*, the ghost answered **yes**, and the bind proceeded. Read-only inspection of the
underlay via a non-recursive bind confirmed it: `inode=31078741 owner=0:0`, empty — the identical
inode the container reported.

The guard's premise was that the source would be *absent* during the race. A stale ghost makes it
*present*. **A ghost is not a transient artifact of one bad boot; it is a permanent trap that re-arms
itself every boot until deleted.** ADR-0019 assessed the leftover ghost as harmless while
`create_host_path:false` held. That assessment was wrong and had it exactly backwards: the ghost is
precisely what makes the guard not hold.

The same underlay inspection found the shadow tree is not vault-specific — it mirrors most of the
host's bind sources (every `api-mono/services/*/publish`, both `ui-mono` `dist`/`nginx.conf`/
`entrypoint.sh`, the routefusion worktree). Any compose guard added elsewhere on this host inherits
the same defeat.

### 2. `restart: unless-stopped` does not retry a failed mount

ADR-0019 states the policy "retries until the real filesystem lands", and rests its whole
no-host-change conclusion on that. Measured directly — container with a bind + `--restart
unless-stopped`, source removed, container killed to force a daemon-initiated restart:

```
t+10s status=exited restarts=0
t+40s status=exited restarts=0     # source restored here
t+65s status=exited                # still exited; never retried
```

The restart policy covers container **exits**, not **start failures**. A mount failure leaves the
container `exited` with `RestartCount=0`, permanently, even once the filesystem returns. There is no
self-heal and there never was. ADR-0019's "fail loud + self-heal, no host change required" delivered
only the first half.

## Decision

**Don't win the boot race — refuse to enter it. Nothing auto-starts; the developer brings each
stack up by hand, from its own folder, when they sit down to work.**

The race only exists because the daemon restarts containers at boot, *before* the late iSCSI mount
lands. Remove the auto-start and the window in which a ghost can be bound never opens. This is a
local development host, not a production server — there is no uptime requirement that a boot-time
auto-start serves, so surrendering it costs nothing and removes an entire failure class.

1. **`restart: "no"` on every service** in `idea-vault/docker-compose.yml` and the three fastxe
   `docker-compose.local.yml` files (18 directives). The daemon never starts these containers on its
   own — not at boot, not on crash. A dev container that dies stays dead and visible, rather than
   silently retrying (which is how `routefusion` reached 52 restarts unnoticed).
2. **Bring-up is explicit, from the folder, with the right file.** Each stack is started by a human
   who has confirmed the vault filesystem is mounted:
   ```bash
   cd git-moje/idea-vault && docker compose up -d                      # .env pins yml:gpu:claude
   cd fast-xe/api-mono     && docker compose -f docker-compose.local.yml -f docker-compose.override.yml up -d
   cd fast-xe/backend-mono && docker compose -f docker-compose.local.yml up -d
   cd fast-xe/ui-mono      && docker compose -f docker-compose.local.yml up -d
   ```
   No stack is stitched from a temp file that outlived its session (a `~/.claude/jobs/.../tmp/*.yml`
   was baked into the live `fastxe-v2-local` project identity before this change).
3. **Stale ghosts are still deleted, not tolerated.** `restart: "no"` prevents *new* ghost binds; it
   does not remove the ones already on the underlay from past short-syntax runs. Those are cleared
   with the [docs/12-deployment.md](../12-deployment.md) runbook — a *non-recursive* bind to reach
   the shadowed underlay, then `rmdir` (never `rm -rf`; its refusal on non-empty dirs is the safety
   property that keeps it away from the 1.8 GB of real cgc data misdirected onto the underlay).
4. **`create_host_path: false` stays**, scope stated honestly: it stops the daemon *inventing* a
   source. A correctness guard against short-syntax regressions — never, on its own, a boot-race
   guard.

A `deploy/idea-vault-boot.nix` unit was written first (systemd `RequiresMountsFor` + force-recreate)
and then **removed**: it solves auto-start-onto-a-ghost, but manual bring-up means there is no
auto-start to protect. See the alternatives below for why the simpler decision won.

## Consequences

- **No host-level change is needed after all — but for a different reason than ADR-0019 gave.**
  ADR-0019 claimed "no host change required" on the back of a false self-heal. The claim happens to
  hold, because the *host* never starts these containers: they start only when a human runs
  `docker compose up`, by which point they have confirmed the mount. Both the daemon-gating unit
  (rejected in ADR-0019, correctly) and the one-service unit (this ADR's first draft) are
  unnecessary once nothing auto-starts.
- **The cost is explicit: containers do not come back after a reboot until you start them.** On a
  development host this is acceptable and arguably desirable — you get a clean slate and start only
  what you are working on. It would be the wrong call on a server; this decision is scoped to a
  local dev host and should not be copied to one that must survive reboots unattended.
- **Crash-restart is also surrendered**, deliberately. `restart: "no"` means a crashing dev
  container stays down and visible instead of masking a broken build behind an infinite retry — the
  `routefusion` 52-restart loop is the anti-pattern this removes.
- **The app-layer guards remain the safety net, and they are why the incident cost nothing.** They
  are independent of boot policy: they catch *any* wrong vault path — a typo'd `IDEA_VAULT_VAULT_DIR`,
  a wrong `--project-directory`, a hand-run `up` before the mount landed — none of which auto-start
  removal addresses. Keep them exactly as ADR-0019 shipped them.
- **Recovery from a ghost is still `up -d --force-recreate <svc>`**, not `restart` — a restart reuses
  the existing mount namespace and keeps the ghost. Relevant whenever a container was started by hand
  before the mount was ready.
- **A ghosted container is diagnosable in one command**, and the device number is the tell:
  ```
  pid=$(docker inspect <c> --format '{{.State.Pid}}')
  awk '$5=="/vault" {print $3, $4}' /proc/$pid/mountinfo    # want 254:0 + a volume-relative root
  ```
  Do not hardcode the device number in a sweep script: `8:34` from the 2026-07-15 sweep was `8:2`
  after this reboot. Compare against `stat -c %D` on the real path instead of a remembered constant.
- **This ADR is why the incident log is worth keeping verbatim.** ADR-0019 was written from a correct
  diagnosis and still shipped two false mechanisms, because the mechanisms were reasoned about rather
  than measured. Both took one command each to falsify. Measure the guard, not just the bug.

## Alternatives considered

- **A targeted systemd unit** (`deploy/idea-vault-boot.nix`, written and then removed) —
  `RequiresMountsFor=<vaultDir>`, `After=docker.service`, `up -d --force-recreate idea-vault`. It
  keeps auto-start *and* orders it behind the mount, gating one service rather than the daemon (so
  none of ADR-0019's daemon-gating blast radius). Genuinely correct, and the right answer **if
  boot-time auto-start is a requirement**. Rejected here because on this dev host it is not: once
  `restart: "no"` removes auto-start, the unit orders something that no longer happens. Preserved in
  git history (and this bullet) so it can be resurrected verbatim if idea-vault ever moves to a host
  that must come back unattended.
- **`RequiresMountsFor` on `docker.service`** — rejected, unchanged from ADR-0019: it gates every
  unrelated container on the NAS. Both the one-service unit above and this decision avoid that.
- **Marker-file tripwire** — bind `./vault/.idea-vault-root` as a second mount with
  `create_host_path:false`. An empty ghost dir cannot contain that file, so the mount fails hard even
  with a stale ghost present. Sound defence-in-depth that closes the guard-1 hole at its root; it
  became moot here because with no auto-start there is no unattended `up` to protect. Worth adding if
  auto-start ever returns.
- **`restart: on-failure`** — auto-restart on crash but (believed) not at boot. Rejected on two
  counts: its boot behaviour was *not measured* (and this whole ADR exists because an unmeasured
  restart-policy claim was wrong), and it reintroduces the silent crash-loop that hid
  `routefusion`'s 52 restarts. `"no"` is the only value that guarantees the requirement without a
  measurement leap.
- **A watchdog container that re-ups stacks when the mount appears** — no host change, but needs the
  docker socket (root-equivalent) and reimplements systemd's dependency graph badly.

## Related

- [ADR-0019](./0019-vault-mount-verified-not-created.md) — the app-layer guards, validated by this
  incident; its guard 1 and self-heal claims are corrected here
- [ADR-0008](./0008-containerized-local-deployment.md) — `vault/` as a host bind mount
- [docs/12-deployment.md](../12-deployment.md) — the ghost-removal runbook and boot-race pitfall
