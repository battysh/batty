# Typical Software Team: Org Chart, Communication Patterns & Information Flow

A reference document for organizing a software engineering team building a product of moderate complexity (SaaS platform, significant internal service, etc.).

---

## 1. Reporting Hierarchy

```
Engineering Director / VP Eng
│
├── Product Manager (dotted line — PM often reports to a PM org, not eng)
│
├── Engineering Manager
│   ├── Tech Lead / Staff Engineer
│   ├── Senior Backend Engineer
│   ├── Backend Engineer
│   ├── Backend Engineer
│   ├── Senior Frontend Engineer
│   ├── Frontend Engineer
│   └── Frontend Engineer
│
├── QA Lead (sometimes reports to EM, sometimes separate org)
│   └── QA Engineer
│
├── DevOps / SRE (sometimes shared across teams)
│   └── SRE Engineer
│
└── UX Designer (dotted line — usually reports to a design org)
```

Team size: ~12-15 people. This is a single "two-pizza team" stretched to its practical limit. Larger products split into multiple such teams.

---

## 2. Communication Heat Map

Who actually talks to whom, and how often:

```
                PM    EM    TL    Sr.BE  BE   Sr.FE  FE   QA   SRE  UX
PM              —     ███   ███   ██     ·    ██     ·    █    ·    ███
EM              ███   —     ███   ██     █    ██     █    ██   █    █
Tech Lead       ███   ███   —     ███    ██   ███    ██   ██   ██   █
Sr. Backend     ██    ██    ███   —      ███  ██     █    ██   ██   ·
Backend         ·     █     ██    ███    —    █      █    █    █    ·
Sr. Frontend    ██    ██    ███   ██     █    —      ███  ██   ·    ███
Frontend        ·     █     ██    █      █    ███    —    █    ·    ██
QA              █     ██    ██    ██     █    ██     █    —    █    ·
SRE             ·     █     ██    ██     █    ·      ·    █    —    ·
UX              ███   █     █     ·      ·    ███    ██   ·    ·    —

███ = daily/constant    ██ = several times/week    █ = weekly    · = rare
```

---

## 3. Key Communication Triangles

Three clusters drive most decisions:

### Triangle 1: The "What" Triangle

```
    PM
   / \
  UX — Sr. Frontend
```

These three figure out what the user sees. PM brings requirements, UX designs the experience, Sr. Frontend says what's feasible in the UI. They're in a room (or a thread) together multiple times a day.

### Triangle 2: The "How" Triangle

```
    Tech Lead
    /      \
Sr. Backend — Sr. Frontend
```

These three figure out the technical approach. API contracts, data models, where logic lives (client vs. server), performance trade-offs. The Tech Lead arbitrates when backend and frontend disagree.

### Triangle 3: The "Ship" Triangle

```
    EM
   / \
  TL — QA Lead
```

These three figure out whether it's ready. EM tracks timeline, TL tracks technical completeness, QA tracks quality. They decide together when to cut scope vs. slip dates.

### The Bridge Role: Tech Lead

The Tech Lead appears in two of the three triangles. They're the most communication-heavy role on the team — they translate between PM-speak ("users need X") and engineer-speak ("that requires Y architecture change"). This is why Tech Lead burnout is so common.

---

## 4. Role-by-Role Breakdown

### Product Manager

- Talks to: EM (priorities, timelines), Tech Lead (feasibility, trade-offs), UX (design), Sr. engineers (clarifying requirements), stakeholders outside the team
- Owns: backlog, feature specs, prioritization, stakeholder communication
- Doesn't talk much to: junior engineers, SRE (unless there's an operational constraint on the product)

### Engineering Manager

- Talks to: everyone on the team (1:1s), PM (capacity/timeline), Tech Lead (technical risk), QA (quality gates), Director (up-management)
- Owns: people, process, hiring, sprint ceremonies, removing blockers, sprint velocity and delivery commitments

### Tech Lead / Staff Engineer

- Talks to: PM (feasibility), EM (technical risk), all senior engineers (design decisions), QA (testability), SRE (operability)
- Owns: technical design docs / RFCs, architecture decisions, code quality standards, mentoring
- Does NOT own: people management, prioritization, timeline commitments

### Senior Backend Engineer

- Talks to: Tech Lead (design), other backend engineers (code review, pairing), Sr. Frontend (API contracts), QA (test scenarios), SRE (deployment, monitoring)
- Owns: backend services, data models, API design, performance
- Mentors junior backend engineers through code review

### Senior Frontend Engineer

- Talks to: Tech Lead (design), UX (translating designs to implementation), other frontend engineers (code review), PM (UX feasibility), QA (UI test scenarios)
- Owns: frontend architecture, component library, client-side performance

### QA Lead

- Talks to: EM (release readiness), Tech Lead (testability of design), all engineers (bug reports, test coverage)
- Owns: test plans, regression suites, release sign-off
- Often the person who says "no, this isn't ready" — needs organizational backing from EM

### SRE / DevOps

- Talks to: Tech Lead (operability requirements), Sr. Backend (deployment, monitoring), EM (incident process)
- Owns: CI/CD pipeline, monitoring/alerting, infrastructure, on-call runbooks
- Often shared across 2-3 teams, which means they're a bottleneck

### UX Designer

- Talks to: PM (requirements), Sr. Frontend (feasibility), Frontend engineers (implementation details)
- Owns: wireframes, prototypes, design system contributions, user research findings
- Often shared across teams like SRE

---

## 5. Information Flow for a Typical Feature

```
1. PM writes a 1-pager (problem, user need, success metrics)
         │
         ▼
2. PM + UX + Sr. Frontend discuss UX approach
         │
         ▼
3. Tech Lead writes RFC / design doc
   (Sr. Backend + Sr. Frontend contribute)
         │
         ▼
4. EM breaks it into sprint tickets with Tech Lead
         │
         ▼
5. Backend engineers build APIs
   Frontend engineers build UI        ← happening in parallel
   (Sr. engineers review PRs)
         │
         ▼
6. QA tests against acceptance criteria
         │
         ▼
7. SRE reviews deployment plan, monitoring
         │
         ▼
8. EM + TL + QA decide: ship or iterate
         │
         ▼
9. PM validates with stakeholders / users
```

Steps 5-8 often loop multiple times within a sprint.

---

## 6. Comparison with Research/AI Teams

| Aspect | Software Team | Research Team (e.g. CICERO) |
|--------|--------------|----------------------------|
| Central coordinator | PM (what to build) | Tech Lead (what to explore) |
| Heaviest communicator | Tech Lead (bridge role) | Tech Lead (integration role) |
| Information flow | Linear (spec → build → test → ship) | Convergent (components → integration) |
| Key meeting | Sprint planning + standup | Architecture sync + playtest |
| Biggest communication risk | PM ↔ Engineering misalignment | Component A ↔ Component B interface mismatch |
| Decision authority | PM decides scope, TL decides approach | Tech Lead decides both |
| Planning | Sprint-driven, date-commitments | Milestone-driven, directional |
| IC autonomy | Medium (work from specs) | High (design own experiments) |
| Definition of done | Deployed, tested, monitored | Beats baseline, statistically significant |
| Code quality bar | Production-grade | Functional, experimental |
| On-call | Standard rotation | None |
| Key risk | Execution (will it ship on time?) | Feasibility (will it work at all?) |
| Hiring | Generalist engineers | PhD, domain specialists |
| Deliverable | Running system serving users | Paper + open-source code |

The fundamental structural difference: a software team has a **pipeline** (requirements flow through stages), while a research team has a **hub-and-spoke** (components develop independently and converge at integration). This is why the software team needs a PM to manage the pipeline, while the research team needs a strong Tech Lead to manage the hub.
