# 001: Cut legacy/v0.6 and announce the reboot

Status: done
Sequence step: 1 ([reboot.md](../reboot.md#sequence))
Blocked by: none
ADRs: [ADR-0001](../adr/0001-reboot-versioning.md)

## Goal

Freeze the pre-reboot source so 0.6.x users have a reference and the reboot can break contracts.

## Scope

- Create `legacy/v0.6` from the current `main` head in kinetix and kinetix-plugins.
- Protect the branch: no feature work, security fixes only if ever needed.
- Add a README notice in both repos: 0.1.0 is a reboot, `kinetix update` will not offer it, and existing installs need a manual reinstall.

## Out of scope

- Deleting releases or tags (done in 027).

## Acceptance

- [x] Both repos have `legacy/v0.6` pushed.
- [x] README notice merged in both repos.
