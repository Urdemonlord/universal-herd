<!--
Pakem PR template v1.2. Delete the sections that do not apply, do not delete
the checklists. Fill the Context7 table for every new library API this PR
introduces. Write the PR body to a file first, then use --body-file.
Never use --body "..." containing backticks.
-->

## Summary

<!-- What this PR changes, in one or two sentences. Link the gap or issue it closes. -->

## Context7

<!-- One row per new library, crate, package, API, or function this PR uses.
A PR that introduces a new API without this table is not ready for review. -->

| Library | Version | Source checked |
|---|---|---|
| <!-- example: hickory-resolver --> | <!-- example: 0.26.3 --> | <!-- example: Context7, docs.rs --> |

New dependency justification (when this PR adds one): why it is needed, the std-only or native alternative, the license, and whether it raises the minimum supported version.

## Definition of Done

Tick the list that fits this PR. Delete the other one.

### Runtime PR (changes product behavior)

- [ ] New tests are green AND proven able to go red: unit, integration, a real smoke test, plus a bug injection reported as `injected -> RED`.
- [ ] Claims are judged with the Engineering Evidence format below.
- [ ] Tests, linter, formatter, and build are green (or the exceptions are recorded below).
- [ ] Live verification with a real scenario, not only unit tests. A screenshot or log is attached.
- [ ] No mockup data, no fake numbers in the UI.
- [ ] Docs updated: the protocol document for a new method or endpoint, the design document for a UI change.
- [ ] CodeRabbit findings triaged, the valid ones fixed or their reason recorded.
- [ ] Ponytail review done: deletion findings handled or rejected with a reason, closed with `net: -N lines possible` or `Lean already. Ship.`
- [ ] A change to the shape of persistent state raises the version and keeps old files readable.

### Non-runtime PR (documentation only, static config only, internal tooling only)

- [ ] Evidence fits the kind: documentation, link check and factual review; config, validation and a runtime check; internal tooling, the commands run and their results.
- [ ] No unproven claim about product behavior.
- [ ] Ponytail review done, with the same closing.
- [ ] No live-verification screenshot or log, no bug injection, and no runtime smoke test is needed, because no runtime behavior changes. When this PR turns out to touch runtime behavior, it moves to the runtime DoD.

## Engineering Evidence

<!-- One block per claim. A claim is downgraded when its evidence is weaker
than its words. Exit code 0 alone is not evidence. -->

```text
CLAIM: <one sentence, an observable behavior>

VERDICT: VERIFIED | PARTIAL | FAILED

EVIDENCE:
- <the exact command run, and its result>
- <the exact command run, and its result>

UNVERIFIED:
- <what is not yet proven, and why>
```

## Exceptions

<!-- One row per approved exception this PR relies on. An exception is only
valid when Arya approved it, with a validity period, recorded in the project
supplement and CHANGELOG. An exception whose validity period has ended
without renewal is a violation, not an exception. -->

| Rule | Reason | Approved | Validity period |
|---|---|---|---|
| <!-- example: clippy Tauri E0463 --> | <!-- reason --> | <!-- date, by Arya --> | <!-- until when, or without limit --> |

## Ponytail review

<!-- One line per finding: location, what is cut, what replaces it.
Close with `net: -N lines possible`, or `Lean already. Ship.` -->
