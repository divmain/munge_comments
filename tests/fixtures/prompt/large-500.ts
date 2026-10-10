/**
 * Job scheduling for the ingestion workers of the document pipeline service.
 *
 * Jobs arrive from the public HTTP API, the nightly backfill and the operator replay tool, are persisted in
 * the `jobs` table, and are leased to workers in priority order. A lease is a time-bounded claim on one job:
 * a worker that crashes or is partitioned away simply lets its lease expire, and another worker takes over.
 */

import { EventEmitter } from "node:events";
import { createHash, randomUUID } from "node:crypto";
import type { Pool, PoolClient, QueryResult, QueryResultRow } from "pg";
import { type Logger, childLogger, redactPayloadForLogging } from "./observability/logging";
import { type Metrics, type Histogram, type Counter, type Gauge } from "./observability/metrics";
import { type RetryPolicy, exponentialBackoff, withJitter } from "./reliability/retry";
import { type Clock, systemClock } from "./reliability/clock";

// Leases shorter than fifteen seconds caused constant lease churn on the slow EU shards in 2023.
export const MIN_LEASE_MS = 15_000;

// Upper bound agreed with the storage team: longer leases hide dead workers for too long.
export const MAX_LEASE_MS = 10 * 60 * 1000;

// Postgres caps a single statement at 65535 bind parameters; we stay well below that limit.
const MAX_BATCH_PARAMETERS = 30_000;

const DEFAULT_PRIORITY = 100;
const HEARTBEAT_FRACTION = 0.4;
const MAX_ATTEMPTS = 8;
const LEASE_SCAN_LIMIT = 32;

export type JobState = "queued" | "leased" | "succeeded" | "failed" | "dead";

export interface JobPayload {
  readonly source: "api" | "backfill" | "replay";
  readonly tenantId: string;
  readonly documentIds: readonly string[];
  // Free-form hints passed through from the API; never trusted for authorization decisions.
  readonly hints?: Readonly<Record<string, string>>;
}

export interface JobRow {
  id: string;
  state: JobState;
  priority: number; // lower runs first; the API maps "interactive" to 10 and "bulk" to 200
  attempts: number; // failed attempts so far, including leases that expired while held
  payload: JobPayload;
  leaseOwner: string | null; // worker id of the current lease holder, null unless state is "leased"
  leaseExpiresAt: Date | null; // for queued jobs this doubles as "not visible before" after a retry
  createdAt: Date;
  updatedAt: Date;
  // Content hash of the payload, used by the partial unique index to collapse duplicate submissions.
  dedupeKey: string;
}

export interface SchedulerOptions {
  readonly pool: Pool;
  readonly logger: Logger;
  readonly metrics: Metrics;
  readonly clock?: Clock; // injectable so the lease-expiry tests can run without real sleeps
  readonly leaseMs?: number; // clamped to [MIN_LEASE_MS, MAX_LEASE_MS]; defaults to twice the minimum
  readonly retry?: RetryPolicy; // delay before a failed job becomes visible to workers again
  // Tenants whose jobs bypass the fair-share limiter entirely (internal tooling and the replay service).
  readonly privilegedTenants?: ReadonlySet<string>;
  // Upper bound on concurrent leases per ordinary tenant; see the constructor for the default's origin.
  readonly perTenantLeaseLimit?: number;
}

export interface LeaseResult {
  readonly job: JobRow;
  readonly token: string;
  readonly expiresAt: Date;
}

export class LeaseLostError extends Error {
  constructor(readonly jobId: string, readonly owner: string) {
    super(`the lease on job ${jobId} is no longer held by worker ${owner}; another worker may own it now`);
    this.name = "LeaseLostError";
  }
}

export class DuplicateJobError extends Error {
  constructor(readonly existingId: string, readonly dedupeKey: string) {
    super(`an equivalent job is already queued or running: ${existingId} (dedupe key ${dedupeKey})`);
    this.name = "DuplicateJobError";
  }
}

/**
 * Computes the dedupe key for a payload: a SHA-256 over the tenant and the sorted document ids.
 *
 * Document ids are sorted first so that two submissions listing the same documents in a different order
 * collapse into a single job. The NUL separators keep `["ab", "c"]` and `["a", "bc"]` from colliding.
 */
export function dedupeKeyFor(payload: JobPayload): string {
  const hash = createHash("sha256");
  hash.update(payload.tenantId, "utf8").update("\u0000");
  // Sorting a copy: callers keep their own ordering, which the API echoes back to the client verbatim.
  const sortedIds = [...payload.documentIds].sort((left, right) => (left < right ? -1 : left > right ? 1 : 0));
  for (const documentId of sortedIds) {
    hash.update(documentId, "utf8").update("\u0000");
  }
  // Hints deliberately do not participate in the key: they never change the work a job performs.
  return hash.digest("hex");
}

function clampLease(requestedMs: number | undefined): number {
  const valueMs = requestedMs ?? 2 * MIN_LEASE_MS;
  return Math.min(MAX_LEASE_MS, Math.max(MIN_LEASE_MS, Math.round(valueMs)));
}

function rowToJob(row: QueryResultRow): JobRow {
  return {
    id: String(row.id),
    state: row.state as JobState,
    priority: Number(row.priority),
    attempts: Number(row.attempts),
    payload: row.payload as JobPayload,
    leaseOwner: (row.lease_owner as string | null | undefined) ?? null,
    leaseExpiresAt: (row.lease_expires_at as Date | null | undefined) ?? null,
    createdAt: row.created_at as Date,
    updatedAt: row.updated_at as Date,
    dedupeKey: String(row.dedupe_key),
  };
}

/**
 * Fair-share limiter: caps how many leases a single tenant may hold at once, so that one tenant's large
 * backfill cannot starve the interactive API traffic of every other tenant sharing the worker fleet.
 *
 * The counts live in process memory only. Each scheduler instance enforces its own share, which is why the
 * default limit is divided by the expected number of scheduler replicas in the deployment manifest.
 */
export class FairShareLimiter {
  private readonly heldLeasesByTenant = new Map<string, number>();

  constructor(private readonly perTenantLimit: number, private readonly privileged: ReadonlySet<string> = new Set()) {}

  tryAcquire(tenantId: string): boolean {
    if (this.privileged.has(tenantId)) {
      return true;
    }
    const currentlyHeld = this.heldLeasesByTenant.get(tenantId) ?? 0;
    if (currentlyHeld >= this.perTenantLimit) {
      return false;
    }
    this.heldLeasesByTenant.set(tenantId, currentlyHeld + 1);
    return true;
  }

  release(tenantId: string): void {
    if (this.privileged.has(tenantId)) {
      return;
    }
    const currentlyHeld = this.heldLeasesByTenant.get(tenantId) ?? 0;
    // A release without a matching acquire means a bookkeeping bug elsewhere. We clamp at zero instead of
    // throwing, because the limiter must never wedge a worker that is trying to report a finished job.
    if (currentlyHeld <= 1) {
      this.heldLeasesByTenant.delete(tenantId);
    } else {
      this.heldLeasesByTenant.set(tenantId, currentlyHeld - 1);
    }
  }
}

export class JobScheduler extends EventEmitter {
  private readonly pool: Pool;
  private readonly log: Logger;
  private readonly clock: Clock;
  private readonly leaseMs: number;
  private readonly retry: RetryPolicy;
  private readonly limiter: FairShareLimiter;
  private readonly leaseLatencyMs: Histogram;
  private readonly leaseConflicts: Counter;
  private readonly deadLettered: Counter;
  private readonly queueDepth: Gauge;
  private stopped = false;

  constructor(options: SchedulerOptions) {
    super();
    this.pool = options.pool;
    this.log = childLogger(options.logger, { component: "scheduler", leaseMs: clampLease(options.leaseMs) });
    this.clock = options.clock ?? systemClock;
    this.leaseMs = clampLease(options.leaseMs);
    this.retry = options.retry ?? withJitter(exponentialBackoff({ baseMs: 500, maxMs: 60_000 }), 0.2);
    // Eight concurrent leases per tenant met the p99 API latency target in the March load test with three
    // scheduler replicas; revisit the number whenever the worker count or the replica count changes.
    this.limiter = new FairShareLimiter(options.perTenantLeaseLimit ?? 8, options.privilegedTenants);
    this.leaseLatencyMs = options.metrics.histogram("scheduler_lease_latency_ms", { buckets: [5, 20, 100, 500] });
    this.leaseConflicts = options.metrics.counter("scheduler_lease_conflicts_total");
    this.deadLettered = options.metrics.counter("scheduler_dead_lettered_total");
    this.queueDepth = options.metrics.gauge("scheduler_queue_depth");
  }

  /**
   * Enqueues a job unless an equivalent job (same dedupe key) is already queued or running.
   *
   * @param payload - what to process; validated by the API layer before it reaches the scheduler
   * @param priority - lower numbers run first; ties are broken by submission time, oldest first
   * @throws DuplicateJobError when a live job already has the same dedupe key
   */
  async enqueue(payload: JobPayload, priority: number = DEFAULT_PRIORITY): Promise<JobRow> {
    const dedupeKey = dedupeKeyFor(payload);
    const now = this.clock.now();
    // The partial unique index covers only the live states, so a finished job with the same key does not
    // block a resubmission: re-running a completed import is a legitimate request from the operators.
    const result = await this.pool.query(
      `INSERT INTO jobs (id, state, priority, attempts, payload, dedupe_key, created_at, updated_at)
       VALUES ($1, 'queued', $2, 0, $3, $4, $5, $5)
       ON CONFLICT (dedupe_key) WHERE state IN ('queued', 'leased') DO NOTHING
       RETURNING *`,
      [randomUUID(), priority, payload, dedupeKey, now],
    );
    if (result.rowCount === 0) {
      const existing = await this.findLiveByKey(dedupeKey);
      // The conflicting job may have finished between the two statements; if so, simply try again.
      if (existing === null) {
        return this.enqueue(payload, priority);
      }
      throw new DuplicateJobError(existing.id, dedupeKey);
    }
    const job = rowToJob(result.rows[0]);
    this.log.debug("job enqueued", { jobId: job.id, payload: redactPayloadForLogging(payload) });
    this.emit("enqueued", job);
    return job;
  }

  /**
   * Enqueues many jobs with as few statements as the bind-parameter limit allows. Duplicates are skipped
   * silently rather than reported, so the returned array holds only the jobs this call actually created.
   */
  async enqueueMany(payloads: readonly JobPayload[], priority: number = DEFAULT_PRIORITY): Promise<JobRow[]> {
    const columnsPerRow = 6;
    const rowsPerStatement = Math.floor(MAX_BATCH_PARAMETERS / columnsPerRow);
    const created: JobRow[] = [];
    for (let batchStart = 0; batchStart < payloads.length; batchStart += rowsPerStatement) {
      const batch = payloads.slice(batchStart, batchStart + rowsPerStatement);
      const now = this.clock.now();
      const values: unknown[] = [];
      const tuples = batch.map((payload, index) => {
        const base = index * columnsPerRow;
        values.push(randomUUID(), priority, payload, dedupeKeyFor(payload), now, now);
        return `($${base + 1}, 'queued', $${base + 2}, 0, $${base + 3}, $${base + 4}, $${base + 5}, $${base + 6})`;
      });
      // Two payloads inside one batch may share a dedupe key; ON CONFLICT handles that case as well, since
      // Postgres checks each inserted row against the rows inserted earlier in the same statement.
      const result = await this.pool.query(
        `INSERT INTO jobs (id, state, priority, attempts, payload, dedupe_key, created_at, updated_at)
         VALUES ${tuples.join(", ")}
         ON CONFLICT (dedupe_key) WHERE state IN ('queued', 'leased') DO NOTHING
         RETURNING *`,
        values,
      );
      created.push(...result.rows.map(rowToJob));
    }
    return created;
  }

  /**
   * Leases the highest-priority queued job this worker is allowed to take, or returns null when nothing is
   * eligible right now. Ties on priority go to the oldest job, so that a steady stream of fresh submissions
   * can never starve older work indefinitely.
   *
   * Jobs whose lease has expired are eligible again: their previous holder is presumed dead. Jobs whose
   * tenant is at its fair-share limit are skipped, not waited on, and stay queued for the next poll.
   */
  async lease(workerId: string): Promise<LeaseResult | null> {
    const startedAtMs = this.clock.now().getTime();
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      // SKIP LOCKED lets concurrent workers scan past the rows another worker is busy claiming, instead of
      // queueing up behind its row locks; without it, lease latency grew linearly with the fleet size.
      const candidates = await client.query(
        `SELECT * FROM jobs
          WHERE (state = 'queued' AND (lease_expires_at IS NULL OR lease_expires_at <= $1))
             OR (state = 'leased' AND lease_expires_at < $1)
          ORDER BY priority ASC, created_at ASC
          LIMIT ${LEASE_SCAN_LIMIT}
          FOR UPDATE SKIP LOCKED`,
        [this.clock.now()],
      );
      for (const row of candidates.rows) {
        const job = rowToJob(row);
        if (!this.limiter.tryAcquire(job.payload.tenantId)) {
          continue;
        }
        const leased = await this.claim(client, job, workerId);
        await client.query("COMMIT");
        this.leaseLatencyMs.observe(this.clock.now().getTime() - startedAtMs);
        return leased;
      }
      // Every candidate was either locked by another worker or over its tenant's share. Committing (rather
      // than rolling back) releases the row locks just the same and keeps the transaction metrics honest.
      await client.query("COMMIT");
      return null;
    } catch (error) {
      await client.query("ROLLBACK");
      throw error;
    } finally {
      client.release();
    }
  }

  private async claim(client: PoolClient, job: JobRow, workerId: string): Promise<LeaseResult> {
    const expiresAt = new Date(this.clock.now().getTime() + this.leaseMs);
    const token = randomUUID();
    // An expired lease counts as a failed attempt: the previous worker most likely crashed in the middle of
    // the job, and counting it is what eventually moves a poison job that kills workers into the DLQ.
    const attempts = job.state === "leased" ? job.attempts + 1 : job.attempts;
    await client.query(
      `UPDATE jobs
          SET state = 'leased', lease_owner = $2, lease_token = $3, lease_expires_at = $4, attempts = $5, updated_at = $6
        WHERE id = $1`,
      [job.id, workerId, token, expiresAt, attempts, this.clock.now()],
    );
    return { job: { ...job, state: "leased", attempts, leaseOwner: workerId, leaseExpiresAt: expiresAt }, token, expiresAt };
  }

  /**
   * Extends a lease the worker still holds. The token, not the worker id, proves ownership: a worker that
   * lost its lease and then re-leased the same job must not be able to extend the stale lease by accident.
   */
  async heartbeat(lease: LeaseResult, workerId: string): Promise<LeaseResult> {
    const expiresAt = new Date(this.clock.now().getTime() + this.leaseMs);
    const result = await this.pool.query(
      `UPDATE jobs SET lease_expires_at = $3, updated_at = $4 WHERE id = $1 AND lease_token = $2 AND state = 'leased'`,
      [lease.job.id, lease.token, expiresAt, this.clock.now()],
    );
    if (result.rowCount === 0) {
      this.leaseConflicts.inc();
      throw new LeaseLostError(lease.job.id, workerId);
    }
    return { ...lease, expiresAt };
  }

  /** How long a worker should wait between two heartbeats under this scheduler's lease length. */
  heartbeatIntervalMs(): number {
    // Beating at 40% of the lease leaves room for one missed beat plus one slow database round trip before
    // the lease actually lapses; at 50% a single GC pause on a busy worker was enough to lose the lease.
    return Math.floor(this.leaseMs * HEARTBEAT_FRACTION);
  }

  async complete(lease: LeaseResult): Promise<void> {
    await this.finish(lease, "succeeded");
    this.emit("completed", lease.job);
  }

  /**
   * Records a failed attempt. The job is requeued with a backoff delay, or dead-lettered once it has used up
   * MAX_ATTEMPTS. Dead-lettered jobs are never retried automatically; the replay tool resubmits them by hand.
   */
  async fail(lease: LeaseResult, error: unknown): Promise<void> {
    const attempts = lease.job.attempts + 1;
    if (attempts >= MAX_ATTEMPTS) {
      await this.finish(lease, "dead");
      this.deadLettered.inc();
      this.log.warn("job dead-lettered after exhausting its attempts", { jobId: lease.job.id, attempts, error });
      this.emit("dead", lease.job);
      return;
    }
    const visibleAt = new Date(this.clock.now().getTime() + this.retry.delayFor(attempts));
    // Requeued jobs keep their original priority: bumping it would let one flaky tenant jump ahead of every
    // other tenant on each retry, which is exactly the starvation the fair-share limiter exists to prevent.
    const result = await this.pool.query(
      `UPDATE jobs
          SET state = 'queued', attempts = $3, lease_owner = NULL, lease_token = NULL, lease_expires_at = $4, updated_at = $5
        WHERE id = $1 AND lease_token = $2`,
      [lease.job.id, lease.token, attempts, visibleAt, this.clock.now()],
    );
    this.limiter.release(lease.job.payload.tenantId);
    if (result.rowCount === 0) {
      throw new LeaseLostError(lease.job.id, lease.job.leaseOwner ?? "unknown");
    }
  }

  private async finish(lease: LeaseResult, state: "succeeded" | "dead"): Promise<void> {
    const result = await this.pool.query(
      `UPDATE jobs SET state = $3, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, updated_at = $4
        WHERE id = $1 AND lease_token = $2`,
      [lease.job.id, lease.token, state, this.clock.now()],
    );
    this.limiter.release(lease.job.payload.tenantId);
    if (result.rowCount === 0) {
      // Another worker re-leased the job after our lease expired. Its result wins and ours is discarded,
      // which is safe only because every job handler is required to be idempotent (see HANDLERS.md).
      throw new LeaseLostError(lease.job.id, lease.job.leaseOwner ?? "unknown");
    }
  }

  private async findLiveByKey(dedupeKey: string): Promise<JobRow | null> {
    const result: QueryResult = await this.pool.query(
      `SELECT * FROM jobs WHERE dedupe_key = $1 AND state IN ('queued', 'leased') ORDER BY created_at LIMIT 1`,
      [dedupeKey],
    );
    return result.rowCount === 0 ? null : rowToJob(result.rows[0]);
  }

  /**
   * Per-state job counts for the admin dashboard and the queue-depth gauge. The query is a sequential scan
   * on large installations, so the dashboard polls it at most once a minute; do not call it per request.
   */
  async stats(): Promise<Record<JobState, number>> {
    const result = await this.pool.query(`SELECT state, COUNT(*)::int AS count FROM jobs GROUP BY state`);
    const counts: Record<JobState, number> = { queued: 0, leased: 0, succeeded: 0, failed: 0, dead: 0 };
    for (const row of result.rows) {
      counts[row.state as JobState] = Number(row.count);
    }
    this.queueDepth.set(counts.queued + counts.leased);
    return counts;
  }

  /**
   * Deletes finished jobs older than the retention window and returns how many rows were removed.
   *
   * Dead jobs are kept four times longer than succeeded ones because support uses them to reconstruct
   * incidents, and a dead job is often reported by a customer days after it happened. Failed jobs are never
   * purged here: "failed" is a transient state that `fail` immediately turns into "queued" or "dead".
   */
  async purge(retentionMs: number): Promise<number> {
    const nowMs = this.clock.now().getTime();
    const result = await this.pool.query(
      `DELETE FROM jobs WHERE (state = 'succeeded' AND updated_at < $1) OR (state = 'dead' AND updated_at < $2)`,
      [new Date(nowMs - retentionMs), new Date(nowMs - 4 * retentionMs)],
    );
    return result.rowCount ?? 0;
  }

  stop(): void {
    this.stopped = true;
  }

  get isStopped(): boolean {
    return this.stopped;
  }
}

/**
 * A job handler. It must be idempotent: after a lease expires, the same job can run again on another
 * worker, possibly while the first run is still finishing. Handlers should check `signal.aborted` between
 * externally visible steps and stop early once it is set.
 */
export type JobHandler = (job: JobRow, signal: AbortSignal) => Promise<void>;

/**
 * Runs one worker loop until the scheduler stops.
 *
 * While the queue is empty the loop polls with a capped exponential backoff; while a handler runs, a
 * heartbeat timer keeps the lease alive and aborts the handler as soon as the lease is lost.
 */
export async function runWorker(scheduler: JobScheduler, workerId: string, handler: JobHandler, idleDelayMs = 250): Promise<void> {
  let currentIdleDelayMs = idleDelayMs;
  while (!scheduler.isStopped) {
    const lease = await scheduler.lease(workerId);
    if (lease === null) {
      await sleep(currentIdleDelayMs);
      // Doubling up to five seconds keeps an idle fleet of two hundred workers from hammering the database
      // with lease queries, while still picking up newly submitted work within a few seconds.
      currentIdleDelayMs = Math.min(currentIdleDelayMs * 2, 5_000);
      continue;
    }
    currentIdleDelayMs = idleDelayMs;
    await runLeased(scheduler, workerId, lease, handler);
  }
}

async function runLeased(scheduler: JobScheduler, workerId: string, initial: LeaseResult, handler: JobHandler): Promise<void> {
  const controller = new AbortController();
  let lease = initial;
  // setInterval rather than a chained setTimeout: a slow heartbeat must not push the next one later, or a
  // few slow round trips in a row would add up to a lapsed lease.
  const timer = setInterval(() => {
    scheduler.heartbeat(lease, workerId).then(
      (renewed) => {
        lease = renewed;
      },
      () => {
        // Losing the lease means another worker may already be running this job. Abort so that the handler
        // stops writing partial results that would race with the new owner's writes.
        controller.abort();
      },
    );
  }, scheduler.heartbeatIntervalMs());
  try {
    await handler(lease.job, controller.signal);
    await scheduler.complete(lease);
  } catch (error) {
    // A lost lease is not the job's failure: the new owner will report the outcome, and counting an extra
    // attempt here would push healthy jobs toward the dead-letter queue during a database failover.
    if (!(error instanceof LeaseLostError)) {
      await scheduler.fail(lease, error);
    }
  } finally {
    clearInterval(timer);
  }
}

// A promise-based sleep. Node's timers/promises version is avoided because the workers still run on Node 14.
function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
