import type { CantClient, JobSummary } from "./client.ts";

export const JOBS_COLLECT_EVENT = "pi:jobs:collect";
export const JOBS_CHANGED_EVENT = "pi:jobs:changed";

export interface ExternalJobEntry {
	id: string;
	source: string;
	kind: string;
	status: string;
	name: string;
	summary: string;
	elapsedMs: number;
	canCancel?: boolean;
}

export interface ExternalJobProvider {
	source: string;
	list(): ExternalJobEntry[] | Promise<ExternalJobEntry[]>;
	tail(id: string): Promise<string>;
	cancel?(id: string): Promise<void>;
}

export interface CollectedExternalJob {
	entry: ExternalJobEntry;
	provider: ExternalJobProvider;
}

export interface EventBusLike {
	on(event: string, handler: (payload: unknown) => void): () => void;
	emit(event: string, payload?: unknown): void;
}

export interface JobTailSnapshot {
	output: string;
	status?: string;
}

const JOB_TAIL_LIMIT = 32_768;

export function isActiveJob(status: string): boolean {
	return status === "running" || status === "queued";
}

export interface DisplayJob {
	id: string;
	kind: string;
	source?: string;
	status: string;
	name: string;
	summary: string;
	elapsedMs: number;
	tail(): Promise<JobTailSnapshot>;
	cancel?(): Promise<void>;
}

export async function collectExternalJobs(events?: EventBusLike): Promise<CollectedExternalJob[]> {
	if (!events) return [];
	const providers: ExternalJobProvider[] = [];
	try {
		events.emit(JOBS_COLLECT_EVENT, providers);
	} catch {
		return [];
	}
	if (providers.length === 0) return [];
	const settled = await Promise.allSettled(
		providers.map(async (provider) => {
			const entries = await provider.list();
			return entries.map((entry) => ({ entry, provider }));
		}),
	);
	const out: CollectedExternalJob[] = [];
	for (const res of settled) {
		if (res.status === "fulfilled") out.push(...res.value);
	}
	return out;
}

export function mergeDisplayJobs(
	cantJobs: Iterable<JobSummary>,
	externalJobs: readonly CollectedExternalJob[],
	client: Pick<CantClient, "tailJob" | "cancelJob">,
): DisplayJob[] {
	const cantDisplay: DisplayJob[] = [...cantJobs]
		.sort((a, b) => a.elapsedMs - b.elapsedMs)
		.map((j) => ({
			id: j.id,
			kind: j.kind,
			status: j.status,
			name: j.name,
			summary: j.summary,
			elapsedMs: j.elapsedMs,
			tail: () => client.tailJob(j.id, JOB_TAIL_LIMIT),
			cancel: async () => {
				await client.cancelJob(j.id);
			},
		}));
	const extDisplay: DisplayJob[] = [...externalJobs]
		.sort((a, b) => a.entry.elapsedMs - b.entry.elapsedMs)
		.map(({ entry, provider }) => ({
			id: entry.id,
			kind: entry.kind,
			source: entry.source,
			status: entry.status,
			name: entry.name,
			summary: entry.summary,
			elapsedMs: entry.elapsedMs,
			tail: async () => {
				const output = await provider.tail(entry.id);
				// Providers don't take a byte limit. Cap the window without leaving a split surrogate pair.
				return { output: output.slice(-JOB_TAIL_LIMIT).replace(/^[\udc00-\udfff]/, "") };
			},
			cancel: entry.canCancel && provider.cancel ? () => provider.cancel!(entry.id) : undefined,
		}));
	return [...cantDisplay, ...extDisplay];
}

export function formatElapsed(ms: number): string {
	if (ms < 1000) return `${ms}ms`;
	const secs = Math.floor(ms / 1000);
	if (secs < 60) return `${secs}s`;
	return `${Math.floor(secs / 60)}m${secs % 60}s`;
}

export function formatJobBadge(job: Pick<DisplayJob, "kind" | "source">): string {
	return job.source ? `${job.source}:${job.kind}` : job.kind;
}

export function formatJobListText(jobs: readonly DisplayJob[]): string {
	if (jobs.length === 0) return "No jobs.";
	return jobs
		.map((j) => `${j.id}  [${formatJobBadge(j)}]  ${j.status}  ${formatElapsed(j.elapsedMs)}  ${j.name} — ${j.summary}`)
		.join("\n");
}
