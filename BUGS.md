# BUGS.md — Inferred ZeroClaw Runtime Issues

Observation-based bug register derived from external agent behavior analysis of the live `jasmin-zeroclaw-v2` WhatsApp agent.

Status legend: **Unconfirmed** (plausible but not verified), **Observed** (seen in chat/logs), **Partially confirmed** (live pod/source supports the hypothesis), **Confirmed** (reproduced or directly present in code/logs).

## Live pod evidence snapshot

Read-only checks on `jasmin-zeroclaw-v2-7cd49ff98f-vbbfd` on 2026-07-02:

- Pod image: `ghcr.io/theblazehen/zeroclaw:debian`
- Mounts: `/zeroclaw-data`, `/nix`, serviceaccount
- Current config is symlinked from `/zeroclaw-data/.zeroclaw/config.toml` to `/zeroclaw-data/config.toml`
- There are still duplicate data trees:
  - `/zeroclaw-data/data/{control_plane.db,cron/jobs.db,memory/brain.db,sessions/*.db}`
  - `/zeroclaw-data/.zeroclaw/data/{control_plane.db,cron/jobs.db,memory/brain.db,sessions/*.db}`
- Mail state is separate again:
  - `/zeroclaw-data/agents/main/workspace/mail-triage/state.json`
  - `/zeroclaw-data/agents/main/workspace/todos.json`
  - `/zeroclaw-data/agents/main/workspace/mail-triage/latest-report.md`
- Config currently has loop detection disabled globally:
  - `loop_detection_enabled = false`
  - `loop_detection_max_repeats = 999999`
- Config has a giant runtime limit only under `[runtime_profiles.cron]`:
  - `max_tool_iterations = 999999`
- `agents.mail_triage` uses `runtime_profile = "cron"`, but `agents.main` has **no** `runtime_profile`, so live WhatsApp turns fall back to the hardcoded default of 10 tool iterations.

## Highest priority inferred bugs

### BUG-001 — Apparent context loss is not session persistence loss; likely history-window / recall limitation

**Status:** Investigated; original duplicate-session-root hypothesis downgraded

**Symptom:** Agent re-asks things it should already know, loses current task state, and needs repeated re-steering after restarts or session boundaries.

**Confirmed evidence:** the active session DB is being used and contains a continuous WhatsApp history:
- DB: `/zeroclaw-data/data/sessions/sessions.db`
- `session_metadata`: 1 row for `whatsapp.main`
- `sessions`: 879 messages, contiguous IDs 856–1734, spanning 2026-06-27 → 2026-07-02
- stale DB `/zeroclaw-data/.zeroclaw/data/sessions/sessions.db` has 0 messages

So this is **not currently** caused by reading the wrong session DB.

**Likely source now:** the runtime persists the history, but only a bounded subset is fed back to the model. `agents.main` has no runtime profile, so it inherits defaults like `max_history_messages = 50` (`schema.rs:3730-3734`) and `max_context_tokens = 32000` (`schema.rs:3736-3740`). Long-running tool-heavy turns can push relevant user corrections out of the model-visible window even though they remain in SQLite.

**Source map:**
- `crates/zeroclaw-config/src/schema.rs:3730-3740` — default history/context limits when no runtime profile is set.
- `crates/zeroclaw-config/src/schema.rs:3816-3843` — resolved agent config bakes those defaults.
- `crates/zeroclaw-runtime/src/agent/loop_.rs` / `turn/` — prompt construction from persisted session history.
- Hindsight is active for long-term memory, but session continuity is separate from memory recall.

**Fix direction:** give `agents.main` an explicit runtime profile with larger history/context and verify prompt construction includes recent steering messages before older tool transcript.

---

### BUG-002 — Duplicate / double replies

**Status:** Investigated; no current evidence in session DB

**Symptom:** Agent sometimes sends the same response twice, or sends a response to an older turn after a newer user message.

**Confirmed evidence:** active session DB has 0 duplicate assistant messages (exact content match), and the runtime trace has no `duplicate` or `dedup` events. Chat export text matching found two exact duplicate Pixel messages within 10 minutes:
- 2026-06-26 12:32 → 12:35: same circuit-breaker error text
- 2026-06-27 13:42 → 13:45: `I couldn't produce a visible reply for that message. Please try again.`

**Status:** Downgraded to **narrow confirmed symptom, root cause unconfirmed**. The duplicates are error/fallback messages, not normal assistant responses. Original broader symptom may also be caused by BUG-003 (stale-turn responses arriving after the conversation moved on) rather than true duplicate delivery. When `interrupt_on_new_message = false`, the agent can finish an old turn and deliver its result after the user has sent newer messages, which looks like a duplicate or stale reply.

**Source map (if it resurfaces):**
- `crates/zeroclaw-channels/src/orchestrator/mod.rs:5520-5573` — in-flight task registration and interrupt logic
- `crates/zeroclaw-channels/src/whatsapp_web.rs` — WhatsApp outbound delivery path
- `crates/zeroclaw-runtime/src/daemon/mod.rs:757` — cron announce delivery path

**What to verify if it resurfaces:** capture the WhatsApp message IDs for both copies and check whether they have the same trace_id (true duplicate) or different trace_ids (stale-turn delivery).

---

### BUG-003 — Stale-turn execution / steering lag because WhatsApp interrupt steering is disabled

**Status:** Confirmed configuration issue

**Symptom:** Agent keeps working on an earlier request after the user has corrected course. In OpenClaw terms: steering does not interrupt or supersede current work.

**Root cause:** interrupt steering exists for channel messages, including WhatsApp, but the live config has it disabled:

```toml
[channels.whatsapp.main]
interrupt_on_new_message = false
```

**Source map:**
- `crates/zeroclaw-channels/src/orchestrator/mod.rs:5525-5556` — when enabled, a new message from the same sender cancels the previous in-flight task and waits for it to finish.
- `crates/zeroclaw-channels/src/orchestrator/mod.rs:5560` — cancellation token is passed into `process_channel_message`.
- `crates/zeroclaw-channels/src/orchestrator/mod.rs:15391` — test coverage for interrupting in-flight WhatsApp requests while preserving context.
- `crates/zeroclaw-config/src/schema.rs:13029-13032` — WhatsApp channel field `interrupt_on_new_message`.

**Fix direction:** set `interrupt_on_new_message = true` for `channels.whatsapp.main`. This is good for interactive chat because user corrections should supersede stale work. Trade-off: a casual “?” or “ping” can abort useful long-running work, so background jobs should run as cron/subagents rather than the live chat turn.

---

### BUG-004 — Live WhatsApp agent still has default 10 tool iterations

**Status:** Confirmed

**Symptom:** Tool-limit-like behavior persists in the live chat even after setting `[runtime_profiles.cron].max_tool_iterations = 999999`.

**Root cause:** there is no single unlimited runtime profile applied to every agent. The current config only assigns `runtime_profile = "cron"` to `agents.mail_triage`; `agents.main` has no `runtime_profile`, so `effective_max_tool_iterations("main")` falls back to 10.

**Live evidence:**

```toml
[runtime_profiles.cron]
max_tool_iterations = 999999

[agents.main]
model_provider = "custom.llmapi_glm"
risk_profile = "main"
# no runtime_profile here → default 10

[agents.mail_triage]
model_provider = "custom.llmapi_glm"
risk_profile = "cron"
runtime_profile = "cron"
```

Runtime trace directly confirms live WhatsApp turns hit max 10:
- `tool_loop_exhausted`, `max_iterations=10`, `agent_alias="main"`, `channel="whatsapp.main"`
- Seen for message IDs `bb26970d-...`, `be3fb117-...`, and `73120561-...` on 2026-07-02.
- Follow-up event: `Max iterations reached, requesting final summary`.

`crates/zeroclaw-config/src/schema.rs:3722-3727`:

```rust
pub fn effective_max_tool_iterations(&self, agent_alias: &str) -> usize {
    self.runtime_profile_for_agent(agent_alias)
        .map(|p| p.max_tool_iterations)
        .filter(|&v| v > 0)
        .unwrap_or(10)
}
```

**Source map:**
- `crates/zeroclaw-config/src/schema.rs:3673-3686` — agent → runtime profile lookup.
- `crates/zeroclaw-config/src/schema.rs:3722-3727` — fallback to hardcoded 10.
- `crates/zeroclaw-config/src/schema.rs:3816-3843` — resolved agent config bakes runtime profile values.
- `crates/zeroclaw-runtime/src/agent/turn/max_iter.rs` — max-iteration enforcement path.

**Fix direction:** create one runtime profile (e.g. `unlimited`) and assign it to `agents.main` and `agents.mail_triage`, or set `runtime_profile = "cron"` on `agents.main` too. Better name it something non-cron if it applies globally.

---

### BUG-005 — Partial completion reported after max-iteration exhaustion

**Status:** Confirmed pattern

**Symptom:** Agent says “done” or gives a confident partial summary after only attempting an action, then later admits a tool limit or command failure prevented completion.

**Confirmed evidence:** runtime trace shows live WhatsApp turns hitting `tool_loop_exhausted` at `max_iterations=10`, then `Max iterations reached, requesting final summary`. That final-summary step asks the model to summarize partial work instead of continuing, which can sound like completion if the prompt/output is not explicit enough.

**Source map:**
- `crates/zeroclaw-runtime/src/agent/turn/max_iter.rs:34` — logs `tool_loop_exhausted` with `max_iterations=10`.
- `crates/zeroclaw-runtime/src/agent/turn/max_iter.rs:54` — requests final summary after max iterations.
- `crates/zeroclaw-runtime/src/agent/turn/results_collect.rs:124-126` — tool output is canonicalized/truncated before history append.
- Shell/tool implementations under `crates/zeroclaw-tools/src/` should be checked for structured exit status propagation.

**Fix direction:** first fix BUG-004. Then ensure final-summary wording says “I stopped because max iterations were reached; this is partial” and require verify-after-mutate for label/file/state actions.

---

### BUG-006 — State drift across chat, cron, report files, Gmail labels

**Status:** Confirmed by live logs/state layout

**Symptom:** Briefing, `latest-report.md`, `state.json`, `todos.json`, and Gmail labels disagree.

**Live evidence:** pod logs show the same item transitioning between done/pending across runs; live state is split across separate files plus Gmail labels. Example: Naked selfie remains in morning briefing after being handled/confirmed, then later mail triage resolves it from a new email.

**Source map:**
- Mail workflow state files are outside core config: `/zeroclaw-data/agents/main/workspace/mail-triage/state.json`, `todos.json`, `latest-report.md`
- `crates/zeroclaw-runtime/src/cron/scheduler.rs` — cron jobs run independently and mutate external state through tools/prompts
- `crates/zeroclaw-runtime/src/cron/types.rs` — no shared application-level state model for mail/todos/labels

**Architectural note:** this violates this repo’s single-source-of-truth rule. The durable source should be one state store, with Gmail labels/report/briefing as derived views.

---

### BUG-007 — Cron agents are not equivalent to the live agent

**Status:** Confirmed

**Symptom:** Cron-launched triage behaves unlike the interactive WhatsApp agent: different context, runtime profile, state, and delivery semantics.

**Live evidence:**
- `mail_triage` cron uses `session_target = "isolated"`, `delivery.mode = "none"`, `agent_alias = "mail_triage"`, and `uses_memory = 1`.
- `morning_briefing` cron uses `session_target = "main"`, `delivery.mode = "announce"`, but active cron DB still stores `agent_alias = "mail_triage"`.
- Config prompts explicitly point at workflow instruction files rather than relying on the live agent's context:
  - `[cron.mail_triage].prompt = "Read /zeroclaw-data/agents/main/workspace/mail-triage/INSTRUCTIONS.md first..."`
  - `[cron.morning_briefing].prompt = "Read /zeroclaw-data/agents/main/workspace/mail-triage/MORNING_BRIEFING.md first..."`
- Runtime-profile split is real: `agents.mail_triage` uses `runtime_profile = "cron"`; `agents.main` has no runtime profile.

**Source map:**
- `crates/zeroclaw-runtime/src/cron/scheduler.rs` — builds and runs cron jobs
- `crates/zeroclaw-runtime/src/tools/cron_add.rs` — cron job prompt/model/delivery schema
- `crates/zeroclaw-runtime/src/cron/types.rs:162` — per-job optional tool allowlist
- `crates/zeroclaw-runtime/src/subagent/mod.rs` — context inheritance model for subagents, compare with cron path

---

### BUG-008 — Memory injection issues are not caused by SQLite `brain.db`; Hindsight is active

**Status:** Investigated; original duplicate-`brain.db` hypothesis downgraded

**Symptom:** Agent sometimes acts as if long-term notes/instructions don't exist.

**Confirmed evidence:** memory is configured for Hindsight and the service is healthy:

```toml
[memory]
backend = "hindsight.default"

[storage.hindsight.default]
url = "http://hindsight.clawd.svc.cluster.local:8888"
tenant = "default"
bank_id = "zeroclaw"
synchronous_retain = true
```

Health check returned `{"status":"healthy","database":"connected"}`. Runtime trace contains `Memory initialized` with `backend="hindsight"` and `memory_namespace="hindsight.default"`.

The SQLite files are legacy/stale:
- `/zeroclaw-data/data/memory/brain.db` mtime 2026-06-27 14:31, no WAL/SHM
- `/zeroclaw-data/.zeroclaw/data/memory/brain.db` mtime 2026-06-26 10:46

**Likely source now:** if memory appears missing, investigate Hindsight recall query relevance / prompt injection, not duplicate SQLite DBs. The runtime can initialize Hindsight, but recall may still return no relevant memories or may not be included in the visible prompt window.

**Source map:**
- `crates/zeroclaw-memory/src/lib.rs:509-517` — Hindsight backend selection from `memory.backend`.
- `crates/zeroclaw-memory/src/hindsight.rs` — Hindsight recall/list/retain API client.
- `crates/zeroclaw-runtime/src/agent/loop_.rs` — memory initialization and injection into turns.

**Fix direction:** add trace logging for each live turn: memory backend, recall query, number of recalled items, and whether recalled items were inserted into the prompt.

---

### BUG-009 — Event ordering race: stale briefing before reconciliation

**Status:** Confirmed by logs

**Symptom:** Morning briefing lists already-handled items because triage/reconciliation has not run yet or has not propagated state.

**Live evidence:** logs show 09:00 briefing still listing BYD selfie, then 11:00 triage resolves it after detecting an approval email.

**Source map:**
- `crates/zeroclaw-runtime/src/cron/scheduler.rs` — cron jobs run independently
- `crates/zeroclaw-runtime/src/cron/types.rs` — schedule type lacks dependencies (`depends_on`, `after`, prerequisite job result)
- `crates/zeroclaw-runtime/src/cron/store.rs` — persistence of jobs, no dependency graph

**Fix direction:** briefing should depend on successful same-cycle reconciliation, or briefing should run reconciliation as an internal first step.

---

### BUG-010 — WhatsApp media ingestion broken or lossy

**Status:** Observed in chat, not confirmed in current runtime trace

**Symptom:** WhatsApp shows `<Media omitted>` and the agent does not receive a stable visible file path for sent media.

**Current evidence:** runtime trace currently has no `whatsapp-web` or `media marker` failures. This needs a fresh media-send repro with trace/log capture. The chat export still shows the user experienced this, but the current trace window does not confirm the failure mode.

**Source map:**
- `crates/zeroclaw-channels/src/whatsapp_web.rs:1291` — media type mapping.
- `crates/zeroclaw-channels/src/whatsapp_web.rs:1697` — “whatsapp-web: media marker delivery failed” log site.
- `crates/zeroclaw-channels/src/orchestrator/` — channel media routing into agent-visible message content.

**What to verify next:** send one image/file to WhatsApp, then inspect runtime trace and workspace files for the inbound message ID.

---

### BUG-011 — Backfill/search freshness issue

**Status:** Observed; likely workflow/tooling issue, not core runtime

**Symptom:** Agent claims a full scan but misses latest messages/emails.

**Confirmed current evidence:** `gog-wrap.sh` is a thin wrapper only; pagination/window behavior lives in external `gog` plus prompt instructions. `INSTRUCTIONS.md` uses broad queries like `newer_than:1d --max 50` and says to use `last_check`, but the wrapper itself does not enforce pagination or timestamp filtering. `state.json` stores `last_check` per account and is manually edited by the agent.

**Source map:**
- `/zeroclaw-data/agents/main/workspace/mail-triage/gog-wrap.sh` — only sets env and execs `gog`.
- `/zeroclaw-data/agents/main/workspace/mail-triage/INSTRUCTIONS.md` — search strategy (`newer_than:1d`, `--max 50`, `last_check`).
- `/zeroclaw-data/agents/main/workspace/mail-triage/state.json` — mutable `last_check` windows.

**Fix direction:** move pagination/windowing into deterministic code instead of instructions: repeatedly follow `nextPageToken`, filter by message internalDate > last_check, then atomically advance `last_check` only after successful processing.

---

### BUG-012 — Over-trusting tool success without verifying postconditions

**Status:** Observed

**Symptom:** Agent believes an action succeeded when only a command was attempted.

**Source map:**
- `crates/zeroclaw-runtime/src/agent/turn/results_collect.rs` — tool outputs become model-visible text; no generic postcondition system
- `crates/zeroclaw-tools/src/` — tool-specific result schemas / exit status handling
- Mail workflow scripts — should verify labels/state after mutation

**Fix direction:** provide structured tool results and require explicit verification for mutating tools.

---

### BUG-013 — No durable active-task ledger

**Status:** Observed

**Symptom:** User repeatedly asks “back?”, “now?”, “go?”, because there is no external signal of current active task, stalled task, or background progress.

**Source map:**
- `crates/zeroclaw-runtime/src/heartbeat/engine.rs` — internal heartbeat metrics
- `crates/zeroclaw-api/src/observability_traits.rs` — Observer interface
- `crates/zeroclaw-runtime/src/daemon/mod.rs` — where status could be exposed or sent to channel
- `crates/zeroclaw-infra/` — stall watchdog / debounce infrastructure

**Fix direction:** durable task ledger with `active/waiting/failed/done` and optional channel-visible progress pings.

---

### BUG-014 — Instruction drift / prompt layering confusion

**Status:** Partially confirmed

**Symptom:** Rules added in one place do not reliably apply to cron, future sessions, or subagents.

**Live evidence:** cron prompts manually tell the agent to read instruction files. That means instruction application is workflow-specific, not guaranteed by runtime.

**Source map:**
- `crates/zeroclaw-runtime/src/cron/scheduler.rs` — cron prompt assembly
- `crates/zeroclaw-runtime/src/subagent/mod.rs` — context inheritance
- `crates/zeroclaw-runtime/src/agent/turn/` — system prompt assembly/truncation
- `crates/zeroclaw-runtime/src/tools/cron_add.rs` — cron prompt storage/update

---

### BUG-015 — Provider/model routing mismatch

**Status:** Investigated; no current mismatch found

**Symptom:** Effective model differs from configured model alias.

**Confirmed current evidence:** runtime trace previously showed all provider calls using `model="alias/glm"`. Live config has now been updated to `[providers.models.custom.llmapi_glm].model = "alias/roulette-glm"` and rollout confirmed. No current evidence of direct `opencode-go` routing or wrong model use.

**Likely historical cause:** same as BUG-019 / BUG-004 — edits were being made to a config/profile not used by the live agent, making routing appear stale.

**Source map if it resurfaces:**
- `crates/zeroclaw-config/src/schema.rs:3845-3882` — agent model provider resolution.
- `crates/zeroclaw-runtime/src/agent/turn/provider_call.rs` — actual provider/model passed to LLM call.
- `crates/zeroclaw-providers/` — provider dispatch/caching.

**Fix direction:** keep startup/turn logging of `agent_alias`, `model_provider`, and concrete `model`.

---

### BUG-016 — Loop detector blocks legitimate polling patterns

**Status:** Confirmed in source, currently disabled in live config

**Symptom:** repeated legitimate polling/checking trips a circuit breaker.

**Source map:**
- `crates/zeroclaw-runtime/src/agent/loop_detector.rs:164-203` counts exact same tool+args consecutive calls.
- `crates/zeroclaw-runtime/src/agent/turn/results_collect.rs:100-116` hard-aborts on `Break`.
- `crates/zeroclaw-config/src/schema.rs:5391-5404` exposes only global loop detection knobs.

**Current live config:** `loop_detection_enabled = false`, `loop_ignore_tools` includes `cron_list`, so any present issue is not this exact detector unless another config root is used.

---

### BUG-017 — Silent failure / liveness opacity

**Status:** Partially confirmed

**Symptom:** Agent hangs or disappears without saying whether it is running, blocked, rate-limited, or dead.

**Confirmed evidence:** max-iteration exhaustion is logged internally (`tool_loop_exhausted`, `Max iterations reached, requesting final summary`), but from the user's side the only signal is a delayed partial reply. The internal WARN does not surface to WhatsApp.

**Source map:**
- `crates/zeroclaw-runtime/src/agent/turn/max_iter.rs:34-54` — logs exhaustion and requests final summary; user only sees the final summary text.
- `crates/zeroclaw-runtime/src/heartbeat/engine.rs` — internal liveness counters
- `crates/zeroclaw-runtime/src/daemon/mod.rs` — daemon supervision/logging
- `crates/zeroclaw-runtime/src/agent/turn/mod.rs` — turn timeout/cancel behavior
- `crates/zeroclaw-infra/` — stall watchdog

**Fix direction:** surface long-running state and failures to the originating channel, not just logs. For max-iter exhaustion, include a visible marker like "(stopped: iteration limit)" in the final summary.

---

### BUG-018 — Pod restart wipes non-persistent installs/state

**Status:** Confirmed infra behavior, probably not core runtime

**Symptom:** `apt-get` installs vanish after pod restart; nix persists due mounted `/nix`.

**Live evidence:** pod mounts `/nix` and `/zeroclaw-data`; normal root filesystem is container image.

**Source map:** deployment/Kubernetes manifests, not obvious Rust source.

**Related runtime issue:** if `workspace_dir` or home path points outside PVC, runtime-created files will also vanish.

---

### BUG-019 — Duplicate durable roots under `/zeroclaw-data/data` and `/zeroclaw-data/.zeroclaw/data`

**Status:** Confirmed, root cause identified

**Symptom:** Durable DB state exists in two parallel roots, causing context loss, memory drift, cron drift, session restore inconsistencies, and config edits appearing ineffective.

#### What's actually happening

The pod env is `HOME=/zeroclaw-data` + `ZEROCLAW_DATA_DIR=/zeroclaw-data`.
Config resolution (`schema.rs:16050-16069`) hits the `ZEROCLAW_DATA_DIR` branch and calls
`resolve_config_dir_for_data("/zeroclaw-data")` (`schema.rs:15828`).
Because `/zeroclaw-data/config.toml` exists, it returns:
- **config_dir = `/zeroclaw-data`**
- **data_dir = `/zeroclaw-data/data`**

So the **current daemon** uses:
| What | Path |
|------|------|
| config.toml | `/zeroclaw-data/config.toml` (symlinked from `.zeroclaw/config.toml`) |
| install_root (`schema.rs:4062`) | `/zeroclaw-data` (parent of config_path) |
| agent_workspace (`schema.rs:3973`) | `/zeroclaw-data/agents/<alias>/workspace` |
| cron DB (`cron/store.rs:1251`) | `/zeroclaw-data/data/cron/jobs.db` |
| session DBs | `/zeroclaw-data/data/sessions/*.db` |
| control_plane | `/zeroclaw-data/data/control_plane.db` |

The **stale root** `/zeroclaw-data/.zeroclaw/data/*` was created when `HOME=/zeroclaw-data`
caused `default_config_dir()` (`schema.rs:15755-15766`) to resolve to `$HOME/.zeroclaw`
= `/zeroclaw-data/.zeroclaw` — this happens when `ZEROCLAW_DATA_DIR` is **not set** (older
pod config, CLI invocations without env, or any code path that calls `default_config_dir()`
directly instead of going through `resolve_runtime_config_dirs()`).

#### Stat evidence (2026-07-02 12:00)

| DB | Active root mtime | Stale root mtime |
|----|-------------------|------------------|
| cron/jobs.db | 2026-07-02 12:00 | 2026-06-26 10:38 |
| sessions/sessions.db | 2026-07-02 11:51 | 2026-06-26 10:15 |
| memory/brain.db | 2026-06-27 14:31 | 2026-06-26 10:46 |
| control_plane.db | 2026-06-26 10:13 | 2026-06-26 10:16 |

Active root is being written; stale root is dead since Jun 26–27.
**But stale DBs still sit on disk and can confuse any code path that falls back to
`default_config_dir()` instead of using `config.data_dir`.**

#### Memory: Hindsight (not SQLite)

Config has `memory.backend = "hindsight.default"` with
`[storage.hindsight.default]` pointing at `http://hindsight.clawd.svc.cluster.local:8888`.
So **memory recall/store goes to the Hindsight service**, not `brain.db`.
The `brain.db` files are legacy SQLite from before the Hindsight switch and are no
longer the memory source of truth — but they still exist in both roots, which means
any code path that defaults to `brain.db` instead of respecting the configured backend
will read stale data. See `crates/zeroclaw-memory/src/lib.rs:509-517` for backend selection.

#### Why config edits "didn't work" historically

Before the symlink fix on Jun 27, `.zeroclaw/config.toml` (what `default_config_dir()`
would find) and `/zeroclaw-data/config.toml` (what `ZEROCLAW_DATA_DIR` resolves to) were
**separate files**. Edits to one didn't affect the other. The daemon read
`/zeroclaw-data/config.toml`; the CLI or any tool using `default_config_dir()` read
`/zeroclaw-data/.zeroclaw/config.toml`. Now symlinked, but the pattern of dual
resolution paths remains a landmine.

#### Source map (definitive)

- `schema.rs:15755-15766` — `default_config_dir()` uses `$HOME/.zeroclaw` when no env
  var is set. With `HOME=/zeroclaw-data`, this returns `/zeroclaw-data/.zeroclaw`.
- `schema.rs:16050-16069` — `ZEROCLAW_DATA_DIR` branch calls
  `resolve_config_dir_for_data()` which returns `/zeroclaw-data` + `/zeroclaw-data/data`.
- `schema.rs:15828-15851` — `resolve_config_dir_for_data()` logic: if `config.toml`
  exists in the data dir, that becomes config_dir; otherwise checks parent `.zeroclaw`.
- `schema.rs:4062` — `install_root_dir()` = parent of `config_path`.
- `schema.rs:3973` — `agent_workspace_dir()` = `install_root/agents/<alias>/workspace`.
- `cron/store.rs:1251` — cron DB = `config.data_dir.join("cron/jobs.db")`.
- `crates/zeroclaw-memory/src/lib.rs:509-517` — memory backend selection respects config;
  but `brain.db` path is still `data_dir.join("memory/brain.db")` for legacy/markdown
  backends.
- `crates/zeroclaw-config/src/cost/tracker.rs:391-393` — cost tracker has its own
  `workspace_dir.join("state/costs.jsonl")` + legacy `workspace_dir.join(".zeroclaw/costs.db")`.

#### Fix direction

1. Delete stale `/zeroclaw-data/.zeroclaw/data/*` (after confirming no code reads it).
2. Audit every call to `default_config_dir()` and ensure it's only used for fallback
   when `ZEROCLAW_DATA_DIR` is absent — never when a live config is already loaded.
3. Add a startup log line emitting `config_path`, `data_dir`, `install_root`,
   `memory_backend`, and `agent_workspace_dir` so "Schrödinger's config" is impossible.

### BUG-020 — Multiple agents claim the same WhatsApp channel; ownership is ambiguous

**Status:** Confirmed

**Symptom:** Live/chat/cron attribution can become confusing: which agent owns `whatsapp.main`, which runtime profile applies, and which instructions/context apply are not obvious.

**Live evidence:** current config has **both** agents claiming the same channel:

```toml
[agents.main]
channels = ["whatsapp.main"]
cron_jobs = ["morning_briefing"]

[agents.mail_triage]
runtime_profile = "cron"
channels = ["whatsapp.main"]
```

The active session metadata has `agent_alias = NULL`, while runtime trace labels live WhatsApp turns as `agent_alias="main"`. The active cron DB also has `morning_briefing.agent_alias = "mail_triage"` even though config says `agents.main.cron_jobs = ["morning_briefing"]` and `agents.mail_triage` does not list it.

**Source map:**
- `crates/zeroclaw-config/src/schema.rs:3897-3904` — `channel_workspace_dir()` resolves the channel's owning agent via `agent_for_channel()`.
- `crates/zeroclaw-config/src/schema.rs:3890-3895` — `agent_for_channel()` returns the first enabled agent whose `channels` contains the channel. Backed by a map iteration, so duplicate ownership is not a stable contract.
- `crates/zeroclaw-config/src/schema.rs:3940-3954` — `agent_for_cron_job()` similarly picks first agent listing a cron alias.
- `crates/zeroclaw-channels/src/orchestrator/mod.rs` — router ownership depends on channel owner maps derived from config.

**Likely impact:** runtime-profile confusion (`main` gets default 10, `mail_triage` gets unlimited), instruction drift, workspace confusion, and cron attribution drift.

**Fix direction:** enforce uniqueness: a channel alias and declarative cron alias must be owned by exactly one enabled agent. Config validation should reject duplicates. Current live config should remove `channels = ["whatsapp.main"]` from `agents.mail_triage` unless mail triage is supposed to be the live WhatsApp agent.

## Suggested next verification steps

1. Inspect `gog-wrap.sh`/`gog` behavior for pagination, attachment ID handling, and timestamp windows.
2. Add startup/turn logging of effective `config_path`, `data_dir`, `install_root`, `agent_alias`, `runtime_profile`, `model_provider/model`, `memory_backend`, and channel owner.
3. Wire full reminder/scheduled-callback tools on top of `task_ledger`.
4. Replace deterministic extractive compaction with optional LLM summarization via `context_compression.summary_provider`.

---

## Decisions & applied fixes (2026-07-02)

### Applied to live pod

- Backed up `/zeroclaw-data/config.toml` → `config.toml.bak.before-bugfixes-20260702123640`.
- `[providers.models.custom.llmapi_glm].model` → `alias/roulette-glm`.
- `[channels.whatsapp.main].interrupt_on_new_message` → `true` (BUG-003 fix).
- `[agents.main].runtime_profile` → `"cron"` (BUG-004 fix: unlimited tool iterations now apply to live WhatsApp turns).
- `[agents.main].channels` → `["whatsapp.main"]` (sole owner; BUG-020 fix).
- Removed `channels` from `[agents.mail_triage]` so only `main` owns WhatsApp.
- Deployment restarted via `kubectl rollout restart deploy/jasmin-zeroclaw-v2`; new pod `jasmin-zeroclaw-v2-5679647cd4-qgjmk` confirmed.

### Design decisions

- **Compaction (BUG-001):** Implemented deterministic extractive compaction. Old whole turns are still removed from the verbatim tail, but their first user line and first assistant reply are injected as a bounded `[Prior conversation summary (compacted)]` block after the trim breadcrumb. `context_compression.summary_provider` config still is not wired for LLM summarization.
- **Task/liveness ledger (BUG-013):** Implemented minimal durable SQLite ledger at `data_dir/tasks/tasks.db` with `tasks` and `task_events`; channel turns now mark `in_progress` then `completed`. Full scheduled callbacks/reminder tools are still a follow-up.
- **SOUL.md / @filename include (BUG-014):** `SOUL.md`, `AGENTS.md`, `TOOLS.md`, `IDENTITY.md`, `USER.md`, `BOOTSTRAP.md`, `MEMORY.md` are **auto-injected** into the system prompt from the agent workspace at `system_prompt.rs:25-42`. The agent does NOT need to `file_read` them. There is **no** `@filename.md` include syntax in the system prompt — workspace files are loaded by fixed filename list, not by `@` references.
- **Nix persistence (BUG-018):** AGENTS.md updated with "Dependency and System Library Policy" section instructing all agents to prefer Nix over `apt-get`.
- **Cron delivery.mode = "agent" (BUG-007/020):** Implemented. `delivery.mode = "agent"` with `delivery.to = "<agent_alias>"` routes cron output into the target agent's session via `agent::run()`. The agent processes it and decides whether/how to relay to the user. Code changes: `cron/mod.rs` (validation), `cron/scheduler.rs` (`deliver_to_agent`), tool schemas in `cron_add.rs`/`cron_update.rs`, config schema doc in `schema.rs`.

### Research: SOTA context compaction

Survey of production agent frameworks (LangChain, Cline, Claude Code, MemGPT, etc.) as of 2025-2026:

1. **Rolling summary + recent verbatim tail** — Production standard (LangChain `SummarizationMiddleware`, Cline Auto Compact, Claude Code). Summarize old turns into a running summary; keep last N messages verbatim. Trigger at ~70-85% of context window.
2. **Hierarchical summaries** — Summary-of-summaries tree. MemGPT/Letta. Overkill for most sessions; useful for multi-day.
3. **Episodic memory extraction** — Extract durable facts/decisions to a separate store, retrieve on demand. Mem0, LangGraph Store, ChatGPT Memory, Cline Memory Bank. Production-tested.
4. **Tool-result distillation** — Compress verbose tool outputs. Cline/Claude Code truncate+summarize; subagent delegation is structural distillation. Production-standard for search tools.
5. **Pinning critical instructions** — System prompt + task state pinned at top, never evicted. Universal in production.
6. **Retrieval over archived transcript** — Vector search old turns, inject top-k. LangGraph Store, Letta, Zep, Mem0. Production-tested.
7. **Token-budget-aware proactive compaction** — Compact before hitting hard limit. LangChain configurable triggers; Cline/Claude Code proactive. Production-standard.
8. **Subagent context offloading** — Child agent does heavy work, returns distilled report. Cline Subagents, LangGraph subgraphs. Emerging production pattern.

Dominant hybrid: rolling-summary + verbatim tail + pinned state + proactive triggers + tool-result distillation. Episodic/retrieval added for multi-session.

**ZeroClaw implementation direction:** keep system + pinned facts + active task ledger + recent N turns verbatim. Summarize older whole turns into durable session summary. Preserve tool-call/tool-result integrity. Never silently drop; emit visible/internal breadcrumb.

### Research: SOTA task/liveness/reminder ledger

Key patterns from OpenAI Assistants, LangGraph, Temporal, CrewAI, Claude Code, AutoGPT:

- **Durable task ledger** outside chat context. Structured store with: task_id, status, owner, timestamps, heartbeat, dependencies, result_ref, retry policy. SQL or event-sourced.
- **Two liveness signals:** heartbeat (process alive) + progress (task advancing). A live agent can be stalled.
- **Durable scheduled callbacks** (reminders): first-class DB table + poller, not `sleep()` inside agent loop.
- **Task handoff:** child writes terminal status to ledger + emits completion event; parent resumes via scheduler or message consumer. Recoverable by scanning ledger if notification lost.
- **Chat-visible status commands:** "what are you working on?", "what's pending?", "what failed?".
- **Minimal schema:** tasks(task_id, parent_task_id, owner_agent_id, status, created_at, started_at, heartbeat_at, last_progress_at, result_summary, error, retry_count, ...), task_dependencies, scheduled_callbacks, task_events.

**ZeroClaw implementation direction:** add `active_tasks` store in `data_dir`, per-agent. Agent loop writes task_id/status/heartbeat. Cron or channel-visible status command reads it. Scope per-agent, per-channel session.

### Stale data cleanup

`/zeroclaw-data/.zeroclaw/data/` was stale (mtimes Jun 26, no active writes). Moved aside on 2026-07-02 to `/zeroclaw-data/.zeroclaw/data.stale-20260702`. Active data remains in `/zeroclaw-data/data/`.