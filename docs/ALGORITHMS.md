# StockMesh algorithms

This guide describes the imported implementation. It separates a mathematical
model, a native quote, an on-chain check and a live deployment claim.

## 1. Economic exposure and OneBook

A ticker may have several issuer products. StockMesh represents each claim with
its own mint and policy and uses a conservative underlying-share conversion for
the final economic condition. Product tokens are not made fungible by that
conversion.

[OneBook](../engine/stockmesh/onebook/mod.rs) contains:

- `SettlementOptionalOrder`: signed stock exposure, price limit, acceptable
  claims, conversion bound, nonce and expiry.
- `ExecutableSlice` / `VirtualLevel`: claim/source/state-bound quantities and
  prices that expire with their state.
- `match_continuous`: price/time matching across compatible claim constraints.

Virtual levels are executable recipes, not invented resting liquidity. A
claim outside the buyer's accepted set cannot be substituted.

The [claim program](../engine/stockmesh/programs/stocklana-settle/src/claim.rs)
binds versioned conversions on chain. The
[exposure program](../engine/stockmesh/programs/stocklana-settle/src/exposure.rs)
preflights product graphs and validates the aggregate conservative underlying-
share floor while keeping all product mints distinct.

**Tests:** [OneBook](../engine/stockmesh/tests/onebook.rs),
[exposure model](../engine/stockmesh/tests/exposure_oracle.rs). These are native
tests, not deployed-program verification.

## 2. Marginal Liquidity IR

The [compiler](../engine/stockmesh/compiler/mod.rs) converts admitted same-pair,
independent, synchronously settleable models into bounded concave curves.
Each band stores input capacity and marginal output per input atom in Q32.

For constant-product models, the compiler uses at most 16 adaptive chords. It
splits the interval with the largest tangent/chord error bound instead of
iterating once for every input atom. The curve carries both lower and upper
output envelopes.

```text
lower_quote(x) ≤ admitted model's exact_quote(x)
admitted model's exact_quote(x) ≤ IR_quote(x) + upper_error_atoms
```

The generic core constant-product model is not a claim of complete Raydium ABI
compatibility. Native venue decoders supply their own fee, rounding, tick/bin,
Token-2022 and account-layout handling. Unsupported variants must stay outside
the eligible envelope.

## 3. Global Marginal Solver

For a fixed admitted same-pair envelope, the objective is to maximize total
output subject to the input budget, capacities, independent writable market
state and the maximum number of used legs.

[The solver](../engine/stockmesh/optimizer/mod.rs) follows this sequence:

1. Merge non-increasing marginal bands for a relaxed allocation.
2. Enumerate eligible venue subsets satisfying the leg cap and resource
   independence checks.
3. Rank feasible allocations using conservative model estimates.
4. Exact-evaluate up to four distinct finalists against the original models.
5. Return the winning allocation and a relaxed upper-bound gap.

At eight candidate edges with a four-leg cap there are at most 162 subsets.
`upper_output - achieved_output` is the certificate's `gap_atoms`. Its domain is
the admitted models; the shortlist is not presented as an unqualified global
optimum over arbitrary native pools or all chain routes.

**Tests:** [kernel](../engine/stockmesh/tests/kernel.rs), including exhaustive
small integer references, model envelope checks and duplicate-liquidity rejection.

## 4. Residual Tape: reuse across subsets and sizes

[Residual Tape](../engine/stockmesh/optimizer/tape.rs) merges the marginal bands
once. Prefix row `P[k][i]` counts how many complete bands of venue `i` appear
among the first `k` globally ordered bands. Per-venue indexes map these counts
to capacity and integral.

For a subset mask and remaining input, rank/select finds the prefix that reaches
the requested capacity. All prior bands are complete; one marginal band takes
the remaining atoms. Removing a venue preserves the relative ordering of all
remaining bands, so the tape is valid for every subset of its compiled mask.

Implementation details:

- A winner tree repairs only the changed frontier for larger merges.
- One/two-stream merges take a separate small-stream path.
- Equal rates have a deterministic venue-index tie break.
- Exhaustion has a distinct sentinel; a zero-rate band is still a valid band.
- Allocation performs no heap allocation or re-sort.
- Curve identity changes require rebuilding the tape.

This accelerates the IR solver without replacing it with a coarser allocation
grid. Its storage is caller-owned; an SBF caller must choose an explicit
heap/account layout instead of placing the full tape on a 4 KiB stack frame.

**Tests:** every nonempty eight-edge mask and a range of residual sizes are
compared with the independent streaming allocator in
[kernel tests](../engine/stockmesh/tests/kernel.rs). Work metering and stable
workspace size are exercised in
[optimizer regressions](../engine/stockmesh/tests/optimizer_regressions.rs).

## 5. Native exact refinement

[Native source](../engine/stockmesh/native/src) includes checked integer models
for concentrated liquidity, Whirlpool, DLMM, DAMM and transfer-fee behavior.
CLMM/DLMM arithmetic uses Q64/U256 where appropriate. Dynamic fees consume a
coherent snapshot clock, not a worker's wall clock.

[QuoteMemo](../engine/stockmesh/native/src/memo.rs) is a bounded per-worker cache.
Its namespace must commit to bank bytes, edge order and decoder policy. The
implementation limits probing and falls back to evaluation instead of returning
an unrelated cached answer.

[Exact-oracle refinement](../engine/stockmesh/optimizer/oracle.rs) begins from
feasible single-venue/subset seeds and moves integer input between candidates.
Step sizes shrink toward token atoms; quote-call, per-scale exchange, resource
and leg budgets bound the search. The best feasible incumbent survives budget
exhaustion, with exhaustion/step information returned to the caller.

This path permits rounded or non-concave curves. It returns the best observed
allocation, not the concave IR path's optimality certificate. Resource estimates
also do not replace final runtime simulation.

**Tests:** [exact-oracle tests](../engine/stockmesh/tests/oracle.rs). Native
decoder and SBF conformance require their separate package/harness checks.

## 6. Flow Folding: multi-asset internal clearing

[Flow Folding](../engine/stockmesh/clearing/mod.rs) is a bounded maximum-cost
circulation problem at supplied common prices or per-asset lots. It constructs
a residual graph and searches for improving cycles. Reverse arcs can undo an
earlier crossing to make room for a better cycle.

The lot-based form avoids a global least-common-multiple explosion from
unrelated price numerators while conserving each asset's integer units. Its
optimality flag is conditional on fixed admitted lots/prices; reaching the pivot
bound returns a feasible result without asserting optimality.

This objective is not simultaneous global price discovery and DEX routing.
The [FlowCell program](../engine/stockmesh/programs/stocklana-settle/src/flowcell.rs)
validates signed transfers and each owner's residual graph, then checks actual
per-owner token deltas. The
[StockMesh cell](../engine/stockmesh/programs/stocklana-settle/src/meshcell.rs)
specializes this for issuer-product crossing and aggregate stock exposure.

**Tests:** [clearing](../engine/stockmesh/tests/clearing.rs),
[cell](../engine/stockmesh/tests/cell.rs), including overlapping cycles,
conservation, dust and per-user minimum-output constraints.

## 7. Execute, observe, reallocate

The [reference reflow state machine](../engine/stockmesh/reflow/mod.rs) checks
identity, state generation, freshness, independence and remaining work before
invocation. After a successful invocation it observes actual balances. A
successful partial/zero fill can leave a residual; a CPI error cannot be
reinterpreted as one.

The [on-chain allocation oracle](../engine/stockmesh/programs/stocklana-settle/src/allocation.rs)
operates only on signed, predeclared candidates. It compiles tick/bin structures
once, then rereads mutable heads and tracks observed use. The
[typed graph](../engine/stockmesh/programs/stocklana-settle/src/graph.rs) limits
spending to signed budgets or actual proceeds from earlier legs.

Funding, internal crossing and residual graphs preflight before moving assets.
Aggregate exposure and token minima are checked before the parent completes and
advances its nonce. Multi-transaction
[capsules](../engine/stockmesh/programs/stocklana-settle/src/capsule.rs) are
sequenced and resumable, not collectively atomic.

## Evidence and performance

The published default CI runs 35 native/core tests. It does not run every native
adapter, host or SBF harness. The code contains separate optimization paths;
the source map above does not assert that all of them run for every live request.

A credible router comparison must pin the same state, eligible products,
amount, fees, output denomination, account/CU budget and inclusion assumptions.
Report achieved output and model gap separately from planning time, p99 latency,
CU and confirmed economic outcome. No measured competitor advantage is claimed
by this documentation update.
