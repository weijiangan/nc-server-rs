# SPECS Navigation Hub (LLM-Friendly)

Organized by intent so each document stays focused and context stays small. The large documents have been broken into per-section files inside their category folder; open the folder's `README.md` for the section index.

## 1) Requirements gathering
Problem framing and the detailed requirement set.
- [`01-requirements/README.md`](01-requirements/README.md)
  - [`problem-statement.md`](01-requirements/problem-statement.md)
  - Requirements, split by section: [`01-requirements/requirements/`](01-requirements/requirements/README.md)

## 2) Specifications and compatibility references
The behavioral/compat contract and known gaps.
- [`02-specifications/README.md`](02-specifications/README.md)
  - API compatibility, split by section: [`02-specifications/api-compatibility/`](02-specifications/api-compatibility/README.md)
  - [`improvements.md`](02-specifications/improvements.md)

## 3) Implementation planning
How the build is sequenced.
- [`03-implementation-plan/README.md`](03-implementation-plan/README.md)
  - Plan, split by section: [`03-implementation-plan/plan/`](03-implementation-plan/plan/README.md)

## 4) Tasks (doing now / planned)
Phase-by-phase execution tracking. Phase files are kept whole (one file per phase).
- [`04-tasks/README.md`](04-tasks/README.md)
  - [`tasks.md`](04-tasks/tasks.md) — status table
  - `04-tasks/phase-0.md` … `04-tasks/phase-8.md`

## 5) LLM guidelines and playbooks
Rules and step-by-step procedures for agents working in this repo.
- [`05-llm-playbooks/README.md`](05-llm-playbooks/README.md)
  - [`CONTRIBUTING-tasks.md`](05-llm-playbooks/CONTRIBUTING-tasks.md)

---

## Directory map

```
SPECS/
├── 01-requirements/
│   ├── problem-statement.md
│   └── requirements/         # REQ split into 20 section files + index
├── 02-specifications/
│   ├── improvements.md
│   └── api-compatibility/    # API_COMPATIBILITY split into 15 section files + index
├── 03-implementation-plan/
│   └── plan/                 # IMPL_PLAN split into 13 section files + index
├── 04-tasks/
│   ├── tasks.md
│   └── phase-0.md … phase-8.md
└── 05-llm-playbooks/
    └── CONTRIBUTING-tasks.md
```
