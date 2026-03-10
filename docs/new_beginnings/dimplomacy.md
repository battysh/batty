# Team Structure for Solving Diplomacy-Class AI Problems

A reference document for organizing a scientific/engineering team to solve complex multi-agent games requiring strategic reasoning and natural language communication. Modeled on Meta FAIR's CICERO project (Science, Nov 2022).

---

## 1. Background

CICERO achieved human-level performance in full-press Diplomacy — a 7-player strategy game requiring negotiation, cooperation, and occasional deception via natural language. The system combines a reinforcement-learning-based strategic engine with a 2.7B parameter dialogue model, coordinated through an intent-conditioned pipeline.

Key authors: Noam Brown (Tech Lead), Anton Bakhtin, Adam Lerer, Emily Dinan, Gabriele Farina, Colin Flaherty, Daniel Fried, Alexander Miller, and others (~15-20 researchers total).

The team was organized around component ownership with a single technical leader holding integration-level context, not full depth in every component.

---

## 2. Org Chart

```
Tech Lead / PI  ←→  Engineering Manager
      │
      ├── Strategic Engine Lead (2-3 ICs)
      ├── Dialogue System Lead (2-3 ICs)
      ├── Integration & Evaluation Lead (1-2 ICs)
      └── Game Theory Lead (1-2 ICs)

      + Domain Expert (part-time consultant)

Total: ~15-20 people
```

---

## 3. Ownership Map

### 3.1 Tech Lead / PI

| Owns | Does NOT Own |
|------|--------------|
| Research roadmap & milestones | Sprint planning, ticket grooming |
| Master architecture design doc | Individual component design docs |
| Final say on integration decisions | People management, hiring process |
| Paper writing & external comms | Compute budget allocation |
| Go/no-go on live experiments | Day-to-day code review |

Maintains the **master architecture doc** — a living document (~10-15 pages) specifying components, interfaces between them, and assumptions each component makes about the others. Updated every few weeks.

The Tech Lead's job is NOT to understand every detail. It is to understand every *interface* — the contracts between components. Each sub-lead owns the depth within their component.

### 3.2 Engineering Manager

| Owns | Does NOT Own |
|------|--------------|
| Hiring, onboarding, retention | Research direction |
| Compute budget & resource allocation | Architecture decisions |
| Project-level Jira board (cross-team) | Component-level technical choices |
| Meeting cadence & team rituals | Paper authorship decisions |
| Vendor relationships (cloud, data) | Experiment design |
| Risk tracking (timeline, people, infra) | What the model should do |

This person notices "the dialogue team has been blocked for 2 weeks waiting on a new intent format from the strategy team" and fixes it. They own the *process*, not the *product*.

### 3.3 Strategic Engine Lead

| Owns | Does NOT Own |
|------|--------------|
| RL training pipeline & self-play infra | Dialogue generation |
| Action space representation | How intents get turned into messages |
| Human regularization (piKL) approach | Evaluation against humans |
| Search algorithm at inference time | Game server integration |
| Component design doc: "Strategic Engine" | |
| Their sub-board in Jira | |

**ICs:**
- IC1: Self-play training loop, reward shaping, hyperparameter sweeps
- IC2: Search/planning at inference time, action space encoding
- IC3 (if needed): Human behavior prediction model (what will other players do?)

### 3.4 Dialogue System Lead

| Owns | Does NOT Own |
|------|--------------|
| Language model fine-tuning | What intent to generate messages for |
| Controllable generation (intent → message) | Strategic planning |
| Message filtering & re-ranking pipeline | Whether to send a message at all (that's integration) |
| Training data curation (human game logs) | |
| Component design doc: "Dialogue System" | |
| Their sub-board in Jira | |

**ICs:**
- IC1: Base model training, fine-tuning on Diplomacy corpus
- IC2: Controllable generation — conditioning on intent, filtering contradictions
- IC3 (if needed): Message understanding — parsing incoming human messages into structured beliefs

### 3.5 Integration & Evaluation Lead

| Owns | Does NOT Own |
|------|--------------|
| The runtime pipeline (strategy ↔ dialogue loop) | Individual component quality |
| Live game infrastructure (bot on webDiplomacy) | RL training |
| Evaluation framework & metrics | Language model training |
| Ablation study design | Game theory research |
| End-to-end test suite | |
| "System Design" doc | |

**ICs:**
- IC1: Runtime pipeline — orchestrates the turn loop: receive messages → update beliefs → plan → generate messages → submit orders
- IC2: Evaluation — runs tournaments, computes Elo, tracks metrics over time, manages human evaluation sessions

This role is critically undervalued in most research teams. Without it, you get the "works in isolation" problem.

### 3.6 Game Theory Lead

| Owns | Does NOT Own |
|------|--------------|
| Equilibrium computation methods | Training infrastructure |
| Theoretical grounding of the approach | NLP pipeline |
| Formal analysis (convergence, regret bounds) | Day-to-day experiment running |
| Component design doc: "Planning & Equilibria" | |

**ICs:**
- IC1: Algorithm design and implementation for search/equilibrium finding
- IC2 (possibly shared with Strategy team): Bridging theory to practice at Diplomacy's scale

### 3.7 Domain Expert (Part-Time Consultant)

- Reviews games qualitatively every few weeks (2-3 days per engagement)
- Provides insight on whether the AI's play "makes sense" beyond win rates
- Helps design evaluation protocols
- Not full-time — part-time experts stay sharp; full-time ones get bored

---

## 4. Communication Channels

### 4.1 Synchronous (Meetings)

| Meeting | Who | Frequency | Purpose |
|---------|-----|-----------|---------|
| Full team standup | Everyone | 2x/week, 15 min | Quick blockers, announcements |
| Architecture sync | Tech Lead + all sub-leads | Weekly, 60 min | Interface issues, integration decisions, roadmap check |
| Component standups | Each sub-team internally | Daily or 3x/week, 10 min | Within-component coordination |
| Playtest review | Everyone | Weekly, 60 min | Watch the bot play, discuss qualitative issues |
| Research reading group | Anyone interested | Biweekly, 45 min | Stay current on relevant papers |
| 1:1s | EM ↔ each person | Biweekly, 30 min | Career, blockers, morale |
| Tech Lead ↔ Sub-leads | Tech Lead ↔ each lead | Weekly, 30 min | Deep-dive on component progress |

### 4.2 Asynchronous (Slack/Chat Channels)

```
#diplomacy-general        — announcements, broad discussion
#diplomacy-strategy       — RL, self-play, search (Strategy team)
#diplomacy-dialogue       — NLP, generation, filtering (Dialogue team)
#diplomacy-integration    — pipeline, runtime, bugs where components meet
#diplomacy-experiments    — "I'm launching run X with config Y, ETA Z"
#diplomacy-results        — automated posts: eval scores, game logs, metrics
#diplomacy-papers         — interesting papers, discussion
#diplomacy-infra          — GPU allocation, cluster issues, CI/CD
```

The most important channel is **#diplomacy-integration**. This is where cross-cutting bugs surface. Both sub-leads should monitor this channel.

---

## 5. Artifacts & Documentation

### 5.1 Design Documents

```
📁 Diplomacy Project
├── 📄 Master Architecture Doc          (Tech Lead owns, all leads contribute)
│   └── Component interfaces, data flow diagrams, key assumptions
├── 📄 Strategic Engine Design           (Strategy Lead owns)
├── 📄 Dialogue System Design            (Dialogue Lead owns)
├── 📄 Integration & Runtime Design      (Integration Lead owns)
├── 📄 Planning & Game Theory            (GT Lead owns)
├── 📄 Evaluation Protocol               (Integration Lead owns)
├── 📄 Data Documentation                (what training data, how collected, licenses)
└── 📁 Experiment Logs
    └── 📄 One doc per major experiment with config, results, conclusions
```

### 5.2 Jira Boards

**Board 1: Project-level (EM owns)**
- Epics = milestones: "No-Press baseline", "Intent-conditioned dialogue", "End-to-end integration", "Live human evaluation"
- Tracks cross-team dependencies
- This is what leadership outside the team looks at

**Boards 2-5: Component-level (each sub-lead owns their own)**
- Strategy board, Dialogue board, Integration board, Game Theory board
- Individual tasks, bugs, experiments
- Sub-leads groom their own boards
- ICs pick up work from their component board

The EM syncs Board 1 with Boards 2-5. If an epic is blocked, the EM traces it to which component board has the blocking ticket.

### 5.3 Code Repository (Monorepo)

```
diplomacy/
├── strategy/          ← Strategy Lead is code owner
│   ├── self_play/
│   ├── search/
│   ├── human_model/
│   └── tests/
├── dialogue/          ← Dialogue Lead is code owner
│   ├── model/
│   ├── generation/
│   ├── filters/
│   └── tests/
├── game_theory/       ← GT Lead is code owner
├── integration/       ← Integration Lead is code owner
│   ├── runtime/       (the main game-playing loop)
│   ├── pipeline/      (strategy ↔ dialogue glue)
│   └── tests/
├── eval/              ← Integration Lead is code owner
│   ├── metrics/
│   ├── tournaments/
│   └── analysis/
├── infra/             ← EM or dedicated infra IC
│   ├── training/
│   ├── cluster/
│   └── ci/
├── data/              ← Shared, Dialogue Lead primary owner
└── CODEOWNERS         ← enforced via GitHub/internal tooling
```

### 5.4 Code Review Policy

- **Within-component PRs:** reviewed by someone on the same sub-team
- **Cross-component PRs** (touching 2+ directories): reviewed by BOTH component leads
- **Interface changes** (intent format, message schema): reviewed by Tech Lead

---

## 6. Roadmap Ownership

The roadmap is shared but not equal:

| Aspect | Owner |
|--------|-------|
| Research roadmap (what scientific questions, in what order, when to pivot) | Tech Lead |
| Execution roadmap (timelines, resource allocation, risk mitigation) | Engineering Manager |
| Component roadmaps ("to hit milestone X, my team does A, B, C") | Sub-leads |

**Example negotiation:** The Tech Lead says "we need intent-conditioned dialogue working by April." The Dialogue Lead says "to do that, we need the intent schema finalized by mid-March, and 2 weeks of training after that." The EM says "we only have 64 GPUs allocated until May, so we need to stagger the strategy and dialogue training runs." They negotiate.

The roadmap lives as a single doc (Tech Lead maintains) with a corresponding Gantt-style view in Jira (EM maintains). Same information, two representations for two audiences.

---

## 7. Key Organizational Principles

1. **Integration is its own role, not an afterthought.** The hardest part of CICERO wasn't any single component — it was making the strategic model and dialogue model work together in a real-time loop. A dedicated person (or pair) whose entire job is the pipeline between components prevents the classic failure mode where everyone assumes "someone else" will glue it together.

2. **Weekly full-stack playtesting.** Every week, the whole team watches the current system play a live game. Nothing reveals integration bugs faster than watching your bot say "I'll support you into Munich" and then attack Munich.

3. **Shared evaluation infrastructure from day one.** Everyone logs to the same system, everyone can see every game the bot plays, every message it generates, every intent it forms. This prevents the failure mode where the RL team says "our model is great" and the NLP team says "our model is great" but the combined system is broken.

4. **Human regularization as a design principle.** Pure RL self-play produces alien strategies incompatible with human cooperation. The key insight from CICERO was blending RL with imitation of human play (piKL). This is an architectural decision, not a tuning knob — it must be decided early and owned by the Tech Lead.

5. **Milestone-driven, not sprint-driven.** Research doesn't fit into 2-week sprints. Use milestones instead:
   - Milestone 1: Beat average human at No-Press Diplomacy
   - Milestone 2: Generate coherent messages given an intent
   - Milestone 3: Play a full game with dialogue without being detected as a bot
   - Milestone 4: Achieve human-level scores in blind online play

6. **The Tech Lead manages interfaces, not details.** The manageable unit of context is the contract between components, not the internals of each component. Sub-leads own depth; the Tech Lead owns breadth.

---

## References

- Meta FAIR Diplomacy Team. "Human-level play in the game of Diplomacy by combining language models with strategic reasoning." *Science*, Nov 2022. https://www.science.org/doi/10.1126/science.ade9097
- Bakhtin et al. "Mastering the Game of No-Press Diplomacy via Human-Regularized Reinforcement Learning and Planning." 2022. https://arxiv.org/abs/2210.05492
- CICERO source code: https://github.com/facebookresearch/diplomacy_cicero
