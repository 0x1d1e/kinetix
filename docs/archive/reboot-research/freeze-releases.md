# Freeze current releases

|Project|Release behavior|Lesson for Kinetix|
| ---------| ---------------------------------------------------------------------------------------------------------------------------------------------------------| -----------------------------------------------------------------------------------------|
|**pi-free**|npm release = published/stable-ish line; GitHub may move ahead; volatile provider facts documented as snapshots, not guarantees.[GitHub](https://github.com/apmantza/pi-free/blob/master/docs/free_models.md?utm_source=chatgpt.com)|Preserve shipped history; clearly separate verified guarantees from experimental state.|
|**9router**|Monotonic`0.x`; immutable-ish version tags/pins; prereleases do**not**automatically become`latest`; explicit promotion/rollback supported.[GitHub](https://github.com/decolua/9router/blob/master/DOCKER.md?utm_source=chatgpt.com)|Keep old versions addressable; control what`latest`means.|
|**OmniRoute**|Strongest model:`release/vX.Y.Z`= active cycle,`main`= published line, tag = shipped snapshot. During freeze, frozen cycle stops changing while next cycle continues.[GitHub](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.50/docs/ops/BRANCHING_MODEL.md?utm_source=chatgpt.com)|Best model to copy.|

### For Kinetix

“Freeze” should mean:

```
current HEAD
   │
   ├── legacy/v0.5 ── old code, no feature work
   │      └── existing v0.5.x tags preserved
   │
   └── reboot branch ── new contracts/architecture
```

Do:

- Preserve **all existing tags/releases**.
- Cut `legacy/v0.5` for both Kinetix + plugins.
- Mark it **legacy / unsupported / known compatibility failures**.
- Only security/data-loss fixes there.
- Stop new feature PRs against it.
- Keep old binaries/artifacts installable for reproducibility.
- New line gets new compatibility/conformance gates before becoming `latest`.

### One correction to my previous recommendation

I **would not publish the reboot as** **`v0.1.0`** **under the same release identity**.

pi-free, 9router, OmniRoute all move versions **forward**, not backward. Resetting `0.5.x → 0.1.0` creates ordering/update ambiguity.

Better:

**`v0.6.0-alpha.1`**  **→**  **`v0.6.0`**

while internally treating it as **Kinetix architecture v1 / reboot**.

If you insist on literal **v0.1.0**, make it a genuinely new distribution/repo/package identity. [GitHub](https://github.com/diegosouzapw/OmniRoute/blob/release/v3.8.50/docs/ops/BRANCHING_MODEL.md?utm_source=chatgpt.com)

So: **freeze 0.5.x, don't erase it; reboot architecture, not SemVer history.**


