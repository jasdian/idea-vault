# idea-vault boot ordering — NixOS module (ADR-0020)
#
# WHY THIS EXISTS
#
# The vault lives on a filesystem that mounts LATER than docker.service (here: an
# iSCSI-backed LVM volume; measured 2026-07-16 — docker at 06:40:27, the mount at
# 06:40:41). Docker wins that race on every boot, and being network-backed, always will.
#
# ADR-0019 assumed two things that were measured FALSE on 2026-07-16:
#
#   1. "create_host_path:false makes a missing source a hard failure."
#      Only if the source is genuinely missing. It refuses to CREATE a source; it
#      binds a pre-existing one happily. A stale root-owned ghost dir left on the
#      underlying filesystem by an older short-syntax run answers "does ./vault
#      exist?" with YES, so the daemon binds the ghost and the guard never fires.
#
#   2. "restart: unless-stopped retries until the filesystem lands."
#      It does not. The restart policy covers container EXITS, not start failures.
#      A failed mount leaves the container `exited` with RestartCount=0, and it stays
#      there even after the filesystem appears. There is no self-heal to rely on.
#
# So boot ordering has to be owned by something that can actually wait for the mount.
# That is this unit.
#
# WHY NOT ORDER docker.service ITSELF (RequiresMountsFor on the daemon)
#
# Rejected deliberately, and this module is the whole point of the distinction: gating
# the DAEMON would gate every unrelated container (the fastxe stack, cloudbeaver,
# ollama) on one network-backed volume — if the NAS is slow or down, nothing on the
# host starts. This unit gates ONLY idea-vault on ONLY its own data. Blast radius: one
# service, the one that actually needs the volume.
#
# INSTALL
#
#   imports = [ /home/john/dump/git-repos/git-moje/idea-vault/deploy/idea-vault-boot.nix ];
#
# then `nixos-rebuild switch`. Verify with:
#
#   systemctl cat idea-vault-boot.service          # ordering resolved as expected
#   systemctl list-dependencies idea-vault-boot    # should show home-john-dump.mount
#   systemctl start idea-vault-boot                # safe to run any time; idempotent
#
{ config, lib, pkgs, ... }:

let
  # The repo checkout. Everything below is derived from this one path.
  repoDir = "/home/john/dump/git-repos/git-moje/idea-vault";

  # The vault directory itself — what RequiresMountsFor keys on, and where the
  # .idea-vault-root marker lives.
  vaultDir = "${repoDir}/vault";

  # The system docker. `docker compose` resolves its CLI plugin from a nix store path
  # baked into the wrapper, so it works under systemd's empty environment (verified:
  # `env -i /run/current-system/sw/bin/docker compose version` -> 5.1.3). Do not
  # substitute pkgs.docker here: the compose plugin is a separate package and the
  # system profile is what already has them wired together.
  docker = "/run/current-system/sw/bin/docker";
in
{
  systemd.services.idea-vault-boot = {
    description = "Start idea-vault after its vault filesystem is mounted";
    documentation = [ "file://${repoDir}/docs/adr/0020-boot-order-and-ghost-binds.md" ];

    after = [ "docker.service" ];
    requires = [ "docker.service" ];
    wantedBy = [ "multi-user.target" ];

    unitConfig = {
      # The load-bearing line. Expands to Requires= + After= on the mount unit backing
      # this path (home-john-dump.mount), so the unit cannot run until the real
      # filesystem is there. Keyed on the vault path rather than the mount unit name so
      # it keeps working if the mount is ever moved or renamed.
      RequiresMountsFor = vaultDir;
    };

    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      WorkingDirectory = repoDir; # docker compose reads .env (COMPOSE_FILE pin) from cwd

      # Verify, don't assume — the same principle as ADR-0019's vault marker, applied to
      # boot. RequiresMountsFor guarantees SOMETHING is mounted; the marker proves it is
      # OUR vault. A ghost is empty by construction, so it can never carry the marker.
      # If this fails, the unit fails loudly and leaves the container alone rather than
      # force-recreating onto the wrong filesystem.
      ExecStartPre = "${pkgs.coreutils}/bin/test -f ${vaultDir}/.idea-vault-root";

      # --force-recreate is NOT optional. By the time this unit runs, the daemon's
      # restart policy has already started idea-vault — possibly bound to a ghost. A
      # plain `up -d` would see a running container with unchanged config and do
      # nothing, leaving the ghost bind in place. Recreating is the only way to redo the
      # mount. Scoped to `idea-vault`, so ollama (named volumes only, unaffected by the
      # race) is started if needed but never needlessly recreated.
      ExecStart = "${docker} compose up -d --force-recreate idea-vault";

      # Image pulls / ollama's healthcheck start_period can make first boot slow.
      TimeoutStartSec = "300";
    };
  };
}
