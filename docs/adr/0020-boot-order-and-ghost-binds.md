# ADR-0020 — Boot order belongs to systemd; `create_host_path:false` is not a boot-race guard

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

**Boot ordering is a boot-ordering problem. It is owned by systemd, scoped to this one service.**

1. **A targeted systemd unit** (`deploy/idea-vault-boot.nix`) with `RequiresMountsFor=<vaultDir>`,
   `After=docker.service`, running `docker compose up -d --force-recreate idea-vault`.
   - `RequiresMountsFor` is what actually waits for the filesystem.
   - `--force-recreate` is load-bearing: by the time the unit runs, the daemon's restart policy has
     already started the container, possibly on a ghost. A plain `up -d` sees unchanged config and
     does nothing. Recreating is the only way to redo a mount.
   - `ExecStartPre=test -f <vaultDir>/.idea-vault-root` applies ADR-0019's verify-don't-assume
     principle to boot: `RequiresMountsFor` proves *something* is mounted, the marker proves it is
     **ours**. A ghost is empty by construction and can never carry it.
2. **Stale ghosts are deleted, not tolerated.** Removing them is what lets guard 1 finally fire as
   ADR-0019 intended. The runbook is in [docs/12-deployment.md](../12-deployment.md); it uses a
   *non-recursive* bind to reach the shadowed underlay and `rmdir` — never `rm -rf` — so it is
   physically incapable of touching a non-empty directory.
3. **`create_host_path: false` stays**, with its scope stated honestly: it stops the daemon
   *inventing* a source. It is a correctness guard against short-syntax regressions, **not** a
   boot-race guard. It was never sufficient alone.

## Consequences

- **ADR-0019's central reversal is itself reversed, narrowly and on evidence.** ADR-0019 rejected all
  host-level ordering because `RequiresMountsFor` on `docker.service` "gates the entire Docker daemon
  on one network-backed volume" — that reasoning is still correct and that option is still rejected.
  What was wrong was concluding *therefore no host change at all*, which only followed from the
  false self-heal premise. A unit ordering **one service** has none of the blast radius of a unit
  ordering **the daemon**. The fastxe stack, cloudbeaver and ollama remain unaffected by the NAS.
- **The app-layer guards are now the second line, not the only line** — and they are why this
  incident cost nothing. The unit prevents the ghost bind; the guards ensure that if it ever happens
  anyway, the failure is loud and non-destructive. Keep both. Neither subsumes the other: the unit
  protects the mount, the guards protect the data from *any* wrong path (typo'd
  `IDEA_VAULT_VAULT_DIR`, wrong `--project-directory`), which no amount of boot ordering addresses.
- **Recovery is `docker compose up -d --force-recreate idea-vault`.** Not `restart` — a restart
  reuses the existing mount namespace and keeps the ghost. This is also why the unit force-recreates.
- **A ghosted container is now diagnosable in one command**, and the device number is the tell:
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

- **`RequiresMountsFor` on `docker.service`** — rejected again, unchanged from ADR-0019: it gates
  every unrelated container on the NAS. This module exists precisely to get the ordering without that
  blast radius.
- **Marker-file tripwire instead of the unit** — bind `./vault/.idea-vault-root` as a second mount
  with `create_host_path:false`. An empty ghost dir cannot contain that file, so the mount fails hard
  during the race even with a stale ghost present, with no host change. Sound, and it closes the
  guard-1 hole at its root. Rejected as the *primary* fix only because it inherits finding 2 — the
  container fails and then stays down until someone runs `up -d`. Worth revisiting as
  defence-in-depth if the ghosts are ever tolerated rather than deleted.
- **Set `restart: no` and let systemd own the lifecycle entirely** — removes the need for
  `--force-recreate`, since the daemon would never pre-start onto a ghost. Rejected: it also
  surrenders crash-restart during normal operation, which is worth more than one recreate per boot.
- **A watchdog container that re-ups stacks when the mount appears** — no host change, but needs the
  docker socket (root-equivalent) and reimplements systemd's dependency graph badly.

## Related

- [ADR-0019](./0019-vault-mount-verified-not-created.md) — the app-layer guards, validated by this
  incident; its guard 1 and self-heal claims are corrected here
- [ADR-0008](./0008-containerized-local-deployment.md) — `vault/` as a host bind mount
- [docs/12-deployment.md](../12-deployment.md) — the ghost-removal runbook and boot-race pitfall
