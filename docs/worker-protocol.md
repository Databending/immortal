# Immortal Worker Protocol

**Status:** descriptive, not aspirational. This documents what the Rust worker
(`immortal-worker-lib`) and the server (`src/service.rs`, `src/server.rs`) actually do as of
`82c483f`. Where the proto allows something the implementation does not do, that is called out
explicitly — an SDK written against the proto alone will get several things wrong.

**Audience:** anyone implementing a worker in another language.

---

## 1. Transport

A worker holds **one long-lived bidirectional stream** plus a set of **unary RPCs** on the same
connection, all on the `immortal.Immortal` service.

| Direction | Mechanism | Messages |
|---|---|---|
| worker → server (stream) | `RegisterWorker` request stream | `ImmortalServerActionVersion` |
| server → worker (stream) | `RegisterWorker` response stream | `ImmortalWorkerActionVersion` |
| worker → server (unary) | `StartActivity`, `CompletedActivity`, `CompletedWorkflow`, `CompletedCall` | — |

Every message is wrapped in a `*Version` envelope with a single `oneof version`. Only `V1` exists.
A message with an absent or unknown version is dropped silently by both sides — it is never an
error, so a version mismatch presents as silence, not a failure.

**Compression.** The server calls `send_compressed(Zstd)` / `accept_compressed(Zstd)`
([server.rs:1142](../src/server.rs#L1142)). Both are negotiated, so a client that advertises no
encoding gets identity framing and works fine. zstd is an optimization, not a requirement.

**Connection failure classification.** The Rust worker treats `UNAVAILABLE`, `DEADLINE_EXCEEDED`,
and `UNKNOWN` whose message contains `transport error`, `connection reset`, `broken pipe`, `tcp`,
or `io error` as retryable; everything else is fatal to the call
([worker.rs:1128](../immortal-worker-lib/src/models/worker.rs#L1128)).

---

## 2. Identity

| Field | Meaning | Constraint |
|---|---|---|
| `instance_id` | This worker **process** | MUST be a UUID (server parses it; non-UUID ⇒ `InvalidArgument`). Stable for the process lifetime, including across reconnects. |
| `worker_id` | Build/version identity of the *code* | Free-form string. Sourced from `worker_build_id`. Not unique. |
| `task_queue` | The single queue this worker serves | One per worker. Used for all dispatch matching. |
| `worker_type` | — | The Rust worker sets this to the task queue name. Unused by the server. |

The server rejects a registration whose `instance_id` is already in its `workers` map
([server.rs:447](../src/server.rs#L447)).

> **Reconnect hazard.** The Rust worker reuses its `instance_id` across reconnects, but the server
> only removes the old entry when its *outbound* task notices the stream died. Reconnecting inside
> that window fails with `InvalidArgument: Instance ID already registered.` The worker's recovery is
> to sleep 1s and retry the whole loop ([worker.rs:738](../immortal-worker-lib/src/models/worker.rs#L738)),
> which eventually succeeds. **An SDK must treat this specific error as retryable, not fatal.**
> Generating a fresh `instance_id` per connection attempt would avoid the race but breaks the
> re-attach flow in §3.2, which matches on the worker's claimed running work rather than on identity.

---

## 3. Registration

### 3.1 The first frame

The first message on the request stream **MUST** be `ImmortalServerActionV1.register_worker`.
The server reads exactly one message before doing anything else
([server.rs:415](../src/server.rs#L415)); if that message is not a `RegisterWorker`, `worker_details`
stays `None` and the RPC fails with `InvalidArgument: Worker details never provided`.

`RegisterImmortalWorkerV1` carries:

- `instance_id`, `worker_id`, `worker_type`, `task_queue`
- `workflow_capacity`, `activity_capacity` — see §5
- `registered_workflows`, `registered_activities`, `registered_calls`, `registered_notifications`
- `running_workflows`, `running_activities` — see §3.2

**Schemas.** Each `Registered*` entry carries `args` and `output` as `bytes`. These are
**UTF-8 JSON Schema documents**, produced in Rust by `schemars` and serialized with
`simd_json::to_vec` ([worker.rs:622-657](../immortal-worker-lib/src/models/worker.rs#L622)).
`args` is an **array** of schemas (one per function parameter) for workflows, and a **single**
schema for activities, calls, and notifications. `registered_notifications` has no `output`.

The server parses these eagerly with `unwrap()` ([server.rs:593](../src/server.rs#L593)) — **malformed
or empty schema bytes panic the registration task.** Emit `{}` rather than an empty byte string if a
schema is unavailable.

### 3.2 Re-attach: reclaiming in-flight work

`running_workflows` and `running_activities` are how a reconnecting worker tells the server
"these are still executing in my process." This is the mechanism that survives a server restart
without abandoning live work. The server reconciles each against Redis history
([server.rs:653-765](../src/server.rs#L653)):

**Activities** — looks up `ActivityHistoryMetadata` for `(workflow_id, activity_id)`. If the last
run's `run_id` equals the claimed `activity_run_id`, the activity is re-inserted into
`running_activities` and owned by this worker. Otherwise it is ignored (the worker is running a
stale run; its eventual result is accepted but the run is no longer authoritative).

**Workflows** — looks up `WorkflowHistoryMetadata`:

| History state | Server action |
|---|---|
| `Completed` | Ignore. The worker keeps running it; the result will be discarded. |
| Epoch ≠ claimed epoch | Send `kill_workflow`. |
| Epoch matches, not completed | Re-insert into `running_workflows` under this worker. |
| No history at all | Send `kill_workflow`. |

A worker that always sends empty `running_*` lists is correct but weaker: its in-flight work is not
reclaimed, and the server will re-dispatch that work elsewhere (workflows via the orphan path in
§7.3, activities via `HistoryStatus::Running` re-queue). For a first SDK cut this is an acceptable
simplification, and it is the recommended starting point.

### 3.3 What the server sends back

**Nothing.** There is no registration ack. The stream simply opens and the server begins pushing a
`heartbeat(0)` every second ([server.rs:780-797](../src/server.rs#L780)). The Rust worker considers
itself connected the moment `register_worker()` returns a stream, before receiving any message
([worker.rs:502](../immortal-worker-lib/src/models/worker.rs#L502)).

---

## 4. Steady-state messages

### 4.1 Server → worker (`ImmortalWorkerActionV1.action`)

| Action | Payload | Sent by server? | Handled by Rust worker? |
|---|---|---|---|
| `start_workflow` | `StartWorkflowOptionsV1` | yes | yes |
| `start_activity` | `StartActivityOptionsV1` | yes | yes |
| `start_call` | `StartCallOptionsV1` | yes | yes |
| `notify` | `StartNotificationOptionsV1` | yes | yes |
| `kill_workflow` | `workflow_id` | yes | yes |
| `kill_activity` | `activity_id` | yes | yes |
| `timeout_activity` | `activity_id` | yes | yes → same path as kill, but reports **retryable** failure |
| `kill_call` | `call_id` | yes | yes |
| `heartbeat` | `int32` (always 0) | yes, every 1s | **no — ignored** |
| `check_activity` | `activity_id` | **never** | no |
| `check_workflow` | `workflow_id` | **never** | no |
| `sleep_workflow` | `workflow_id` | **never** | no |

The last four fall into the Rust worker's `_ => {}` arm
([worker.rs:567](../immortal-worker-lib/src/models/worker.rs#L567)). An SDK should ignore unknown
actions the same way rather than erroring.

`check_activity` / `check_workflow` are wired on the *server's receive* side — it maintains
`orphaned_activities` / `orphaned_workflows` oneshot maps waiting for the worker's answer
([server.rs:473-491](../src/server.rs#L473)) — but nothing ever populates those maps or sends the
request. **The liveness-probe flow is dead code on both sides.** Do not implement it.

### 4.2 Worker → server (`ImmortalServerActionV1.action`)

| Action | Sent by Rust worker? | Handled by server? | Notes |
|---|---|---|---|
| `register_worker` | first frame only | yes | §3 |
| `activity_heartbeat_v1` | yes, per running activity | yes | `ActivityHeartbeatV1`. The liveness signal for a running activity — see §6 |
| `log_event` | yes | yes | Persisted to Redis; also refreshes activity liveness as a legacy path (§6) |
| `metrics` | yes, on sampler tick | yes | CPU %, mem used/total; feeds the UI |
| `check_activity` | no | yes (dead) | — |
| `check_workflow` | no | yes (dead) | — |
| `heartbeat` | no | **no** | Ignored by server's `_ => {}` |
| `workflow_heartbeat` | no | **no** | Ignored; workflows have no liveness timeout (§7.3) |
| `activity_heartbeat` (int32, field 3) | no | **no** | Deprecated. Carried no activity id. Superseded by `activity_heartbeat_v1`. |

The server's 1s `heartbeat(0)` is **not** a liveness check on the worker. It is a probe of the
*sending* direction only: it exists so `tx.send()` fails when the client is gone, which is what
triggers worker de-registration ([server.rs:789-833](../src/server.rs#L789)). The worker never
responds to it.

---

## 5. Capacity

Capacity is **entirely server-side accounting**. The worker declares two integers at registration
and never enforces or updates them.

- Dispatch requires `activity_capacity > 0` (or `workflow_capacity > 0`) *and* a matching
  `task_queue` *and* the type present in the worker's registered map
  ([service.rs:1680](../src/service.rs#L1680), [service.rs:1960](../src/service.rs#L1960)).
- Among eligible workers, one is chosen **uniformly at random**.
- On dispatch the server decrements by 1 ([service.rs:1849](../src/service.rs#L1849),
  [service.rs:2078](../src/service.rs#L2078)).
- On completion it increments by 1, clamped at the max captured at registration
  ([service.rs:424](../src/service.rs#L424), [service.rs:508](../src/service.rs#L508)).

Consequences for an SDK:

1. **Capacity is not a queue depth the worker can push back on.** If you declare 50, you can receive
   50 concurrent `start_activity` actions with no flow control. Size your declared capacity to what
   your runtime can genuinely run in parallel.
2. Capacity is restored only through the completion paths. A result that never reaches the server
   leaks a slot for the lifetime of the connection. This is the main reason the outbox in §8 must
   not drop results.
3. Both defaults are `1` in `WorkerConfigBuilder`.

---

## 6. Activity liveness

The server's watchdog ticks every 100ms and times out any running activity where
`now > last_heartbeat + heartbeat_timeout` ([service.rs:1048](../src/service.rs#L1048)).
`heartbeat_timeout` comes from the activity options, defaulting to **30 seconds**
([service.rs:1752](../src/service.rs#L1752)).

`last_heartbeat` is initialized at dispatch and refreshed by `activity_heartbeat_v1`.

### 6.1 Sending heartbeats

For every activity it is executing, a worker MUST send:

```proto
ActivityHeartbeatV1 { activity_id, activity_run_id, workflow_id, workflow_epoch }
```

All four fields must be echoed exactly as received in `StartActivityOptionsV1`.

**Interval.** `StartActivityOptionsV1.heartbeat_timeout` carries the deadline the server will
enforce, forwarded from the original request. Send at **one third** of it, floored at 1s, and treat
an absent value as the server's 30s default — so the common case is a heartbeat every 10s. Three
beats per window means one dropped or delayed message can't trip the watchdog. The Rust
implementation is `Worker::heartbeat_interval`
([worker.rs:1350](../immortal-worker-lib/src/models/worker.rs#L1350)).

**Lifetime.** The heartbeat must start when the activity starts and stop when it settles — no
earlier, no later. A heartbeat loop that outlives its activity keeps a dead slot alive; one that
stops early gets the activity killed. The Rust worker gets this structurally by `select!`ing the
heartbeat loop against the activity future inside the same spawned task, so it is dropped on return
and aborted along with the task on kill or timeout.

**Superseded runs.** The server ignores a heartbeat whose `activity_run_id` is not the current
`latest_run_id` ([server.rs:492](../src/server.rs#L492)), so a straggler from a previous attempt
cannot keep a newer run alive. A heartbeat also clears a `Suspected` kill state back to `Healthy`,
retracting any escalation the watchdog had built up (§8.5).

### 6.2 Logs as a fallback

The `log_event` handler still refreshes `last_heartbeat` when the log carries an `activity_id`
([server.rs:555](../src/server.rs#L555)). This predates the real heartbeat and is retained so that a
worker built before `activity_heartbeat_v1` existed keeps working: in the Rust worker, `tracing`
spans around activity execution carry `workflow_id`/`activity_id`/`activity_run_id`, and a
`ChannelLayer` subscriber turns every event inside such a span into a `log_event`
([worker.rs:208-282](../immortal-worker-lib/src/models/worker.rs#L208)) — so ordinary logging
happened to keep activities alive.

**Do not rely on this in a new SDK.** It only fires if the activity logs, which is not something the
protocol can require. Send real heartbeats.

The Rust worker's `ChannelLayer` carries its own filter rather than inheriting the global
`EnvFilter`, so changing `RUST_LOG` cannot silence the log stream sent to the server. It is tuned
independently via **`IMMORTAL_LOG`**, defaulting to `info`. That default must stay at or below the
level of the workflow/activity `info_span!`s — a span that never registers produces no `SpanData`,
and events inside it are dropped.

The `Log` message requires `when` (**Unix seconds**, not millis — `DateTime::from_timestamp(log.when, 0)`),
`message`, `workflow_id`, `level`, and optional `activity_id`, `activity_run_id`, and `metadata`
(JSON bytes). A log whose `when` does not parse is dropped entirely.

### 6.3 Workflows

Workflows have no liveness timeout. `workflow_heartbeat` is ignored, and running workflows are
reaped only via the orphan path on worker disconnect (§7.3). This is deliberate: a workflow is
mostly parked waiting on activities, and a wall-clock deadline on it would kill legitimately
long-running ones.

---

## 7. Workflows

### 7.1 Dispatch and epochs

`StartWorkflowOptionsV1` carries `workflow_type`, `input` (`Payloads`), `workflow_id`, `task_queue`,
and `epoch`. The server computes `epoch = previous_epoch + 1`, or `0` for a first run
([service.rs:1932](../src/service.rs#L1932)).

**The epoch is the resurrection counter.** Every re-dispatch of the same `workflow_id` — after a
worker died, after a server restart — increments it. The worker must thread the received epoch
through unchanged into every activity request and into the final result. The server uses it to
distinguish live work from a stale prior attempt.

If the workflow type is not registered, the Rust worker immediately reports a failed result rather
than dropping the action ([worker.rs:774](../immortal-worker-lib/src/models/worker.rs#L774)).

### 7.2 Completion

`CompletedWorkflow` (unary) with `WorkflowResultV1`: `workflow_id`, `worker_id`,
`worker_instance_id`, `epoch`, and a status of `completed` / `failed` / `cancelled` / `sleep`.
The Rust worker only ever emits `completed` and `failed`; a killed workflow is reported as `failed`
with message `workflow {id} cancelled`.

The server sleeps **1 second** before processing a workflow completion
([server.rs:972](../src/server.rs#L972)), to let in-flight activity results land in Redis first.
Expect that latency on `ExecuteWorkflow`.

### 7.3 Orphaning

When a worker's stream ends, every workflow the server has assigned to that `instance_id` is marked
`Orphaned { first_seen }` ([server.rs:808-824](../src/server.rs#L808)). If it is still orphaned
**10 seconds** later, the watchdog re-queues it for dispatch to any worker
([service.rs:1017](../src/service.rs#L1017)) — producing a new epoch. Separately, `resurrect()` runs
once 60s after server start and re-queues any workflow that Redis says is `Running` but which is not
in memory ([service.rs:2147](../src/service.rs#L2147)).

**Implication: workflow bodies re-run from the top on a new epoch.** Durability comes entirely from
activity dedup (§8.2), not from replay. Workflow code must be structured so that re-execution is
safe given that completed activities return cached results.

---

## 8. Activities

### 8.1 Two request paths

**A. Workflow-initiated (the normal path).** Workflow code calls `StartActivity` as a **unary RPC**
and blocks on the response ([workflow.rs:83](../immortal-worker-lib/src/models/workflow.rs#L83)). The
server queues it, dispatches it to some worker over *that* worker's stream, and only answers the
unary call when the activity finally settles. The call can therefore be outstanding for the entire
duration of the activity, including retries.

On retryable RPC errors the worker retries the whole `StartActivity` call with exponential backoff
(200ms, doubling, capped at 5s), waiting for reconnection first. This is safe **because of the
idempotency key** — a retried request attaches to the existing activity rather than starting a
second one.

**B. Server-dispatched.** The worker receives `start_activity` on its stream and runs it. This is
the execution side of path A, and is also how `Call` fallback works (§9).

### 8.2 Idempotency key and fingerprint

```
idempotency_key = "{workflow_id}:{epoch}:{activity_type}:{seq}"
fingerprint     = blake3(payload.data).to_hex()
```

`seq` is a per-`WfContext` counter starting at 0, incremented on every `activity()` call
([workflow.rs:88](../immortal-worker-lib/src/models/workflow.rs#L88)). The server uses
`idempotency_key` as the `activity_id`.

This is the entire durability mechanism. When a request arrives whose `activity_id` already exists
in history ([service.rs:1495-1638](../src/service.rs#L1495)):

| Last run status | Server response |
|---|---|
| `Completed` | Returns the stored output immediately. No re-execution. |
| `Failed` | Returns the stored failure immediately. |
| `Running` | Attaches this waiter to the existing run; both get the eventual result. |

Two constraints fall out of this, and both must be documented for SDK users:

1. **Activity call order must be stable across epochs.** `seq` is positional. If a re-run issues
   activities in a different order, keys shift and cached results are matched to the wrong calls.
   Conditional branches that depend on non-deterministic input are the hazard.
2. **The key does not include the input.** Same type at the same position with different arguments
   collides and returns the first call's result. `fingerprint` records the input hash for the
   timeline, but the server never compares it. If your SDK needs safety here, the natural fix is
   folding the fingerprint into the key — but that is a **protocol change**, and Rust and TS workers
   would then compute different keys for the same workflow. Do not do it unilaterally.

Note that `fingerprint` hashes serialized JSON bytes. Rust `serde` emits struct fields in
declaration order; `JSON.stringify` emits insertion order. These need not agree, so fingerprints are
only comparable within one language. Since nothing compares them today this is latent, not broken.

### 8.3 Retries

Retries are **server-side**. The worker never retries a failed activity itself.

- Max **3** attempts total, counted as `activity.runs.len()`
  ([service.rs:775](../src/service.rs#L775)).
- Backoff `1000ms * 2^(attempt-1)` plus jitter in `[0, 250]ms`
  ([service.rs:825](../src/service.rs#L825)).
- A retry reuses the same `activity_id` and appends a new run with a fresh `activity_run_id`.
- **`cancelled` is not retried** — it sets `failed = false` while still marking the run `Failed`
  ([service.rs:675](../src/service.rs#L675)). `failed` / `timeout` are retried.
- Only a completion for the **latest** run schedules a retry; stale results are absorbed.

The `retry_policy` field on `RequestStartActivityOptionsV1` is accepted, persisted, and **never
consulted**. The constants above are the real policy.

### 8.4 Completion

`CompletedActivity` (unary) with `ActivityResultV1`: `workflow_id`, `activity_id`,
`activity_run_id`, `workflow_epoch`, and one of `completed` / `failed` / `cancelled` / `timeout`.
Both IDs and the epoch must be echoed exactly as received.

`Completed` with a **null result payload** is treated as a failure
([service.rs:606](../src/service.rs#L606)) — send an encoded `null`, never an absent payload, for a
void activity.

### 8.5 Kill and timeout

`kill_activity` and `timeout_activity` both abort the running task and synthesize a result:

| Action | Synthesized result |
|---|---|
| `kill_activity` | `Cancelled` — terminal, no retry |
| `timeout_activity` | `Failed` (retryable, message `Activity Timeout`) — retried if attempts remain |

The watchdog escalates: it sends `timeout_activity` on each tick where the activity is over its
deadline, tracking a `KillState` of `Healthy → Suspected{attempts}`. After **3** attempts, or
immediately if the worker is unreachable, it finalizes the activity server-side with a `Timeout`
status without the worker's participation ([service.rs:1133-1186](../src/service.rs#L1133)).

> **This is the hardest requirement to port.** Rust aborts a `tokio` task at its next await point.
> Node cannot abort a running async function. A JS SDK can only offer cooperative cancellation via
> `AbortSignal`, or true termination via `worker_threads`. The protocol tolerates a worker that
> ignores kill entirely — the watchdog finalizes server-side after 3 attempts — but the activity
> keeps consuming a capacity slot and CPU in the worker until it finishes on its own.

---

## 9. Calls and notifications

**Calls** are the non-durable RPC path: no history, no epoch, no idempotency, no retry.
`start_call` carries `call_id`, `call_type`, `call_run_id`, `call_input` (a single `Payload`).
Reply with `CompletedCall` / `CallResultV1`.

Dispatch has a fallback the SDK must reproduce: if `call_type` is not in the registered **calls**
map, the Rust worker looks it up in the registered **activities** map and runs the activity as a
call, mapping `ActivityError` onto `CallError` ([worker.rs:1508](../immortal-worker-lib/src/models/worker.rs#L1508)).
Note it `unwrap()`s that second lookup — an unknown `call_type` panics the worker task. An SDK
should return a failed `CallResultV1` instead.

Timed-out calls get `kill_call` and are dropped from `running_calls` unconditionally; the client's
`Call` RPC then hangs until its own deadline.

**Notifications** are fire-and-forget. `notify` carries `notification_id`, `notification_type`,
`notification_input`. **There is no completion message and no result.** An unregistered
notification type is silently dropped ([worker.rs:1572](../immortal-worker-lib/src/models/worker.rs#L1572)).

---

## 10. The outbox

All three completion types go through a single durable-ish outbox rather than being sent inline
([worker.rs:1056-1126](../immortal-worker-lib/src/models/worker.rs#L1056)):

- A `VecDeque` drained by a dedicated task, woken by notify or a 500ms interval tick.
- The drain is skipped entirely while disconnected.
- On send failure the item is **pushed back to the front**, the task sleeps 500ms, and the drain
  stops for this round — preserving order and never dropping results.

**An SDK must implement this, not send completions inline.** It is what makes results survive a
server restart mid-activity, and it is what keeps capacity accounting from leaking (§5). The queue
is in-memory only; results are lost if the worker process dies. That is accepted — the workflow
re-runs at a new epoch.

Delivery is **at-least-once**. Duplicate completions are absorbed: `completed_activity_inner`
returns early if the run is already `Completed` or `Failed`
([service.rs:562](../src/service.rs#L562)).

---

## 11. Reconnection

The worker's outer loop ([worker.rs:738](../immortal-worker-lib/src/models/worker.rs#L738)):

1. Set connected = false.
2. Rebuild `RegisterImmortalWorkerV1` from *current* state — registered types plus whatever is in
   `running_workflows` / `running_activities` right now.
3. Open the stream, send it, set connected = true, kick the outbox.
4. Process actions until the stream ends or errors.
5. Sleep 1s. Repeat forever.

There is no backoff beyond the flat 1s and no give-up condition. In-flight workflows and activities
keep running across the gap; the `connected` watch channel gates `StartActivity` calls and the
outbox so they park rather than fail while disconnected.

---

## 12. Known gaps

Do not implement these. They are visible in the proto but inert.

| Feature | State |
|---|---|
| `Sleep` RPC | Server returns `Ok(())` without doing anything ([server.rs:235](../src/server.rs#L235)); `WfContext::sleep` returns `Ok(())` without calling it ([workflow.rs:76](../immortal-worker-lib/src/models/workflow.rs#L76)). Fully stubbed on both sides. |
| `sleep_workflow` action, `WorkflowResultV1.sleep` | Never sent, never handled. |
| `check_activity` / `check_workflow` | Server handles replies; nothing sends the requests. Dead on both sides. |
| `heartbeat`, `workflow_heartbeat`, `activity_heartbeat` (int32, field 3) | Ignored by the server. `activity_heartbeat_v1` (field 9) is the live one — §6. |
| `retry_policy` | Persisted, never read. §8.3 constants govern. |
| `ActivityCancellationType` | Set by the worker, never read by the server. |
| `WillCompleteAsync`, `DoBackoff` | Defined in the proto, unreferenced in both binaries. |
| `ImmortalServerless` service | `SERVERLESS_MODE` branch is commented out in `main_thread`. |

---

## 13. Minimum viable worker

To run activities and nothing else:

1. Open `RegisterWorker`; send `register_worker` with your activity schemas, empty `running_*`
   lists, and honest capacity.
2. Ignore every inbound action except `start_activity`, `kill_activity`, `timeout_activity`.
3. Run activities, respecting declared capacity.
4. While an activity runs, send `activity_heartbeat_v1` every `heartbeat_timeout / 3` (10s by
   default), starting and stopping exactly with the activity (§6).
5. Push every result through an outbox that retries and preserves order (§10).
6. Echo `workflow_id`, `activity_id`, `activity_run_id`, `workflow_epoch` back exactly.
7. On stream end: sleep 1s, reconnect, treating `Instance ID already registered` as retryable (§2).

Add workflow support (§7) once that is solid; it is the piece that requires care about epochs and
`seq` ordering.
