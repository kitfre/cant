import assert from "node:assert/strict";
import test from "node:test";
import {
	collectExternalJobs,
	formatJobListText,
	JOBS_COLLECT_EVENT,
	mergeDisplayJobs,
	type ExternalJobEntry,
	type ExternalJobProvider,
} from "./jobs-providers.ts";
import type { JobSummary } from "./client.ts";

function createFakeEventBus() {
	const handlers = new Map<string, Array<(payload: unknown) => void>>();
	return {
		on(event: string, fn: (payload: unknown) => void) {
			const list = handlers.get(event) ?? [];
			list.push(fn);
			handlers.set(event, list);
			return () => {
				const cur = handlers.get(event);
				if (!cur) return;
				const idx = cur.indexOf(fn);
				if (idx >= 0) cur.splice(idx, 1);
			};
		},
		emit(event: string, payload?: unknown) {
			for (const fn of handlers.get(event) ?? []) {
				fn(payload);
			}
		},
	};
}

test("collectExternalJobs gathers entries from pi:jobs:collect providers and isolates failing providers", async () => {
	const events = createFakeEventBus();
	const entry: ExternalJobEntry = {
		id: "run-1",
		source: "example",
		kind: "auto",
		status: "running",
		name: "sample-job",
		summary: "Scanning repo",
		elapsedMs: 1200,
		canCancel: true,
	};
	const goodProvider: ExternalJobProvider = {
		source: "example",
		list: async () => [entry],
		tail: async (id) => `logs for ${id}`,
		cancel: async () => undefined,
	};
	const brokenProvider: ExternalJobProvider = {
		source: "broken",
		list: async () => {
			throw new Error("daemon offline");
		},
		tail: async () => "",
	};

	events.on(JOBS_COLLECT_EVENT, (providers) => {
		(providers as ExternalJobProvider[]).push(goodProvider, brokenProvider);
	});

	const collected = await collectExternalJobs(events);
	assert.equal(collected.length, 1);
	assert.equal(collected[0]!.entry.id, "run-1");
	assert.equal(collected[0]!.entry.source, "example");
	assert.equal(await collected[0]!.provider.tail("run-1"), "logs for run-1");
});

test("mergeDisplayJobs combines cant and external provider jobs and delegates tail/cancel", async () => {
	const events = createFakeEventBus();
	const cancelled: string[] = [];

	const cantJob: JobSummary = {
		sessionId: "test",
		id: "bg-1",
		kind: "background",
		status: "running",
		name: "cargo-test",
		summary: "running",
		elapsedMs: 500,
	};

	const fakeClient = {
		tailJob: async (id: string) => ({ id, kind: "background", status: "running", output: `cant:${id}` }),
		cancelJob: async (id: string) => {
			cancelled.push(`cant:${id}`);
		},
	};

	const exampleProvider: ExternalJobProvider = {
		source: "example",
		list: () => [
			{
				id: "ext-42",
				source: "example",
				kind: "auto",
				status: "running",
				name: "sample-job",
				summary: "running",
				elapsedMs: 900,
				canCancel: true,
			},
		],
		tail: async (id) => `example:${id}`,
		cancel: async (id) => {
			cancelled.push(`example:${id}`);
		},
	};

	events.on(JOBS_COLLECT_EVENT, (providers) => {
		(providers as ExternalJobProvider[]).push(exampleProvider);
	});

	const external = await collectExternalJobs(events);
	const merged = mergeDisplayJobs([cantJob], external, fakeClient);
	assert.equal(merged.length, 2);

	const text = formatJobListText(merged);
	assert.match(text, /bg-1\s+\[background\]/);
	assert.match(text, /ext-42\s+\[example:auto\]/);

	const cantTail = await merged[0]!.tail();
	assert.equal(cantTail.output, "cant:bg-1");
	assert.equal(cantTail.status, "running");
	assert.equal((await merged[1]!.tail()).output, "example:ext-42");

	await merged[0]!.cancel?.();
	await merged[1]!.cancel?.();
	assert.deepEqual(cancelled, ["cant:bg-1", "example:ext-42"]);
});
