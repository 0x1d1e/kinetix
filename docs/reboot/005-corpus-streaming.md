# 005: Corpus: streaming cases

Status: open
Sequence step: 2 ([reboot.md](../reboot.md#sequence))
Blocked by: [002](002-corpus-runner.md)

## Goal

Pin streaming behavior byte-level where the client sees it.

## Scope

- Arbitrary chunk boundaries, split UTF-8, multiple events per chunk, usage events, thinking/text/tool ordering, parallel tool deltas, `[DONE]` handling per protocol, early upstream close, error before and after commit.
- Cover both the canonical lane and same-format passthrough.

## Acceptance

- [ ] Cases exist for every listed item on both lanes; failing ones are in the manifest.
