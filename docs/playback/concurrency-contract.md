# #57 — Decode-ahead concurrency contract

> Status: design contract. Implemented in Phase 3 (epoch + scheduler want-list) and Phase 4
> (priority mailbox + decode-ahead worker). See [README](README.md) and the
> [memory contract](memory-contract.md) — back-pressure here *is* the #56 budget.

## Decode stays sequential

One `ExrData::load` already saturates all cores via the patched `exr` crate's internal global-rayon
block decode. There is **no `ThreadPoolBuilder` anywhere**. `tools.rs`'s `into_par_iter` over files
would **nest** over exr's internal rayon → oversubscription — this is the #57 trap.

So decode-ahead is a **scheduling** problem, not a parallelism one. **Do not** reuse
`run_conversion_task`'s `into_par_iter` shape for prefetch.

## Worker model

Keep the single dedicated worker (`app.rs`), one in-flight decode at a time. Change **what feeds it**:

- Replace the unbounded FIFO `LoadJob` mailbox with a **single-slot priority mailbox**
  (`Mutex<Option<Request>>` + `Condvar`, or a capacity-1 rendezvous).
- On finishing a frame, the worker asks the UI-side **scheduler** for the next.

### The scheduler (pure, unit-testable)
Inputs: `playhead + direction + resident set + budget` → an **ordered want-list**. Priorities:

| Pri | Meaning |
|-----|---------|
| **P0** | user seek / explicit open |
| **P1** | playhead frame not yet resident (we fell behind) |
| **P2** | prefetch ahead in the play direction, nearest first |

## Back-pressure = the cache budget itself

The scheduler never requests a frame that wouldn't fit the **T1 budget**:

```
decode_ahead = min(configured, max_t1 − 1)
```

Ring full ahead of the playhead → "nothing to do," the worker idles. There is **no separate bounded
queue** — the budget *is* the bound. This ties #57 directly to #56.

## Epoch counter — required; path-check is insufficient

Today supersession is a `(path, is_b)` mismatch (`app.rs`). Sequences **recur the same paths** (loop,
ping-pong, scrub-back), so a stale `frame.0007.exr` result could be mistaken for the current one.

Add **`epoch: AtomicU64`**:

- Bumped on every **seek / scrub / direction-change / sequence-change**.
- Carried by **each request** and **each `LoadMsg`** result.
- The UI **drops** any result whose `epoch ≠ current`.

`ExrData::load` is **not cancellable mid-decode** (exr's internal rayon gives no clean seam), so a
seek **wastes at most one in-flight decode**. Document this; do **not** design mid-decode
cancellation.

## Ordering

Decodes complete in issue order per epoch, but the **playhead advances only when the needed frame's
T2 is resident**. The clock tick checks residency and **never blocks the UI thread on a decode** —
the pacing policy decides drop-vs-stutter (see [sequence-playback](sequence-playback.md)).

## exr internal pool — leave it alone (by default)

One decode wanting all cores is optimal when decode-bound, so leave the global rayon pool alone. A
dedicated bounded decode pool is a **measure-first** lever (UI-thread texture packing also uses
rayon, `viewer.rs`) — out of scope for v2.0 unless measured to be a problem (Phase 4).
