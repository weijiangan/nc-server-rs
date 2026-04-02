# How to Write Tasks for This Project

All implementation work is tracked in phase files (`PHASE-N.md`). This document describes the conventions so that new tasks are consistent and verifiable.

---

## Guiding principle

> A task is **done** when its verification passes — not when the code is written.

Every task must have a stated way to confirm it works. Unit tests are the preferred mechanism for pure Rust logic; integration tests (Behat feature files) cover HTTP-level behaviour; manual verification steps cover things that cannot be automated yet (PHP shim behaviour, browser login flows).

---

## Phase file structure

Each phase file follows this skeleton:

```markdown
# Phase N — <Short Title>

Goal: one-sentence description of what this phase achieves and why.

---

## Starting state

Describe the state of the codebase at the beginning of this phase:
which stubs exist, what returns 501/stub values, which fields are absent.
Include a table of deferred items from earlier phases if relevant.

---

## N.0 <First section title>

<optional prose context — one paragraph max>

- [ ] Task description — implementation detail inline after em-dash
- [ ] Another task

<optional impl notes block for non-obvious design choices>

**Unit tests:** `cargo test -p <crate>` — list each test name and what it asserts.

**Verify:** <how to confirm correctness once the code is written>

---

## N.1 <Next section title>
…
```

---

## Checkbox states

| Symbol | Meaning |
|---|---|
| `- [ ]` | Not started |
| `- [x]` | Done — verification passed |

Mark a checkbox `[x]` only after its **Verify** step passes, not after writing the code.

---

## Task line format

Each `- [ ]` line should be self-contained enough to implement without reading the whole file. Follow this pattern:

```
- [ ] <imperative verb phrase> — <key detail or why>
```

If a task has sub-steps, add them as an indented numbered list immediately below:

```
- [ ] Do the thing:
  1. First sub-step.
  2. Second sub-step.
```

If the implementation deviates from the description once done, add a blockquote impl note:

```
- [x] Do the thing — expected detail.
  > **Impl note:** what actually happened and why it differs.
```

For deliberate design deviations that need future awareness, use:

```
  > **Deviation:** what changed, why, and what the consequence is.
```

---

## Verification blocks

Every section must end with at least one of these blocks.

### `**Unit tests:**`

Used for Rust logic that can be tested without a running server or database. Name each test explicitly so an implementor knows exactly what to write:

```markdown
**Unit tests:** `cargo test -p nc-auth` — add tests in `nc_auth::session`:
- `test_name_describes_scenario`: inputs → expected output.
- `another_test`: edge case → expected output.
```

Rules:
- Name the crate (`-p nc-auth`) and the module path.
- Name every test individually — do not write "add tests for X".
- Each test name should be a readable sentence (snake_case is fine).
- State the assertion, not just the topic: "returns `None`" not "handles the None case".

### `**Verify:**`

For behaviour that requires running the server, querying a DB, calling an HTTP endpoint, or running an existing test suite:

```markdown
**Verify:** `build/integration/features/auth.feature` — all auth scenarios pass.
```

or for manual steps:

```markdown
**Verify:**
- Send request X → assert response Y.
- Send request Z → assert response W.
```

Point to an existing Behat `.feature` file wherever one covers the scenario. If no feature file exists, write concrete curl/request steps.

---

## Second-decimal sections (N.x.y)

Use numbered sub-sections (`### N.x.y Title`) when a top-level section is large enough that its tasks span multiple concerns. Each sub-section gets its own verification block. The parent section (`## N.x`) keeps a brief prose intro but no checkbox items of its own.

```markdown
## 7.9 Session cookie → uid resolution

<background prose — cookie table, auth flow description, etc.>

### 7.9.1 Config: instanceid in Rust
- [ ] …
**Verified:** …

### 7.9.2 nc-auth session module update
- [ ] …
**Unit tests:** …
**Verify:** …
```

---

## Background/rationale blocks

Use a bold prose block (not a checkbox) for non-obvious design choices that future readers need to understand:

```markdown
**Why X is not used (and when to add it):** explanation…
```

Keep these blocks after the checkbox list and before the **Unit tests** / **Verify** blocks.

---

## Deferred items

When a task cannot be completed in the current phase, leave a placeholder checkbox with a clear label and add a row to the deferred-items table in the **Starting state** section of the phase that will complete it:

```markdown
- [ ] **`{oc:}downloadURL`** real URL — deferred to Phase 7 (requires PHP-FPM proxy).
```

And in the target phase's starting state:

```markdown
| `{oc:}downloadURL` real URL (currently empty string placeholder) | Phase 4.8 |
```

---

## Impl notes vs. errata

| Block | When to use |
|---|---|
| `> **Impl note:**` | Non-obvious implementation choice made during the task; not a bug. |
| `> **Deviation:**` | The implementation differs from the spec description; documents the difference and reason. |
| `> **Errata:**` | A pre-existing bug or incorrect assumption in the spec or earlier code; explains what is wrong and where it is corrected. |

---

## Updating TASKS.md

After adding a new phase file, add a row to the table in `TASKS.md`:

```markdown
| N — Title | [PHASE-N.md](PHASE-N.md) | ⬜ Not started |
```

Status icons:

| Icon | Meaning |
|---|---|
| ⬜ | Not started |
| 🔧 | In progress |
| ✅ | Complete |
