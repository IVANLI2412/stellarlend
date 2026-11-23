# Governance Lifecycle

StellarLend’s on-chain governance (`hello-world` contract, `gov_*` entrypoints) implements a full proposal lifecycle with timelock, quorum, and voting-threshold gates. This document describes the state machine, configuration knobs, and how the lifecycle is exercised in E2E tests and gas benchmarks (Issue #690).

## Proposal State Machine

```
 create ──► Active ──(end_time)──► Succeeded ──queue──► Queued ──(eta)──► Executed
              │                        │                  │
              │                        └─(grace)─► Expired└─(eta + grace)─► Expired
              └──(end_time, fails)──► Defeated
 Pending / Active / Queued ──cancel (proposer, admin, guardian)──► Cancelled
```

Only transitions made by a successful call are written to storage. States that
follow from time alone (voting closed, grace period lapsed) are derived on read
by `gov_get_proposal_state`, because a call that fails cannot persist anything.
Clients should read state from `gov_get_proposal_state`, not from the stored
`Proposal.status`.

| Status | Meaning |
|--------|---------|
| `Pending` | Stored at creation; reported as `Active` once `now >= start_time` |
| `Active` | Voting open: `start_time <= now < end_time` |
| `Succeeded` | Voting closed, quorum and threshold met, not yet queued (derived) |
| `Defeated` | Voting closed without quorum or threshold (derived, stored by `queue`) |
| `Queued` | In the timelock; executable from `execution_time` (eta) |
| `Executed` | Applied by `execute_proposal` (terminal) |
| `Expired` | Not queued within the grace period after `end_time`, or not executed within the grace period after eta (derived, terminal) |
| `Cancelled` | Cancelled before execution (terminal) |

## Voting Power (lock-to-vote)

Soroban tokens expose neither historical balances nor a total supply, so
governance cannot snapshot holders by reading the vote token. Instead:

- Holders lock vote tokens with `gov_lock_tokens` and withdraw them with
  `gov_unlock_tokens`. Locked tokens are held by the contract.
- Every change to an account's voting power, or to the total locked supply,
  writes a timestamped checkpoint.
- A proposal counts power held **strictly before** the timestamp it was created
  at. Tokens borrowed, bought, locked or delegated in the proposal's own ledger
  or later carry no weight on it. This is what stops flash-loan voting and
  vote-then-transfer double voting.
- After voting, a voter's locked tokens and delegation stay fixed until the
  voting period of every proposal they voted on has ended (`gov_is_vote_locked`).

### Delegation

`gov_delegate_vote(delegator, delegatee)` moves the voting power of the
delegator's locked tokens, including tokens locked later, to the delegatee.
`gov_revoke_delegation` returns it. Calling `gov_delegate_vote` again
re-delegates.

Delegation is single-hop. Power received by delegation cannot be delegated
onward. A delegatee can still delegate its *own* locked tokens, so chains and
cycles cannot form and no depth limit is needed. Delegation counts on a proposal
only if it predates the proposal's creation, like any other change to voting
power.

## Configuration (`gov_initialize`)

```rust
gov_initialize(
    admin: Address,
    vote_token: Address,
    voting_period: Option<u64>,        // seconds; 1 hour ..= 30 days
    execution_delay: Option<u64>,      // seconds; timelock between queue and eta, <= 30 days
    quorum_bps: Option<u32>,           // share of locked supply that must vote; 1 ..= 10_000
    proposal_threshold: Option<i128>,  // min voting power to create a proposal
    timelock_duration: Option<u64>,    // seconds; grace period to queue / execute, 1 ..= 30 days
    default_voting_threshold: Option<i128>, // bps of votes cast that must be `For`; 1 ..= 10_000
)
```

Defaults fall back to contract-level constants (`DEFAULT_TIMELOCK_DURATION`,
etc.) when `None`. Out-of-bounds values are rejected.

After initialization, the rules change only through governance itself: an
`UpdateGovernanceConfig(GovernanceParams)` proposal passes a vote and the
timelock like any other. Its bounds are re-checked at execution. A proposal keeps
the voting period, quorum and threshold it was created under.

## Key Invariants Enforced On-Chain

1. **Proposal threshold**: the proposer's voting power before the current ledger is at least `proposal_threshold`.
2. **Vote power**: checkpointed power strictly before `created_at`. Voters with zero power are rejected (`NoVotingPower`).
3. **One vote per address**: a second vote reverts with `AlreadyVoted`.
4. **Voting window**: votes are accepted only while `start_time <= now < end_time` (`NotInVotingPeriod`).
5. **Quorum**: `for + against + abstain >= quorum_votes`, where `quorum_votes = ceil(total_locked_before_creation * quorum_bps / 10_000)` is fixed at creation.
6. **Threshold**: `for >= (for + against + abstain) * voting_threshold / 10_000` and `for > against`, so a tie never passes. A proposer may raise `voting_threshold` above the default, never lower it.
7. **Execution delay**: `execute` rejects while `now < eta` (`ExecutionTooEarly`), where `eta = queue time + execution_delay`.
8. **Grace period**: a proposal must be queued within `timelock_duration` of `end_time`, and executed within `timelock_duration` of eta, or it expires.
9. **Veto window**: the proposer, admin, or any guardian can cancel a proposal until it executes, including while it is queued. See [Timelock Veto Window](#timelock-veto-window) for the exact semantics and the security-council escape path.
10. **Emergency proposals** skip voting and the delay. Each needs approvals (`gov_approve_proposal`) from the multisig threshold of current multisig admins, and a multisig admin must execute it.

## Timelock Veto Window

Once a proposal is `Queued` it sits in the timelock until its eta
(`queue time + execution_delay`). During that window the proposal can still be
stopped by a veto; after it executes the veto is no longer possible.

### Who may veto

| Actor | Entrypoint | Notes |
|-------|-----------|-------|
| Proposer | `gov_cancel_proposal` | Always allowed while `Pending`/`Active`/`Queued` |
| Governance admin | `gov_cancel_proposal` | Always allowed while `Pending`/`Active`/`Queued` |
| Guardian | `gov_cancel_proposal` | Registered via `gov_add_guardian` |
| Security council | `gov_council_escape` | Escape hatch, see below |

A veto moves the proposal to `Cancelled` (terminal). Vetoing an `Executed`,
`Expired` or already `Cancelled` proposal reverts with `ProposalNotCancellable`.

### Security-council escape

The security council is the last line of defence when the normal veto path is
unavailable (for example the admin key is compromised or the proposer is the
attacker). It is a distinct role from guardians and is configured with
`gov_set_security_council(council, threshold)`.

- `threshold` is the number of council members that must approve an escape
  before it can be applied; it must be `>= 1` and `<=` the number of registered
  council members.
- Members approve with `gov_council_approve(proposal_id, member)`. Each member
  counts once; a duplicate approval reverts with `AlreadyApproved`.
- Once `threshold` approvals are collected, any council member calls
  `gov_council_escape(proposal_id, member)` to cancel the proposal immediately,
  bypassing the remaining timelock delay. The escape is only valid while the
  proposal is `Queued` (or `Active`); it cannot resurrect an `Executed` or
  `Expired` proposal.
- The escape does **not** skip the grace period check for execution: it only
  cancels. To push a replacement proposal, create and queue a new one through
  the normal lifecycle.

### Events

| Event | Topics | Emitted when |
|-------|--------|--------------|
| `timelock` | `(proposal_id,)` | A proposal enters the timelock (`gov_queue_proposal` succeeds) |
| `vetoed` | `(proposal_id, actor)` | A proposal is cancelled by proposer, admin or guardian |
| `timelock` | `(proposal_id, eta)` | The timelock eta is (re)confirmed on read/queue |
| `council_escape` | `(proposal_id, member)` | The security council applies an escape cancellation |

Consumers should treat `vetoed` and `council_escape` as terminal-cancel signals
and stop tracking the proposal for execution.

### Operational runbook: emergency cancellation

1. **Confirm the target.** Read `gov_get_proposal_state(id)`; it must be
   `Active` or `Queued`. If it is already `Executed` or `Expired`, stop — the
   escape cannot help.
2. **Assess the normal path.** If the admin or a guardian key is healthy, use
   `gov_cancel_proposal(id, actor)` and skip the council. The council escape is
   for when those keys are unavailable or untrusted.
3. **Collect approvals.** Have each council member submit
   `gov_council_approve(id, member)` until the configured `threshold` is met.
   Watch for `council_escape`-adjacent `approval` events to confirm progress.
4. **Apply the escape.** Any council member calls `gov_council_escape(id, member)`.
   On success a `council_escape` event is emitted and the proposal is
   `Cancelled`.
5. **Verify.** Re-read `gov_get_proposal_state(id)`; it must be `Cancelled`.
   Confirm the `council_escape` event in the transaction trace.
6. **Follow up.** Rotate any compromised admin/guardian keys, then re-submit the
   proposal (or a corrected version) through `gov_create_proposal` and the normal
   queue/execute path. Record the incident and the escape transaction hash in the
   governance runbook log.
=======

## Entrypoints (all under `#[contractimpl]`)

| Entrypoint | Purpose |
|-----------|---------|
| `gov_initialize` | One-time config + vote token setup |
| `gov_lock_tokens` / `gov_unlock_tokens` | Lock vote tokens for voting power / withdraw them |
| `gov_delegate_vote` / `gov_revoke_delegation` | Delegate locked-token power / take it back |
| `gov_get_votes` / `gov_get_past_votes` | Current voting power / power strictly before a timestamp |
| `gov_get_locked_balance` / `gov_get_total_locked` / `gov_get_delegation` | Lock and delegation views |
| `gov_create_proposal` | Open a proposal (threshold-gated) |
| `gov_vote` | Cast `For`/`Against`/`Abstain` with checkpointed power |
| `gov_get_vote_power_snapshot` | The power a voter can use on a proposal |
| `gov_queue_proposal` | Tally after voting ends → `Queued` or `Defeated` |
| `gov_execute_proposal` | Apply a queued proposal after the delay, within the grace period |
| `gov_cancel_proposal` | Cancel / veto by proposer, admin or guardian |
| `gov_set_security_council` | Configure the security council and its approval threshold |
| `gov_council_approve` | Council member approves an escape cancellation |
| `gov_council_escape` | Apply a council escape cancellation (bypasses remaining delay) |
| `gov_get_proposal` / `gov_get_proposal_state` | Stored proposal / current lifecycle state |
| `gov_get_config` | Read current config |
| `gov_create_emergency_proposal` / `gov_approve_proposal` | Multisig emergency path |
| `gov_add_guardian` | Register a guardian (can veto proposals) |

## E2E Coverage

`tests/e2e/governance.e2e.test.ts` exercises the full lifecycle against the in-memory harness (`buildGovernanceApp` in `tests/e2e/scenarios/harness.ts`):

- Initialization and re-init rejection
- Proposal creation + threshold enforcement
- Vote activation, double-vote rejection, zero-power rejection
- Queue gating (before voting ends), success → `Queued`, failure → `Defeated`
- Execute gating (before delay → 409; after delay → `Executed`; past timelock → 410/`Expired`)
- Cancel by proposer; cancel of executed proposal rejected
- Timelock veto window: guardian/admin veto while `Queued`, `vetoed` event emitted
- Security-council escape: threshold enforcement, duplicate-approval rejection, `council_escape` event, escape of an `Executed` proposal rejected
- End-to-end happy path with event trace

Run:

```bash
cd tests/e2e
npm ci
npx jest --runInBand --forceExit governance.e2e.test.ts
```

## Gas Benchmarks

`stellar-lend/benchmarks/src/governance_benchmarks.rs` (group: **Governance Lifecycle**) measures instruction counts for:

- `gov_initialize`
- `gov_create_proposal`
- `gov_vote`
- `gov_queue_proposal`
- `gov_execute_proposal`
- `gov_cancel_proposal`
- `gov_council_approve`
- `gov_council_escape`
- `gov_get_proposal`
- `gov_get_governance_config`

Budgets are registered in `framework.rs` (`hello_world::gov_*` keys). Run:

```bash
cd stellar-lend
cargo run --bin run_benchmarks -- --output governance-bench.json
```

## CI

`.github/workflows/governance-testing.yml` runs the governance E2E suite on PRs touching governance paths and nightly.

## Related Specs

- Rust lifecycle tests: `stellar-lend/contracts/hello-world/src/tests/governance_{test,lifecycle_test,attack_prevention_test}.rs`
- State machine source: `stellar-lend/contracts/hello-world/src/governance/{proposal,voting,power,execution}.rs`
