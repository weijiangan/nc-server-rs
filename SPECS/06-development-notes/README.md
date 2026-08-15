# 06 · Development Notes

Incident-derived engineering lessons, one numbered file per entry. CLAUDE.md's
Engineering Hygiene holds the general guidance; these notes hold the incident
specifics — what happened, what we found out, the options, the choice, and the
follow-ups.

## Creating a new note

- **One file per entry**, named `NN-xxx-yyy.md`. The README deliberately carries no per-entry summaries.
- **The title states the defect** — the thing that was wrong or missing — not the broader lesson it illustrates.
- **Structure each entry as**: the observable failure; the root cause(s), grounded in `file:line` references; the options weighed with their real costs; the choice made and the reasoning, including why the alternatives lost; the verification evidence; and any follow-ups worth revisiting.
- **Link back to the task trail** — the phase docs, plans, and commits involved — so the entry connects to the execution history.

Back: [`../README.md`](../README.md)
