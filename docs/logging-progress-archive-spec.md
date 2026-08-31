# Immortal Logging, Activity Liveness, and Archive Specification

**Status:** proposed

**Audience:** maintainers of the Immortal server, worker SDKs, log UI/API, and deployment
infrastructure.

**Scope:** this specification defines how activity logs refresh the existing activity heartbeat
clock, how log traffic remains bounded and isolated from the worker control plane, how multiple
Immortal servers share live logs through Redis, and how logs are archived durably to S3.

Normative terms such as **MUST**, **SHOULD**, and **MAY** describe intended implementation
requirements. This is an aspirational design document; `docs/worker-protocol.md` remains the
description of the protocol that is currently implemented.

---

## 1. Summary of decisions

1. A log emitted from the active execution scope of an activity acts as an activity heartbeat.
2. The heartbeat clock is refreshed before the full log is queued or persisted. Redis, S3, the
   archiver, and the UI are never part of the activity-liveness decision.
3. Existing `LogEvent` messages continue to refresh `last_heartbeat` for backward compatibility.
4. No new activity-progress protocol message is introduced in this version. Progress mirroring is
   retained only as a backburner design option.
5. Worker registration, explicit heartbeats, logs, and metrics remain on the existing
   `RegisterWorker` RPC. They use bounded logical lanes with heartbeats and recent activity logs
   taking priority.
6. Every queue is bounded by both record count and bytes. Metrics, explicit heartbeat state, and
   activity-log overflow fallback use latest-value state rather than unbounded event queues.
7. The server's worker-ingress loop never awaits Redis or S3. It refreshes activity liveness and
   hands full logs to a bounded background writer.
8. Redis remains the shared, cluster-wide hot log store. Any Immortal server can receive a log and
   any Immortal server can serve it.
9. Durable archival uses fixed, sharded Redis Streams and an independent consumer-group archiver
   that writes immutable, compressed S3 segments.
10. The archive is at-least-once. Every log has a stable `event_id`, and consumers tolerate
    duplicates.

---

## 2. Goals and non-goals

### 2.1 Goals

- Preserve the current behavior in which activity logs act as heartbeats.
- Ensure a Redis, S3, archiver, or UI outage cannot cause a healthy activity to time out.
- Preserve compatibility with workers that only send `LogEvent`.
- Prevent log floods and storage outages from causing unbounded memory growth.
- Keep activity heartbeat handling and worker control responsive under log load.
- Support multiple Immortal server instances without workflow-to-server affinity for logs.
- Retain recent logs in Redis for low-latency streaming and pagination.
- Retain historical logs durably in S3.
- Make archived logs retrievable and deletable by workflow.
- Provide explicit failure policies and observable queue utilization.

### 2.2 Non-goals

- Exactly-once delivery across Redis and S3.
- Using an ordinary log as a recoverable activity checkpoint.
- Preserving every log during an arbitrarily long outage without a finite storage limit.
- Replacing all workflow/history persistence with S3.
- Solving every Redis Cluster multi-key issue in the existing history implementation.
- Adding a second log RPC in this protocol version.
- Treating a complete worker-to-server transport outage as proof that an activity is alive.

---

## 3. Terminology

- **Activity liveness signal:** An existing `LogEvent` or explicit activity heartbeat accepted for
  the current activity run.
- **Qualifying activity log:** A `LogEvent` carrying both `activity_id` and `activity_run_id` for
  the active execution scope. Only a qualifying, validated log can refresh `last_heartbeat`.
- **Worker lease:** Evidence that the worker process and control connection are alive. This does
  not prove that a particular activity is progressing.
- **Log persistence:** Successful storage of the full log record in Redis or S3. Persistence is not
  required to refresh `last_heartbeat`.
- **Hot log:** A recent log retained in a per-workflow Redis Stream for UI streaming and
  recent-history reads.
- **Archive log:** A log retained in a sharded Redis archive stream until it is written to S3 and
  acknowledged.

---

## 4. Required invariants

The implementation MUST preserve these invariants:

1. `last_heartbeat` is updated before attempting Redis or S3 persistence.
2. Redis and S3 I/O never run inline in the worker-ingress receive loop.
3. A full or closed log queue never blocks an activity thread or the worker control stream.
4. A log or explicit heartbeat is accepted only for the current activity run; an explicit
   heartbeat must also match the workflow epoch carried in that message.
5. A late log from a superseded run cannot keep the current run alive.
6. A log emitted after an activity has settled may be archived but cannot refresh its heartbeat.
7. Synthetic queue-overflow messages cannot refresh an activity heartbeat.
8. Memory consumption has a configured ceiling independent of outage duration.
9. An archive entry is acknowledged only after its S3 object has been stored successfully.
10. Worker, server, Redis, and archiver overload is observable through metrics and structured
    diagnostics.

---

## 5. Existing activity-heartbeat semantics

### 5.1 Independent clocks

The server tracks three independent clocks:

| Clock | Meaning | Used for activity timeout |
|---|---|---:|
| `last_worker_lease_at` | Worker/control connection is alive | No |
| `last_heartbeat` | Server received a current-run log or explicit activity heartbeat | Yes |
| `last_log_persisted_at` | A full log reached Redis or S3 | No |

Storage health MUST NOT influence `last_heartbeat`.

### 5.2 Existing heartbeat inputs

This version has exactly two activity-heartbeat inputs:

```text
LogEvent             A log was emitted from the active activity execution scope.
ActivityHeartbeatV1  Activity code called the existing explicit heartbeat API.
```

Both inputs refresh the same existing server-side `last_heartbeat` field.

All log levels accepted by the Immortal activity tracing layer act as heartbeats. A log filtered out
before reaching that layer does not.

### 5.3 Server-side heartbeat validation and refresh

This specification does not prescribe a new function signature or state transition. The two
existing message handlers validate the evidence available in their existing wire messages and then
mutate the same existing activity state.

Before either message refreshes a heartbeat, the server MUST:

1. Find the running activity by `activity_id`.
2. Verify that the message arrived on the worker connection currently assigned to the activity.
3. Verify that `workflow_id` matches the running activity.
4. Verify that `activity_run_id` equals the current `latest_run_id`.
5. Ignore stale, unknown, completed, or killed runs.

`ActivityHeartbeatV1` carries `workflow_epoch`, so its handler MUST verify that field as well.
`LogEvent` does not carry `workflow_epoch`; a matching current `activity_run_id` associates the log
with the server's stored epoch without adding a new wire field.

Although `LogEvent.activity_run_id` is optional in protobuf, it MUST be present for that log to
refresh an activity heartbeat. A log without it MAY still be persisted or archived, but cannot
safely keep an activity alive because the server cannot distinguish a superseded execution.

After those checks pass, the server MUST use its receipt time for `last_heartbeat` and clear
`KillState::Suspected`. The implementation SHOULD centralize this small state mutation so the two
handlers cannot drift, but that is an internal refactor rather than a new protocol concept.

Worker timestamps are diagnostic fields only. They MUST NOT be used as the server timeout clock.

### 5.4 Timeout behavior

The current effective heartbeat timeout behavior remains compatible with existing deployments,
including the existing default where one is applied. This specification does not change
start-to-close timeout semantics.

The existing activity-heartbeat deadline remains:

```text
last_heartbeat + effective_heartbeat_timeout
```

Start-to-close timeout, schedule-to-close timeout, worker disconnection, and heartbeat timeout are
separate conditions and SHOULD produce distinguishable failure reasons.

### 5.5 Detached and late logs

A late log MAY still enter the log archive, but the server MUST verify its current activity run ID
before refreshing `last_heartbeat`. Once the activity is removed from the running-activity map, its
logs cannot refresh liveness.

---

## 6. Backward compatibility and deferred progress mirroring

### 6.1 Existing workers

Existing workers send full `LogEvent` messages containing the activity and run IDs already present
in the protobuf. A new server MUST continue to refresh `last_heartbeat` as soon as a valid
current-run `LogEvent` is received and before queueing the full record for persistence.

This makes Redis, S3, archiver, and UI outages safe for existing workers whenever their full
`LogEvent` reaches server ingress. No new worker API or registration capability is required.

### 6.2 No new liveness protocol in this version

This specification does not add a message, local sequence, worker API, or capability negotiation.
The only activity-heartbeat inputs in the current scope are:

- A valid existing `LogEvent` received by the server.
- A valid existing explicit activity heartbeat.

Worker-side queue pressure is handled with a bounded latest-activity-log fallback described in
sections 7 and 9. The fallback sends an ordinary `LogEvent`, preserving wire compatibility.

### 6.3 Backburner: compact progress mirroring

A future protocol version MAY revisit a compact message, tentatively `ActivityProgressV1`, that
mirrors locally observed log progress without carrying the full log. That option could protect
activity liveness when a full-log payload cannot traverse the worker-to-server transport.

This idea is deliberately non-normative and outside the rollout plan. It requires a separate
proposal covering protobuf fields, capability negotiation, throttling, and compatibility. No part
of the current implementation may depend on it.

---

## 7. Worker transport and logical lanes

### 7.1 One RPC in this version

This version retains the existing `RegisterWorker` bidirectional RPC. A separate log RPC is not
required for the initial implementation.

The worker creates bounded logical lanes feeding the one request stream:

```text
registration       first frame; required
heartbeat token     latest explicit or log-derived heartbeat per active activity; highest priority
activity fallback  latest ordinary LogEvent per active activity; bounded priority reserve
logs               bounded general queue
metrics            latest sample only
```

The outbound scheduler processes lanes in this order:

1. Registration.
2. Latest activity heartbeat tokens, including compact tokens derived from valid activity logs.
3. Latest activity-log fallback records.
4. One bounded general-log batch.
5. The latest metrics sample when due.
6. Repeat.

Heartbeats and activity-log fallback records take priority, but the scheduler SHOULD send a general
log batch after draining current priority work so it cannot be starved indefinitely.

Every qualifying activity log first coalesces a compact heartbeat token containing only workflow,
activity, run, and epoch identity. This token uses the existing heartbeat wire message and does not
create a new worker API. It ensures an activity log queued behind bulk data still contributes fresh
liveness evidence immediately after reconnect. Detached and superseded runs cannot create tokens.

The activity fallback contains at most one ordinary `LogEvent` per active activity. It is used only
when the general queue cannot accept a qualifying activity log. A newer fallback log for the same
activity replaces the older one. Its count and bytes are included in the worker's hard memory
budget, and entries are removed when the activity settles.

### 7.2 Limits of a shared RPC

Logical priority and the activity fallback prevent old queued log records from routinely delaying
new activity-heartbeat evidence. They cannot provide a mathematical isolation guarantee once
frames are already inside HTTP/2 or TCP buffers.

If load testing later demonstrates transport-level contention, a separate client-streaming log RPC
MAY be introduced. A hard transport bulkhead would require a distinct connection, not merely a
second HTTP/2 stream on the same connection. That change is outside the current scope.

### 7.3 Batching

The worker SHOULD batch full logs by maximum records, maximum bytes, and maximum delay. The batch
builder itself is subject to the same byte budget and MUST NOT copy or serialize records without a
limit.

Initial tuning values are listed in section 9. They are defaults, not protocol constants.

---

## 8. Server ingress and persistence

### 8.1 Receive-loop responsibilities

The server's worker-ingress loop is a dispatcher. It MAY await short-lived in-memory locks needed
to validate and refresh an activity heartbeat, but MUST NOT await Redis, S3, an archive consumer,
or a WebSocket client.

For a full log, in pseudocode:

```rust
if validate_current_activity_log(&log, source_worker_instance_id) {
    running_activity.last_heartbeat = server_receipt_time;
    running_activity.clear_suspected_kill_state();
}

match log_queue.try_send(log) {
    Ok(()) => {}
    Err(Full(_)) => record_log_drop(...),
    Err(Closed(_)) => record_log_drop(...),
}
```

The heartbeat is refreshed even when the full log is oversized, rejected, or dropped because a
queue is full.

### 8.2 Background Redis writers

One or more background tasks consume the server log queue, batch Redis operations, and apply
bounded retry with jitter. Every Redis checkout and command has a deadline.

The current five-second timeout is a temporary safeguard. In the target architecture it remains a
writer-side defense and no longer delays the worker-ingress loop.

The writer reports persistence success through metrics. It never refreshes `last_heartbeat`;
otherwise delayed storage could extend a dead activity.

### 8.3 Worker stream termination

The end or failure of the worker-to-server request stream MUST be tied to worker deregistration or
an explicit orphan/reconnect grace path. The server MUST NOT leave a worker registered solely
because its server-to-worker response channel remains open.

A complete control-connection failure is not a logging outage. Activities owned by that worker are
classified as unreachable/orphaned under the worker recovery policy rather than as locally stuck.

---

## 9. Bounded-memory policy

### 9.1 Fundamental tradeoff

During an arbitrarily long storage outage, a system cannot guarantee all three of:

1. Bounded memory and disk.
2. Non-blocking activities.
3. Zero log loss.

Immortal prioritizes bounded resources and a responsive control plane. Logs are dropped according
to policy when all configured buffers are exhausted. Deployments requiring longer lossless outage
tolerance add a bounded disk spool or a durable external broker.

### 9.2 Worker log queue

The worker has one bounded full-log queue, not one queue per activity. It is bounded by both record
count and bytes.

Proposed initial defaults:

| Setting | Default |
|---|---:|
| Maximum queued records | 4,096 |
| Maximum queued bytes | 32 MiB |
| Maximum single record | 256 KiB |
| Maximum batch records | 256 |
| Maximum batch bytes | 1 MiB |
| Maximum batch delay | 200 ms |

A count-bounded `mpsc` queue is insufficient because log sizes vary. The worker SHOULD pair it with
a byte-budget semaphore or equivalent accounting. Byte permits are acquired with a non-blocking
operation and released when the record is sent or dropped.

The tracing layer MUST NOT wait for queue capacity.

### 9.3 Worker overflow policy

When the worker queue is full or a record is oversized:

1. A qualifying activity log is truncated to the maximum record size if necessary and offered to
   the bounded latest-activity-log fallback. Truncation may affect only payload fields such as the
   message and metadata; it MUST retain the activity and run IDs.
2. A newer fallback log for the same activity replaces the older fallback log.
3. Displaced or unaccepted full payloads are dropped.
4. Non-activity logs that cannot enter the general queue are dropped.
5. Drop counters are incremented by worker, activity, level, and reason.
6. When capacity recovers, the worker MAY emit one synthetic summary of dropped records.
7. The synthetic summary MUST NOT refresh an activity heartbeat.

The fallback is an overload safety mechanism, not a second copy of every log. The compact
log-derived heartbeat token carries liveness independently; the retained ordinary `LogEvent`
preserves the newest available payload.
The fallback cannot prove liveness during a complete worker-to-server transport outage; that
condition is handled as worker disconnection or orphaning.

Capacity MAY be partitioned so a small percentage remains reserved for `WARN` and `ERROR` logs.
Suggested degradation order:

1. Drop `TRACE`.
2. Drop `DEBUG`.
3. Sample repeated `INFO` records.
4. Use reserved capacity for `WARN` and `ERROR`.
5. Drop all levels at the hard byte ceiling.

No level is permitted to allocate unbounded memory.

### 9.4 Server queue

The server queue is also bounded by records and bytes. It SHOULD enforce:

- A global process limit.
- A per-worker allowance so one noisy worker cannot monopolize the server.
- A maximum single-record size before copying or parsing large metadata.
- A maximum Redis batch size.

Initial server limits SHOULD be derived from the deployment's memory limit. A reasonable starting
point is 64–128 MiB globally, with a substantially smaller per-worker allowance, followed by load
testing.

### 9.5 Latest-value state

The following data MUST NOT use event queues:

- Explicit heartbeat state: latest value per running activity.
- Activity-log overflow fallback: latest ordinary log per running activity.
- Metrics: latest sample per worker.

Overwriting a stale metric, heartbeat, or fallback log is expected behavior.

### 9.6 Completion outbox

Workflow and activity completion results are not logs and normally cannot be dropped. The existing
worker completion outbox also requires a resource policy:

- Bounded in-memory queue by records and encoded bytes.
- Backpressure completion producers when the queue is full, retaining capacity ownership until the
  result can be queued.
- Reject an individual result larger than the configured total outbox byte limit with a visible
  error so it cannot create an unbounded exception to the memory policy.
- Alert before either limit is reached.

This work is adjacent to, but not a prerequisite for, the log-pipeline changes.

---

## 10. Redis topology for clustered Immortal servers

### 10.1 Hot per-workflow streams

Recent logs remain available under per-workflow keys conceptually equivalent to:

```text
immortal:logs:workflow:<workflow_id>
```

These streams are ephemeral and bounded by time and/or length. They support:

- Live WebSocket subscriptions.
- Recent-history pagination.
- Cross-server reads when the workflow, worker stream, and UI request reach different Immortal
  server instances.

WebSocket producers MUST handle Redis errors without panicking. A transient failed `XREAD` causes a
bounded retry and does not permanently end the subscription producer.

### 10.2 Archive-ingestion streams

Add a fixed number of archive streams:

```text
immortal:logs:archive:v1-s32:{00}
immortal:logs:archive:v1-s32:{01}
...
immortal:logs:archive:{31}
```

The shard is selected by a stable hash of `workflow_id`. Every log for a workflow therefore enters
one ordered archive stream.

The number of shards is configurable but stable for a given archive version. Changing the shard
count creates a new archive stream version rather than silently changing the mapping.

### 10.3 Dual-write order

The background server writer performs:

1. `XADD` to the archive shard.
2. Best-effort `XADD` to the per-workflow hot stream.

Archive-first behavior prioritizes durable handoff over live display. The two writes are not
required to be atomic. Both records carry the same `event_id`, making retry and deduplication safe.

If the deployment chooses hot-first behavior for latency, that choice MUST be explicit and its
additional archive-loss window documented.

### 10.4 Retention and trimming

The current hot-stream `MAXLEN ~1000` behavior MUST NOT be copied to archive streams. An archive
entry remains available until it has been delivered to S3 and acknowledged, subject to a hard
operational retention ceiling.

Archive trimming SHOULD be acknowledgement-aware where the Redis version supports it. Otherwise,
the trim point must remain below both:

- The oldest pending consumer-group entry.
- A configured safety window.

Proposed initial safety retention is 24–72 hours, sized from expected peak log volume and the Redis
memory budget. Alerts fire well before the hard ceiling.

Redis used as the archive handoff MUST have persistence, replication, capacity monitoring, and an
eviction policy that cannot arbitrarily remove archive entries or workflow state.

### 10.5 Redis Cluster note

Multiple Immortal application servers sharing one Redis deployment is distinct from Redis Cluster
hash-slot sharding. The fixed archive streams are compatible with Redis Cluster when consumers
read shards independently.

Existing commands that read multiple per-workflow keys in one `XREAD` may cross hash slots. A true
Redis Cluster deployment must group reads by slot or use independent reads. The broader history-key
layout requires a separate Redis Cluster compatibility review.

---

## 11. Canonical log envelope

Every full log uses a versioned internal envelope:

```json
{
  "schema_version": 1,
  "event_id": "worker-instance:connection-generation:sequence",
  "namespace": "default",
  "workflow_id": "...",
  "workflow_epoch": 3,
  "activity_id": "...",
  "activity_run_id": "...",
  "task_queue": "...",
  "worker_id": "...",
  "worker_instance_id": "...",
  "source_server_id": "...",
  "level": "info",
  "message": "...",
  "metadata": {},
  "emitted_at": "2026-08-30T20:00:00Z",
  "ingested_at": "2026-08-30T20:00:01Z"
}
```

Requirements:

- `event_id` is stable across retries and unique for the worker process.
- `workflow_epoch` is server-enriched when the log matches a known activity run and is otherwise
  nullable; it is not added to the existing `LogEvent` wire message in this version.
- `emitted_at` is provided by the worker and used for display and diagnostics.
- `ingested_at` is assigned by the receiving server and used for archive partitioning.
- Timeout logic uses neither timestamp; it uses the server receipt time assigned to
  `last_heartbeat`.
- Unknown schema fields are preserved where practical.
- Maximum sizes are enforced for message, metadata, details, and the complete encoded record.
- Namespace or tenant identity is included before multi-tenancy is introduced so archives need not
  be migrated later.

---

## 12. S3 archiver

### 12.1 Service topology

The archiver is a separate `LogArchiverService` module launched and supervised by the Immortal
server process when `IMMORTAL_LOG_ARCHIVER_ENABLED=true`. It does not run storage work on request
tasks and owns one reconnecting task and Redis connection per shard. Multiple Immortal server
replicas with the service enabled share one Redis consumer group, for example:

```text
consumer group: immortal-s3-archiver-v1
consumer name:  <instance-id>
```

Each enabled server instance:

1. Reads new archive entries with `XREADGROUP`.
2. Reclaims abandoned pending entries with `XAUTOCLAIM` after a configured idle period.
3. Validates and groups records into bounded workflow buffers.
4. Compresses and uploads immutable S3 objects.
5. Calls `XACK` only after a successful upload.
6. Retries S3 failures with capped exponential backoff and jitter.

The archiver has its own count and byte limits. The number of simultaneously open workflow buffers
is bounded; an LRU policy flushes a buffer before opening more.

### 12.2 Delivery semantics

Delivery is at-least-once. A crash after S3 upload but before `XACK` can cause a second upload.

Readers MUST deduplicate by `event_id`. S3 object keys SHOULD include the first and last event
identifiers plus a content hash so identical retry batches naturally converge where possible.

Exactly-once delivery would require a transactional manifest shared by Redis and S3 and is not a
goal.

### 12.3 Object format and layout

The initial canonical format is newline-delimited JSON compressed with gzip. It is simple to
recover, stream, inspect, and evolve.

Proposed object layout:

```text
s3://<bucket>/raw/v1/
  tenant=<tenant>/
  workflow_bucket=<first-two-workflow-hash-bytes>/
  workflow_hash=<sha256-workflow-id>/
  dt=2026-08-30/
  hour=20/
  part-<first-event>-<last-event>-<content-hash>.jsonl.gz
```

The original workflow ID is stored inside every record. User-controlled identifiers are not used
directly as S3 path components.

Grouping raw objects by workflow provides:

- Direct retrieval without scanning unrelated workflows.
- Straightforward deletion when workflow history is deleted.
- Bounded object sizes suitable for paginated UI reads.

### 12.4 Flush policy

An object is flushed when any configured condition is met:

- Uncompressed size reaches approximately 8–32 MiB.
- Maximum buffer age reaches approximately one minute.
- The workflow completes and its buffer is non-empty.
- The workflow buffer is evicted by the bounded LRU policy.
- The archiver is shutting down gracefully.

These values are deployment defaults and must be tuned from observed volume. The archiver MUST NOT
create one S3 object per log.

Normal `PutObject` is sufficient for these segment sizes. Multipart upload is reserved for future
large compaction jobs, with a lifecycle rule to abort incomplete multipart uploads.

### 12.5 Security, retention, and deletion

The bucket SHOULD enable:

- Default server-side encryption.
- Least-privilege IAM scoped to the archive prefix.
- Versioning when recovery from accidental overwrites/deletes is required.
- Lifecycle transitions and expiration matching product retention policy.
- Audit logging appropriate to the deployment.

S3 Object Lock MAY be enabled for compliance tenants. Because Object Lock deliberately prevents
deletion during retention, it conflicts with ordinary workflow deletion and must be an explicit
policy choice.

Deleting workflow history SHOULD delete or tombstone its hot Redis key and delete its raw S3
workflow prefix unless retention or Object Lock policy forbids it.

### 12.6 Optional disk spool

Deployments requiring greater tolerance than the Redis retention window MAY add an append-only,
bounded disk spool to the server or archiver.

The spool has explicit maximum bytes, segment size, and full-disk behavior. Reaching the spool limit
must never cause unbounded memory growth. The deployment chooses whether to drop oldest or newest
logs at the hard limit.

### 12.7 Firehose and analytics

Amazon Data Firehose MAY be used later for a derived, time-partitioned analytics copy. It is not the
initial canonical archive because per-workflow retrieval and deletion are first-class requirements,
and workflow-ID dynamic partitioning can create excessive active partitions and small objects.

An optional compaction job MAY convert raw JSONL segments into Parquet organized by date, hour, and
a modest shard count for Athena or other analytical engines. The Parquet data is derived; raw
per-workflow objects remain canonical.

---

## 13. Hot and cold reads

### 13.1 Tiered API

The log API evolves from Redis-only reads to a tiered reader:

1. Recent range: per-workflow Redis Stream.
2. Archived range: workflow S3 prefix.
3. Boundary range: merge Redis and S3 records and deduplicate by `event_id`.

### 13.2 Cursor

A permanent API cursor MUST NOT be only a Redis stream ID. The external cursor is based on a stable
ordering tuple such as:

```text
(emitted_at, event_id)
```

The encoded cursor MAY also carry internal tier/object position as an optimization, but clients do
not depend on it.

Ordering ties and retry duplicates are resolved by `event_id`. The API documents whether display
order is worker emission time or server ingestion time.

### 13.3 Live subscriptions

Live WebSocket subscriptions continue to read hot Redis streams so they work from any Immortal
server. Subscription producers use bounded retry and cancellation and MUST NOT panic on a transient
Redis error.

---

## 14. Failure behavior

| Failure | Required behavior |
|---|---|
| Redis command stalls | Deadline expires in background writer; ingress and heartbeat handling continue |
| Redis unavailable | Server queue fills to its cap; full logs eventually drop or spool; heartbeat handling continues |
| S3 unavailable | Archiver leaves entries pending and retries within bounded resources |
| Archiver crashes before upload | Another consumer reclaims the pending entries |
| Archiver crashes after upload, before `XACK` | Records may be uploaded twice; readers deduplicate |
| Worker log queue full | Latest qualifying activity log uses the bounded fallback; older/full payloads may drop |
| Server log queue full | A valid received log refreshes `last_heartbeat`; its payload drops according to policy |
| Oversized log | A valid received log refreshes `last_heartbeat`; its full payload is rejected and measured |
| UI/WebSocket reader fails | No effect on worker execution or archival |
| Full worker control connection fails | Worker becomes disconnected/orphaned under recovery policy |
| Activity stops logging and explicitly heartbeating | Activity heartbeat timeout proceeds normally |
| Late log from old run | Log may archive; heartbeat refresh is rejected |
| Synthetic dropped-log summary | May archive; never refreshes an activity heartbeat |

---

## 15. Observability and operational limits

At minimum, expose:

### Worker

- Log queue records and bytes.
- Queue utilization high-water marks.
- Dropped logs by level and reason.
- Oversized logs.
- Explicit heartbeat state and send latency.
- Activity-log fallback slots, bytes, replacements, and send latency.
- Time since last successful worker-to-server send.
- Completion outbox records and bytes.

### Immortal server

- Global and per-worker log queue records and bytes.
- Ingress-to-heartbeat-refresh latency.
- Redis checkout, `XADD`, and `EXPIRE` latency and timeout counts.
- Hot-write and archive-write success/failure counts.
- Logs dropped by worker, level, and reason.
- Worker request-stream termination reasons.
- Active activities by heartbeat age.

### Archiver

- Consumer-group lag by shard.
- Oldest unarchived entry age.
- Pending entry count and reclaim count.
- Open workflow buffers and total buffered bytes.
- S3 object count, bytes, latency, retries, and failures.
- Duplicate/replayed event count when detectable.
- Time from server ingestion to S3 acknowledgement.

Alerts SHOULD cover queue utilization, drop rate, Redis timeout rate, oldest unarchived age, S3
failure duration, and completion-outbox capacity.

---

## 16. Rollout plan

### Phase 0: temporary protection

- Retain the existing Redis log-persistence deadline.
- Add metrics for timeout frequency.

### Phase 1: server ingress isolation

- Centralize the existing `last_heartbeat` mutation behind the two validated message handlers.
- Validate the source worker, workflow ID, and current activity run ID before a log refreshes
  `last_heartbeat`; validate the epoch on explicit heartbeats, which carry that field.
- Move Redis I/O into a bounded background queue.
- Make stream termination remove or orphan the worker correctly.
- Make WebSocket Redis reads recoverable rather than panic-driven.

This phase is compatible with all existing workers.

### Phase 2: bounded worker lanes

- Replace the single mixed broadcast behavior with logical explicit-heartbeat,
  latest-activity-log fallback, general-log, and metrics lanes.
- Add count and byte limits, maximum record size, batching, and drop summaries.
- Keep the existing `RegisterWorker` RPC.

### Phase 3: archive streams

- Add stable `event_id` and the canonical envelope.
- Add fixed archive shards and archive-first dual writes.
- Configure consumer group, retention, trimming, and lag metrics.

### Phase 4: S3 archiver

- Implement bounded consumer buffers, `XAUTOCLAIM`, gzip JSONL, `PutObject`, and post-upload
  `XACK`.
- Add lifecycle, encryption, IAM, and failure metrics.

### Phase 5: tiered reads and deletion

- Add S3 workflow-prefix reads.
- Introduce a stable cross-tier cursor.
- Merge and deduplicate Redis/S3 boundaries.
- Extend workflow deletion and retention behavior to S3.

### Phase 6: optional analytics

- Add Parquet compaction or a Firehose analytics copy if required.

---

## 17. Acceptance and chaos tests

The implementation is not complete until these cases are automated.

### 17.1 Heartbeat correctness

- A valid current-run log refreshes `last_heartbeat`.
- A stale run ID does not refresh `last_heartbeat`.
- A stale workflow epoch does not refresh `last_heartbeat`.
- A late log after completion archives but does not refresh `last_heartbeat`.
- An explicit heartbeat and a log refresh the same server-side heartbeat deadline.
- A synthetic drop summary does not refresh `last_heartbeat`.
- Worker/server clock skew does not cause an incorrect timeout.

### 17.2 Persistence isolation

- Blackhole Redis for longer than the activity heartbeat timeout while an activity logs.
- Hold every Redis pool connection.
- Make `XADD` and `EXPIRE` exceed their deadlines.
- Fill the server log queue.
- Stop every archiver replica.
- Reject S3 uploads for longer than the flush interval.

In every storage-outage case, server ingress remains responsive and an activity remains healthy
while its full `LogEvent` messages reach server ingress, regardless of downstream persistence
failure.

When the general worker log queue is full but the transport remains drainable, the latest bounded
activity-log fallback must reach the server as an ordinary `LogEvent` and refresh `last_heartbeat`.

### 17.3 Bounded resources

- Generate maximum-sized logs faster than the network and Redis can consume them.
- Verify worker and server resident memory plateau at configured ceilings.
- Verify metric samples do not accumulate.
- Verify activity-log fallback and explicit heartbeat state remain proportional to active activity
  capacity, not log count.
- Verify oversized logs are rejected before large unbounded copies.
- Verify overload drop summaries remain bounded.

### 17.4 Archive recovery

- Kill an archiver before upload and verify pending reclaim.
- Kill it after upload but before `XACK` and verify duplicate-safe reads.
- Restart all archivers and verify the oldest pending entries are processed.
- Exercise archive retention near its limit and verify alerts precede trimming.
- Delete a workflow and verify hot and archive behavior follows retention policy.

### 17.5 Multi-server behavior

- Send a log through server A and read it through server B.
- Restart the server owning the worker stream while another server serves log history.
- Run multiple archiver replicas and verify every archive event is acknowledged.
- Where Redis Cluster is supported, distribute archive shards across slots and verify independent
  consumption.

---

## 18. Initial configuration surface

Names are illustrative and finalized during implementation:

```text
IMMORTAL_WORKER_LOG_MAX_RECORDS
IMMORTAL_WORKER_LOG_MAX_BYTES
IMMORTAL_WORKER_LOG_MAX_RECORD_BYTES
IMMORTAL_WORKER_ACTIVITY_LOG_FALLBACK_MAX_BYTES
IMMORTAL_WORKER_LOG_BATCH_RECORDS
IMMORTAL_WORKER_LOG_BATCH_BYTES
IMMORTAL_WORKER_LOG_BATCH_DELAY_MS

IMMORTAL_SERVER_LOG_MAX_RECORDS
IMMORTAL_SERVER_LOG_MAX_BYTES
IMMORTAL_SERVER_LOG_MAX_BYTES_PER_WORKER
IMMORTAL_LOG_REDIS_TIMEOUT_MS

IMMORTAL_LOG_ARCHIVE_ENABLED
IMMORTAL_LOG_ARCHIVER_ENABLED
IMMORTAL_LOG_ARCHIVE_SHARDS
IMMORTAL_LOG_ARCHIVE_MAX_RECORDS_PER_SHARD
IMMORTAL_LOG_ARCHIVE_BATCH_RECORDS
IMMORTAL_LOG_ARCHIVE_BATCH_BYTES
IMMORTAL_LOG_ARCHIVE_CLAIM_IDLE_MS
IMMORTAL_LOG_ARCHIVE_REQUEST_TIMEOUT_MS

IMMORTAL_LOG_S3_BUCKET
IMMORTAL_LOG_S3_PREFIX
IMMORTAL_LOG_S3_REGION
```

All size and count values MUST be validated at startup. Invalid or unsafe combinations fail startup
with an actionable error rather than silently disabling limits.

---

## 19. Deferred decisions

The following decisions may be made from load and operational testing without invalidating the
architecture:

- Exact worker and server queue defaults.
- Number of archive shards.
- Hot Redis retention duration and length.
- Whether `WARN`/`ERROR` receive reserved capacity.
- Whether the server or worker gains a bounded disk spool first.
- The reconnect grace policy for unreachable workers.
- Whether to revisit compact, optional activity-progress mirroring.
- When to add a separate physical log RPC/connection.
- Whether archived logs require Object Lock.
- Whether a Parquet analytics representation is needed.

None of these decisions may remove the core invariants: `last_heartbeat` is refreshed before
persistence, control-plane work is isolated from storage, and every buffer is explicitly bounded.
