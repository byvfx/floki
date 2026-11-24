Decode-ahead is a scheduling problem. The full note is [`docs/playback/concurrency-contract.md`](https://github.com/byvfx/floki/blob/main/docs/playback/concurrency-contract.md). Back-pressure here *is* the [memory contract](Memory-contract) budget.

One `ExrData::load` already saturates every core through the patched `exr` crate's internal rayon block decode. There is no `ThreadPoolBuilder` anywhere. `tools.rs` parallelizes *across files*. Nesting that `into_par_iter` under exr's pool oversubscribes the machine. Do not reuse `run_conversion_task`'s shape for prefetch.

## One worker

Keep the single dedicated worker in `app.rs`, one decode in flight. The original design replaced the unbounded FIFO with a single-slot priority mailbox. What shipped (#153) kept the FIFO and added a worker-side **epoch skip** (`epoch_signal: AtomicU64`). Queued sequence jobs superseded by a newer seek are dropped before their decode. Combined with `pump_decode` submitting at most one job, that is near the mailbox for sequence frames.

The accepted gap: explicit opens are generation-keyed, never epoch-skipped, so a reference opened mid-playback can queue ahead of the next playhead frame — one full decode of priority inversion.

The scheduler is pure. Inputs are playhead, direction, the resident set, and the budget. Output is an ordered want-list:

| Priority | Meaning |
|---|---|
| **P0** | User seek or explicit open |
| **P1** | Playhead frame not yet resident |
| **P2** | Prefetch ahead in the play direction, nearest first |
| **P3** | Read-behind window (#169), after the forward window |

```text
prefetch_depth = min(t1_cap − 1, out_point − in_point)
decode_ahead   = prefetch_depth / active_sources
read_behind    = scheduler::read_behind(decode_ahead)
```

There is no separate bounded queue and no `MAX_PREFETCH` ceiling. A constant of 16 used to clamp playback and not idle, so pressing Play *shrank* the lookahead. It was removed in #207. The budget and the trimmed range are the only bounds. Since #232, `t1_cap` is derived from `budget::t1_budget_bytes` via `budget::frames_in`. Mis-sizing the cap mis-sizes the window. That is what #230 did.

## Epoch

Sequences reuse the same paths (loop, ping-pong, scrub-back), so a path check cannot tell a stale `frame.0007.exr` from the current one. `epoch: AtomicU64` bumps on every seek, scrub, direction change, and sequence change. Each request and each result carries it. The UI drops a result whose epoch does not match.

`ExrData::load` is not cancellable mid-decode. A seek wastes at most one in-flight decode. Don't design mid-decode cancellation.

The clock checks residency and never blocks the UI thread on a decode. Stutter waits for the next frame. Drop-frames skips to the latest resident frame. See [Playback](Playback) and [Sequence playback](Sequence-playback).

Leave the global rayon pool alone unless a measurement says otherwise. One decode that wants every core is the right shape when the machine is decode-bound. The I/O overlap is the prefetch thread (`src/prefetch.rs`, #164), not a second decode pool.
