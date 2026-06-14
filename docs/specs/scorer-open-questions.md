# Smart-Routing Scorer: Open Questions for the Director

> Companion to `docs/specs/smart-routing-scorer-spec.md`. Everything in the spec that the
> architect could resolve from code, convention, or the house rules **has been resolved
> there** (normalization formulas, determinism proof, model-gate default, prompt-size
> wiring, config validation, Phase-3 store recommendation). This document isolates the
> residue: decisions that are **product / policy calls a human must make** — the spec
> picks a sane default for each so the sprint is not blocked, but each default is
> overturnable and is called out as such.
>
> Format per item: **question · why it needs a human · options w/ trade-offs ·
> recommendation (the spec's current default) · what flips if overturned**.

---

## Q1 — Phase-3 history persistence: in-memory vs SQLite (THE reshape decision)

**Question.** Should the Phase-3 `RoutingStats` (per-(backend,model) EWMA latency, rolling
error rate, flap counter) survive a router restart, or is volatile in-memory acceptable?

**Why it needs a human.** This is not a correctness question — both work — it is a product
expectation: *does a node's measured reputation persist across a Herd restart?* That is a
judgment about how operators expect the system to behave, and it is the one Phase-3 choice
that materially changes scope.

**Options.**
- **(c1) In-memory only — RECOMMENDED.** New `RoutingStats` struct, updated on the existing
  post-request hook, request-count EWMA decay, neutral-`0.5` cold start. Zero persistence,
  zero migration, no per-request write amplification, off the hot path. *Cost:* after a
  restart, dims 14–17 read neutral for a few minutes until traffic re-warms them — routing
  falls back to Phase-1 placement signals in the interim. No data loss of consequence (it's
  derived telemetry, not state of record).
- **(c2) SQLite `routing_history` table.** The node registry already runs `rusqlite` +
  migrations, so it's infrastructurally cheap. History survives restart; supports a longer
  window than memory. *Cost:* a migration, a per-request upsert (write amplification), a
  startup hydration step, and a staleness-eviction policy — a real Phase-3 scope increase.
  Even with c2, the scorer reads the in-memory snapshot; SQLite is only the durability layer
  behind it.

**Recommendation (spec default): c1, in-memory.** Ship Phase-3 v1 without persistence; c2 is
purely additive on top of c1's struct if you later want cross-restart memory.

**What flips if overturned.** Choosing c2 adds the migration, the write path, and hydration
to Phase-3's task list — flag it now because it reshapes the Phase-3 estimate, not the
Phase-1 sprint.

---

## Q2 — Default weight values: what is the fleet optimizing for?

**Question.** The Phase-1 default weights (`model_resident 5`, `gpu_utilization 3`,
`model_fits_vram 2`, `vram_headroom 2`, `operator_priority 2`, `gpu_temperature 1`,
`tag_affinity 1`, `prompt_size_vs_capacity 1`, `backend_type_affinity 0`) encode a specific
preference. Is "prefer the model-resident, least-loaded node, respecting operator priority"
the right default posture?

**Why it needs a human.** Weight ratios are a **policy statement** about what the operator
values — there is no objectively correct answer derivable from code. Different operators want
different things:
- **Latency-first** (interactive workloads): bump `gpu_utilization`, add Phase-2 `ttft_p50`
  weight, lower `operator_priority`.
- **Utilization/packing-first** (batch / cost-efficiency): favor filling nodes
  (lower `gpu_utilization` weight, raise `model_resident` to avoid reloads).
- **Operator-intent-first** (pinned tiers): raise `operator_priority` to dominate.
- **Cost/power-aware** (Phase 4): once `power_cost` exists, weight it up.

**Options.** Ship one default posture (current), or ship a small set of **named presets**
(`balanced` | `latency` | `utilization` | `operator`) the operator selects, expanding to
custom weights. Presets are a later additive nicety, not Phase-1-blocking.

**Recommendation (spec default): the "balanced" weights as listed**, documented as a sane
starting point, fully overridable per-key in `routing.scored.weights`.

**What flips if overturned.** Only the default constants in the defaults table change —
mechanical, no engine impact. If presets are wanted, that's a small additive config feature
(a named-preset → weight-map expansion at load), deferrable.

---

## Q3 — Is fleet-relative scoring's cross-call variability acceptable as product behavior?

**Question.** `operator_priority` (dim 7) is normalized **fleet-relative** (min/max over the
candidate set). Consequence: the *same* backend can produce a different `operator_priority`
norm in two different requests if the candidate set differed (e.g. a peer was excluded by the
retry loop, or went unhealthy). The architect's determinism contract is **same snapshot +
same request ⇒ same route** — it does **not** promise cross-call score *stability*. Is that
the behavior you want to commit to as a product property?

**Why it needs a human.** It is defensible engineering (routing only ever decides among *this
call's* candidates, so peer-relative ranking is meaningful), but it is also a behavior an
operator might find surprising when reading audit logs ("why did node A's priority score
change between two requests?"). Whether to accept that surprise is a product call.

**Options.**
- **Accept it (RECOMMENDED).** Keep `operator_priority` fleet-relative — it's the one
  dimension with no natural absolute scale, and the variability only appears when the
  candidate set itself changed (which *should* change the decision). All other Phase-1 dims
  are absolute-threshold and peer-independent, so this is the lone source of cross-call
  variance.
- **Make it absolute.** Map `priority` through a fixed curve (e.g. `clamp(priority/100, 0,
  1)` treating 100 as "max meaningful"). Stable across calls and auditable, but injects a
  magic constant (what is "max priority"?) and mis-scores fleets that use, say, 1–10 or
  1–1000 priority ranges.

**Recommendation (spec default): accept fleet-relative for dim 7 only**, with the all-equal/
n=1 → `0.5` collapse rule. Documented in the Normalization Strategy section.

**What flips if overturned.** If you want fully peer-independent scoring, dim 7 moves to an
absolute curve with an operator-supplied "priority ceiling" constant — a small config
addition and a one-line normalization change.

---

## Q4 — Cardinality / large-fleet behavior for the Phase-3 store and the metrics map

**Question.** `metrics.rs` `labeled_latency` is hard-capped at **200** distinct
`backend|model|status` combos and **silently drops** new combos past that
(`metrics.rs:222`). The Phase-3 `RoutingStats` store (which the scorer reads) must decide its
own cardinality policy. For a large fleet × many models, what is the desired behavior when the
combination space is large?

**Why it needs a human.** "Silently drop" means some (backend,model) pairs would read neutral
forever — a *correctness-adjacent* surprise (those nodes never get history-based scoring) that
only manifests at scale, and the right ceiling depends on the operator's actual fleet size and
model catalog, which the architect cannot know.

**What the spec already resolved.** `RoutingStats` v1 uses a **bounded LRU with a soft
ceiling `MAX_ROUTING_STATS = 20_000`** (≈2× the realistic `≤100 backends × ≤100 models ≈
10k` worst case; entries are ~100 B ⇒ low-hundreds-of-KB). At the ceiling it **evicts the
least-recently-updated entry** (request-count LRU, deterministic) — explicitly **NOT** the
`metrics.rs` silent-drop, and an evicted-then-re-seen pair just reads neutral `0.5` until it
re-warms (harmless cold-start, never a wrong score). So the *mechanism* is decided; what
remains for the Director is the **ceiling value as a policy knob**:

**Options (the residual policy choice).**
- **Keep `20_000` as a fixed const (RECOMMENDED).** Covers every realistic self-hosted
  fleet with headroom; no config surface.
- **Config-promote the ceiling** (`routing.scored.max_routing_stats`) for operators running
  a genuinely huge model catalog (thousands of distinct models × many backends). Additive,
  but adds a knob most operators never touch.
- **Separately (independent of routing):** raise or make-configurable the existing
  `metrics.rs` 200-combo cap, since the dashboard already silently under-reports large
  fleets. Same root issue, different module — flagged so it isn't forgotten.

**Recommendation (spec default): fixed `20_000` const + LRU eviction** (already specified);
config-promote only if a deployment legitimately exceeds it. The `metrics.rs` 200 cap is a
pre-existing limitation to raise/configure independently.

**What flips if overturned.** Promoting the ceiling to config is a one-field addition to
`ScoredConfig`; raising `metrics.rs`'s cap is a separate one-line/one-field change.

---

## Q5 — Dimension *direction/semantics* that are genuinely product choices

A few dimensions have a defensible-either-way reading. The spec picks one; flagging the ones
that are policy, not mechanics.

**Q5a — `tag_affinity` (dim 8) semantics.** The GATE already enforces `requested_tags ⊆
backend.tags`, so among candidates every requested tag is matched and the dimension is
**uniformly 1.0 / non-discriminating in Phase 1**. Two readings: (i) keep it as a forward
placeholder that only bites once tags become a *soft preference* rather than a hard gate
(current); (ii) redefine it now as a bonus for backends carrying *extra* relevant tags beyond
those requested. The latter is a real product decision about whether extra tags should pull
traffic. **Recommendation: keep (i)** — tags are a hard gate today; don't overload them with
soft semantics mid-sprint. *Overturn cost:* a normalization redefinition, no engine change.

**Q5b — `backend_type_affinity` (dim 9) default weight `0.0`.** The house principle is
backend-agnostic routing, so this is off by default. But an operator who *does* prefer
llama-server over Ollama for throughput (per `docs/LLAMA_CPP_BACKEND.md`) might reasonably want
a non-zero default. **Recommendation: keep `0.0`** (agnostic by default; operators opt in via
`prefer_backend_type` + a non-zero weight). *Overturn cost:* one default constant.

**Q5c — Model VRAM-size estimate constant (dim 2).** `model_fits_vram` estimates model
footprint from the name via `extract_param_billions(model)` × a bytes-per-billion-params
constant (spec uses a rough `~1024 MB/B` fp16-ish figure). The real footprint depends on
quantization (Q4_K_M vs fp16 differ ~4×), which the model name often doesn't encode. The
estimate is deliberately coarse. **Recommendation: ship the coarse constant** (it only needs
to be directionally right for the ratio normalization) and refine later, possibly from
observed load telemetry. *Overturn cost:* tune one constant, or add a per-model override map
(additive). Flagged because an operator running heavily-quantized models may find the default
estimate pessimistic.

---

## Q6 — Should a dimension that is *uniform across this call's candidates* be neutralized? (report-nothing vs report-bad-news)

> **RESOLVED 2026-06-14 = option (b): neutralize call-uniform dimensions.** A dimension that
> takes the SAME normalized value for ALL candidates in a given routing call carries zero
> discriminating information and is **DROPPED from scoring for that call** — removed from every
> candidate's present-set BEFORE per-backend active-weight renormalization, rather than scored.
> This generalizes the existing dim-7 (`operator_priority`) all-equal → non-participation rule
> into a uniform rule applied to every dimension. The spec now SPECIFIES this behavior: see the
> **call-uniform pre-pass** in `smart-routing-scorer-spec.md` (Scoring Math), the determinism
> proof step 3/3a (incl. the all-dimensions-dropped → `denom == 0 → 0.5` → priority/name edge
> case), and acceptance test #12. The question text below is retained for the decision record.

**Question.** Under per-backend active-weight renormalization (`score = Σ_present w·n /
Σ_present w`), a dimension that is **present** but takes the **same value for every candidate
in a call** is *not* inert: it still enters each candidate's denominator, and because
telemetry-poor candidates have *smaller* denominators dominated by the uniform-`1.0` dims,
they can **out-score** telemetry-rich candidates reporting honest-mediocre values. Worked
example (default weights, model resident on both ⇒ dim 1 uniform `1.0`; tags requested ⇒ dim
8 uniform `1.0`; equal priority ⇒ dim 7 = `0.5`):

| Candidate | Present dims (norm·w) | denom | score |
|-----------|------------------------|-------|-------|
| **P** — reports nothing on GPU | `1.0·5`, `1.0·1`, `0.5·2` | 8 | **0.875** |
| **R** — reports honest 50% util/vram/temp | `1.0·5`, `1.0·1`, `0.5·2`, `0.5·3`, `0.5·2`, `0.5·1` | 14 | **0.714** |

`P` wins despite contributing *no* GPU information, purely because its denominator is
dominated by the `1.0`-pinned dims. This is the **core missing-value-neutrality tension**: a
backend that *says nothing* is weight-dropped to neutral on the axis, but a backend that
*reports honest bad news* is scored on it — so silence can beat candor.

**Why it needs a human.** Both behaviors are defensible and the choice is a **policy stance**,
not a correctness bug (the formula is computed correctly either way):
- *Keep current behavior* — a telemetry-rich backend reporting genuinely mediocre GPU state
  is, arguably, correctly out-ranked by a backend we have no bad news about. "Innocent until
  measured guilty."
- *Neutralize uniform dims* — "a node shouldn't be advantaged for withholding telemetry."

**Options.**
- **(a) Keep as-is (status quo).** A present uniform dim contributes to the level. Simplest;
  matches the literal weighted-mean semantics. *Cost:* the report-nothing-beats-report-bad-news
  artifact above; an operator reading the audit log may be surprised a no-metrics node won.
- **(b) Neutralize call-uniform dims — RECOMMENDED.** A dimension whose `nᵢ(b)` is identical
  for **all** candidates in this call contributes nothing to *discrimination*, so treat it
  exactly like an **absent** dim for *every* candidate that call — drop it from each
  candidate's present-set (and denominator) for that call. This is the **same shape** as the
  fleet-relative all-equal rule that already collapses dim 7 to non-participation when
  `pmin == pmax`, generalized to any dim. It is **deterministic** (uniformity is a pure
  function of the candidate set, order-independent) and it **removes the artifact** (in the
  example both P and R drop dims 1 and 8, then P scores `0.5` on dim 7 alone, R scores `0.5`
  on dims 7+4+5+6 — they tie and fall to priority/name, so the no-metrics node no longer
  *wins* on phantom strength). *Cost / side effects:* it changes which dims are present per
  call (like fleet-relative dim 7 already does — acceptable, the determinism contract is
  same-snapshot-same-request, not cross-call score stability); it **equalizes**
  "report-nothing" with "report-bad-news" (which is itself the policy stance, made explicit);
  and post-relax it neutralizes dim 1's uniform `0.0`, which is harmless (it was
  non-discriminating anyway). Implementation: a pre-pass over the candidate set computes, per
  dimension, whether all present candidates share one `nᵢ` value (within the `1e-6` quantum);
  if so, mark that dim not-present for the whole call.
- **(c) Neutralize only the *trivially*-uniform structural dims (dim 1 pre/post-relax, dim 8
  under the subset gate)** — narrower than (b): hard-code that the gate-induced uniform dims
  don't enter the denominator, but leave genuinely-coincidentally-uniform telemetry dims
  scored. *Cost:* special-cases the catalog; less principled than (b)'s general rule.

**Decision (2026-06-14): (b), neutralize call-uniform dims** — adopted as the principled
generalization of the existing dim-7 all-equal rule. The Director owned the "silence ==
bad-news" policy stance and ruled to equalize report-nothing with report-bad-news on any axis
that cannot discriminate among the call's candidates. The spec now **specifies** (b): a
deterministic pre-pass computes per-dim uniformity over the candidate set (using the existing
`1e-6` quantization, no new epsilon) and drops every uniform dim from every candidate's
present-set before renormalization. See `smart-routing-scorer-spec.md` Scoring Math
(call-uniform pre-pass), determinism proof step 3/3a, and acceptance test #12.

**Implementation footprint of (b).** One pre-pass in the scoring kernel (per-dim uniformity
over the candidate set, drop uniform dims from every present-set), plus acceptance test #12
(a uniform dim is provably dropped and a discriminating dim decides the route; an all-identical
fleet falls through to the priority/name tie-break via the `denom == 0 → 0.5` guard without
panic). No config surface, no schema change. Determinism is preserved because the dropped-dim
set is a pure function of the (snapshot, request) via the fixed dimension order and the
existing quantizer.

---

## Summary of escalations

| # | Decision | Spec default | Overturn impact |
|---|----------|--------------|-----------------|
| Q1 | Phase-3 history persistence | In-memory (c1) | Adds SQLite migration + write path (reshapes Phase 3) |
| Q2 | Default weight posture | "Balanced" weights | Constants only; presets are additive |
| Q3 | Fleet-relative dim-7 variability | Accept (relative for priority only) | Absolute curve + "ceiling" constant |
| Q4 | Large-fleet cardinality | Bounded LRU, ceiling `20_000` (fixed const) | Config-promote the ceiling (one field) |
| Q5a | `tag_affinity` semantics | Forward placeholder | Normalization redefinition |
| Q5b | `backend_type_affinity` default | `0.0` (agnostic) | One constant |
| Q5c | VRAM-size estimate constant | Coarse, name-derived | Tune constant / add override map |
| Q6 | Uniform-present-dim neutralization (report-nothing vs report-bad-news) | **RESOLVED 2026-06-14 = (b)**: drop call-uniform dims (now specified) | One scoring-kernel pre-pass + acceptance test #12; no config/schema change |

Only **Q1** can reshape a phase's scope; Q2–Q5 are constants or small additive features. **Q6
was a ranking-policy call, now RESOLVED 2026-06-14 = (b)**: the spec specifies neutralizing
call-uniform dimensions (dropping any dim that takes the same value for all candidates this
call), the principled generalization of the dim-7 all-equal rule. This is a contained
scoring-kernel change (one deterministic pre-pass + acceptance test #12), not a phase reshape,
with no config or schema surface.
