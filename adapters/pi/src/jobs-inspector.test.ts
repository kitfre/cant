import assert from "node:assert/strict";
import test, { type TestContext } from "node:test";
import type { ExtensionAPI, ExtensionCommandContext } from "@earendil-works/pi-coding-agent";
import { visibleWidth, type Component } from "@earendil-works/pi-tui";
import type { CantClient, DaemonEvent, JobSummary } from "./client.ts";
import { JOBS_CHANGED_EVENT, JOBS_COLLECT_EVENT, registerJobsUi, type ExternalJobProvider } from "./jobs-overlay.ts";

const running: JobSummary = { sessionId: "session", id: "job-1", kind: "script", status: "running", name: "cargo test", summary: "", elapsedMs: 0 };
const flush = () => new Promise<void>((resolve) => setImmediate(resolve));
const snapshot = (output: string, status = "running", id = running.id) => ({ id, kind: running.kind, status, output });

function pendingSnapshot() {
	let resolve!: (result: ReturnType<typeof snapshot>) => void;
	const promise = new Promise<ReturnType<typeof snapshot>>((done) => { resolve = done; });
	return { promise, resolve };
}

async function openJobs(
	t: TestContext,
	tailJob: CantClient["tailJob"],
	jobs = [running],
	provider?: ExternalJobProvider,
) {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	let emit!: (event: DaemonEvent) => void;
	let panel!: Component;
	const commands = new Map<string, (args: string, ctx: ExtensionCommandContext) => Promise<void>>();
	const listeners = new Map<string, () => void>();
	const client = {
		listJobs: async () => jobs,
		tailJob,
		subscribe: async (_: string, cb: typeof emit) => {
			emit = cb;
			return () => {};
		},
	};
	const pi = {
		registerCommand: (name: string, command: Parameters<ExtensionAPI["registerCommand"]>[1]) => {
			commands.set(name, command.handler);
		},
		events: {
			on(name: string, fn: () => void) {
				listeners.set(name, fn);
				return () => { listeners.delete(name); };
			},
			emit(name: string, providers: ExternalJobProvider[]) {
				if (name === JOBS_COLLECT_EVENT && provider) providers.push(provider);
			},
		},
	};
	const theme = { fg: (_: string, text: string) => text };
	const ctx = { hasUI: true, ui: {
		theme,
		setStatus() {},
		custom: (factory: (tui: unknown, theme: unknown, keys: unknown, done: () => void) => Component) => new Promise<void>((done) => {
			panel = factory({ terminal: { rows: 12 }, requestRender() {} }, theme, {}, done);
		}),
	} } as unknown as ExtensionCommandContext;
	const ui = registerJobsUi(pi as unknown as ExtensionAPI, client as unknown as CantClient, "session");
	t.after(() => ui.dispose());
	await ui.startSubscription(ctx);
	const closed = commands.get("jobs")!("", ctx);
	await flush();
	panel.handleInput!("\r");
	return {
		emit,
		externalChanged: () => listeners.get(JOBS_CHANGED_EVENT)?.(),
		key: (data: string) => panel.handleInput!(data),
		render: (width = 88) => panel.render(width),
		text: () => panel.render(88).join("\n"),
		async tick() {
			t.mock.timers.tick(500);
			await flush();
		},
		async close() {
			panel.handleInput!("\x1b");
			panel.handleInput!("\x1b");
			await closed;
		},
	};
}

test("inspector follows bounded snapshots without overlapping reads or duplicating pushed chunks", async (t) => {
	const first = pendingSnapshot();
	let reads = 0;
	const app = await openJobs(t, async (id, maxBytes) => {
		assert.equal(id, running.id);
		assert.equal(maxBytes, 32_768);
		return ++reads === 1 ? first.promise : snapshot("start\nnew output");
	});
	assert.match(app.text(), /Loading/);
	const height = app.render().length;
	app.emit({ event: "job_output", id: running.id, chunk: "new output" });
	await app.tick();
	assert.equal(reads, 1);
	first.resolve(snapshot("start"));
	await flush();
	await app.tick();
	assert.equal(reads, 2);
	assert.equal(app.render().length, height);
	assert.equal(app.text().match(/new output/g)?.length, 1);
	assert.match(app.text(), /Following/);
	await app.close();
	await app.tick();
	assert.equal(reads, 2);
});

test("paused inspector holds its reading position until End, even when output is replaced", async (t) => {
	let output = Array.from({ length: 20 }, (_, i) => `line ${i}`).join("\n");
	const app = await openJobs(t, async () => snapshot(output));
	await flush();
	assert.match(app.text(), /line 19/);
	app.key("\x1b[A");
	const paused = app.render().slice(1, -2);
	assert.match(app.text(), /Paused/);
	assert.doesNotMatch(app.text(), /line 19/);
	output = "replacement output";
	await app.tick();
	assert.deepEqual(app.render().slice(1, -2), paused);
	assert.match(app.text(), /New output/);
	app.key("\x1b[B");
	assert.match(app.text(), /Paused/);
	assert.match(app.text(), /line 19/);
	app.key("\x1b[F");
	assert.match(app.text(), /replacement output/);
	assert.match(app.text(), /Following/);
	await app.close();
});

test("completion during an in-flight read fetches the final delegate answer and stops refreshing", async (t) => {
	const job = { ...running, kind: "delegate" };
	const pending = pendingSnapshot();
	let reads = 0;
	const app = await openJobs(t, async () => ++reads === 1 ? pending.promise : snapshot("final answer", "completed"), [job]);
	app.emit({ event: "job_updated", job: { ...job, status: "completed" } });
	assert.equal(reads, 1);
	pending.resolve(snapshot("progress log"));
	await flush();
	assert.match(app.text(), /cargo test.*completed/);
	assert.match(app.text(), /final answer/);
	assert.doesNotMatch(app.text(), /progress log/);
	await app.tick();
	assert.equal(reads, 2);
	await app.close();
});

test("leaving a tail ignores an in-flight reply even when reopening the same job", async (t) => {
	const pending = pendingSnapshot();
	let reads = 0;
	const app = await openJobs(t, async () => ++reads === 1 ? pending.promise : snapshot("current output", "succeeded"));
	app.key("\x1b");
	app.key("\r");
	await flush();
	pending.resolve(snapshot("stale output"));
	await flush();
	assert.match(app.text(), /current output/);
	assert.doesNotMatch(app.text(), /stale output/);
	await app.tick();
	assert.equal(reads, 2);
	await app.close();
});

test("inspector fits a narrow terminal and escapes control sequences in output and titles", async (t) => {
	const job = { ...running, name: "界界界\n\x1b[0m test" };
	const app = await openJobs(t, async () => snapshot("log\r\n\x1b[31m界界界", "succeeded"), [job]);
	await flush();
	const lines = app.render(20);
	assert.ok(lines.every((line) => visibleWidth(line) === 20));
	assert.match(lines[0]!, /succeeded/);
	assert.ok(lines.every((line) => !/[\n\r\x1b]/.test(line.replaceAll("\x1b[0m", ""))));
	assert.match(lines.join("\n"), /\\u\{1b\}\[31m/);
	await app.close();
});

test("external jobs refresh, keep their last good output on error, and fetch once more on completion", async (t) => {
	let status = "running";
	let reads = 0;
	const provider: ExternalJobProvider = {
		source: "example",
		list: () => [{ ...running, source: "example", status }],
		async tail() {
			if (++reads === 2) throw new Error("provider unavailable");
			return `snapshot ${reads}`;
		},
	};
	const app = await openJobs(t, async () => { throw new Error("unexpected native read"); }, [], provider);
	await flush();
	assert.match(app.text(), /snapshot 1/);
	await app.tick();
	assert.match(app.text(), /snapshot 1/);
	assert.match(app.text(), /Refresh failed.*retrying/);
	await app.tick();
	assert.match(app.text(), /snapshot 3/);
	assert.doesNotMatch(app.text(), /Refresh failed/);
	status = "completed";
	app.externalChanged();
	await flush();
	assert.match(app.text(), /cargo test.*completed/);
	assert.match(app.text(), /snapshot 4/);
	await app.tick();
	assert.equal(reads, 4);
	await app.close();
});
