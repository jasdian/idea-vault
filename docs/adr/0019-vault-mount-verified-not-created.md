# ADR-0019 — The vault mount is verified, not created; reindex refuses an empty-vault wipe

- **Status:** Accepted — guards 2–4 validated in production 2026-07-16; **guard 1 and the self-heal
  claim amended by [ADR-0020](./0020-boot-order-and-ghost-binds.md)**
- **Date:** 2026-07-15
- **Deciders:** owner

## Context

On 2026-07-13 the owner's containerized instance served an **empty idea list for two days** while
reporting healthy. No data was lost — every idea was intact on disk the whole time — but the app was
unusable and nothing warned.

The mechanism, reconstructed from the host and confirmed by measurement:

1. The vault lived on a filesystem that mounts **later than `docker.service`** (an iSCSI-backed LVM
   volume). On the boot in question `docker.service` became active at 06:10:10 and the vault's mount
   unit at 06:10:20 — Docker won by 10 seconds, and being network-backed, it always would.
2. The compose file used the **short bind syntax** (`./vault:/vault`). That records a legacy bind,
   and the Docker daemon **auto-creates a missing bind source as `root:root`**. So Docker created
   `…/idea-vault/vault` on the *underlying* filesystem and bind-mounted that empty directory.
3. LVM then mounted the real volume over the top, hiding the ghost. The container held a bind to an
   inode that no longer had a reachable path. `stat` from inside the container and from the host
   reported **different devices and different inodes** for "the same" directory.
4. `ensure_vault_dir` did an unconditional `create_dir_all` — it **manufactured** a vault rather than
   **verifying** one, so a wrong path became a plausible empty vault.
5. `walk_ideas` returned `Ok(vec![])` (*"a missing `vault_dir` is an empty vault"*), `check_drift`
   saw empty-disk vs populated-index as maximal drift and green-lit a rebuild, and `reindex` deleted
   every derived row, inserted nothing, committed, and logged `reindex complete ideas=0` at INFO.
6. `/admin/health` returned a hardcoded `"status":"ok"` and was typed `Json<Value>` — **structurally
   incapable** of returning non-200. The Docker HEALTHCHECK stayed green throughout.
7. The idea list is enumerated **from the index alone** (correct per [ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)),
   so a wiped index renders an empty UI over a perfectly intact vault.

Every layer reported success. The failure was not any single bug but the absence of a single
skeptical check across six of them.

## Decision

**A vault directory is verified, not manufactured, and a derived index is never destroyed on the
word of a vault that might not be ours.** Four independent guards, at four layers:

1. **Compose declares the bind, and the daemon must not invent it.** `docker-compose.yml` uses the
   long syntax with `create_host_path: false`, recording a `Mounts` entry rather than a legacy
   `Binds` entry. A missing source is a hard start failure — which persists to daemon-initiated
   restarts at boot, not just `compose up`.

   > **Amended by [ADR-0020](./0020-boot-order-and-ghost-binds.md) (measured 2026-07-16).** This
   > guard is real but narrower than described here, and two claims made in this ADR are false:
   > - It refuses to **create** a missing source; it will **bind** a stale ghost left by an older
   >   short-syntax run, because an empty directory answers "does the source exist?" with yes. On
   >   2026-07-16 this exact bind ghosted with the guard in place. It is a regression guard against
   >   the short syntax, **not** a boot-race guard.
   > - The original text continued *"`restart: unless-stopped` then retries until the real
   >   filesystem lands"*. It does not. The restart policy covers container **exits**, not start
   >   failures; a failed mount stays `exited` with `RestartCount=0` forever.
   >
   > The boot race is sidestepped by setting `restart: "no"` (ADR-0020): nothing auto-starts, so no
   > ghost is ever bound during the pre-mount window. Guards 2–4 below are unaffected — they fired
   > correctly on 2026-07-16 and preserved the index.
2. **A vault-root marker** (`.idea-vault-root`). `ensure_vault_dir` returns a `VaultInit` telling the
   caller which of four cases it found. The load-bearing case is **`Suspect`** — directory exists,
   no marker, no ideas — where the marker is deliberately **not written**, because writing it would
   bless a wrong path and make the next boot look healthy forever.
3. **A reindex precondition.** If the walk yields 0 ideas while the index holds >0, `reindex`
   returns `IndexError::RefusingEmptyRebuild` instead of committing the DELETEs. `reindex_forced`
   is the explicit override.
4. **Health that can fail.** `/admin/health` probes the vault and returns **503** when it is
   unreadable or unwritable, turning the existing HEALTHCHECK red.

### The vault decision table (`ensure_vault_dir`)

| Directory | Marker | Idea dirs | → `VaultInit` | Action |
|---|---|---|---|---|
| absent | — | — | `Created` | create + write marker — a genuine first run |
| present | yes | any | `Existing` | nothing; the steady state |
| present | no | ≥1 | `Adopted` | write marker — a real vault predating the marker |
| present | no | 0 | **`Suspect`** | **log ERROR, write nothing** — a ghost looks exactly like this |

`Created` vs `Suspect` is the whole trick: a genuine first run has **no directory**, while Docker
pre-creates the bind source before the app ever runs. The ghost can never be mistaken for first-run.

### Boot is degraded-and-visible, not a hard exit

A `Suspect` vault does not stop boot. Under `restart: unless-stopped` an `exit 1` is a crash loop
that destroys the only surface able to explain the fault — `docker ps` would show
`Restarting (1)`, indistinguishable from a hundred other bugs. Staying up yields
`Up 2 days (unhealthy)` and a `/admin/health` that names the path and the reason. Nothing depends on
`idea-vault` in the compose graph, so an unhealthy container stops nothing else; and plain Docker
does not restart on *unhealthy*, only on *exit* — which is exactly what makes this safe. Refuse to
**destroy** (guard 3); do not refuse to **run**.

## Consequences

- **The guard is a precondition on the input, not a change to the operation.** ADR-0002's
  `reindex(V) == reindex(reindex(V))` and rebuild-from-markdown-alone are untouched: the guard fires
  only on (vault = ∅) ∧ (index ≠ ∅), precisely the state where the identity's premise — *"the index
  is derived from **this** vault"* — is in doubt. `reindex_forced` retains the unconditional
  identity verbatim, and the keystone idempotency test passes unmodified.
- **Deleting the last idea must force the rebuild.** It is the one legitimate way to reach
  (vault = ∅) ∧ (index ≠ ∅). `ideas::delete_idea` therefore uses `reindex_logged_forced`, which is
  provably safe there: the route 404s unless `store::delete_idea` returned true, so reaching the
  rebuild proves the folder existed and the vault was writable — neither of which a ghost can fake.
  Without this the guard would strand the deleted idea in the list forever.
- **Writability is proven by writing.** Permission bits cannot answer the question: the ghost was
  `root:root 0755`, which *looks* writable and is not writable by uid 1000. `/admin/health` creates
  and removes a probe file on each call. This is also the only continuously-observed surface, which
  matters because the incident is literally *a mount that changed under a running process*.
- **Health draws a new line, and it is not "is everything perfect".** An absent **model** stays 200
  ([D20](../05-ai-integration.md) — a model-less stack must pass the HEALTHCHECK). An unusable
  **vault** is 503. The `admin.rs` module doc that read *"Health is always 200"* is now false and has
  been rewritten.
- **A manual `rm -rf vault/*` now needs `?force=1`.** Unavoidable and correct: at that layer "all my
  idea folders vanished" is *indistinguishable* from a mount fault — that indistinguishability is
  the entire premise. The 409 body names `?force=1` explicitly, because that string is the only
  escape route the owner will find.
- **Hosts with a late-mounting vault are now loud.** The container reports the fault instead of
  serving an empty vault, and refuses to destroy the index on its word.

  > **Amended by [ADR-0020](./0020-boot-order-and-ghost-binds.md).** This bullet originally claimed
  > the container "self-heals on the next restart-policy retry once the filesystem lands". That is
  > false: Docker never retries a failed mount. On this dev host the boot race is instead sidestepped
  > by `restart: "no"` — nothing auto-starts, so no ghost is bound before the mount lands, and
  > bring-up is a deliberate manual `docker compose up` after the filesystem is confirmed. The
  > "no host change required" conclusion happens to survive (the host never starts the container),
  > but for a different reason than this bullet gave. Ordering **`docker.service` itself** remains
  > rejected, as below.

## Alternatives considered

- **Order `docker.service` after the vault's mount unit** (`RequiresMountsFor=`). Deterministic and
  fixes the true root cause, but gates the **entire Docker daemon** on one network-backed volume: if
  the NAS is slow or down, no containers start at all. Rejected — the blast radius dwarfs the
  problem. *(Still rejected, and for this reason. The original sentence that followed — "the
  loud-fail + restart retry achieves the same end scoped to this one service" — was false; there is
  no retry. [ADR-0020](./0020-boot-order-and-ghost-binds.md) gets the ordering by putting
  `RequiresMountsFor` on a unit for **`idea-vault` alone**, which is what this bullet's reasoning
  actually argues for.)*
- **Hard-exit on a suspect vault.** Rejected: crash-loop under `restart: unless-stopped`, and it
  kills the diagnostic surface precisely when it is needed. Guard 3 already refuses the destructive
  act, which is the part that actually needed refusing.
- **Marker-aware reindex guard** — refuse only when the marker is *also* absent, allowing a wipe on a
  marked, genuinely-emptied vault (removing the need to force on delete). More precise, but couples
  `index::reindex`'s correctness to a second non-markdown precondition artifact, makes the guard's
  behaviour depend on vault layout, and breaks its in-memory unit-testability. The count-based guard
  needs zero knowledge of vault layout. Revisit if the manual-`rm -rf` papercut bites in practice.
- **Gate health's 503 on `Suspect` too.** Rejected: a hand-made empty vault (and every test harness
  that `mkdir`s a tempdir) is legitimately marker-less. Marker state is reported as advisory detail;
  only readable+writable gates the status code — and writability is what actually caught this.
- **Teach `check_drift` about the marker.** Rejected: `check_drift(ghost) == true` is not a lie, it
  is the literal question that function answers. The judgement belongs in the guard.

## Related

- [ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md) — the rebuild invariant this preserves
- [ADR-0008](./0008-containerized-local-deployment.md) — decided `vault/` is a host bind mount, but
  said nothing about the source existing; this ADR closes that gap
- [docs/12-deployment.md](../12-deployment.md) — the pitfall and its recovery steps
