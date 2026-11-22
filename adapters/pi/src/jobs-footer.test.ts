import assert from "node:assert/strict";
import test from "node:test";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { CantClient, DaemonEvent, JobSummary } from "./client.ts";
import { registerJobsUi } from "./jobs-overlay.ts";

test("footer Down belongs only to an empty, unobstructed prompt while jobs are active", async () => {
	const down = "\x1b[B";
	let text = "", completing = false, overlay = false, opened = 0;
	const forwarded: string[] = [];
	const editor = {
		focused: true,
		getText: () => text,
		isShowingAutocomplete: () => completing,
		handleInput: (data: string) => { forwarded.push(data); },
	};
	const tui = { hasOverlay: () => overlay };
	let status: string | undefined;
	const ctx = { mode: "tui", hasUI: true, ui: {
		getEditorComponent: () => () => editor,
		setEditorComponent: (factory: (t: typeof tui) => unknown) => factory(tui),
		setStatus: (_key: string, value?: string) => { status = value; },
		theme: { fg: (_: string, value: string) => value },
		custom: async () => { opened++; },
	} } as unknown as ExtensionContext;
	const job: JobSummary = { sessionId: "test", id: "bg-1", kind: "background", name: "test", status: "queued", summary: "", elapsedMs: 0 };
	let emit!: (e: DaemonEvent) => void;
	const client = { listJobs: async () => [job], subscribe: async (_: string, cb: typeof emit) => { emit = cb; } };
	const ui = registerJobsUi({ registerCommand() {} } as unknown as ExtensionAPI, client as unknown as CantClient, "test");
	ui.bindContext(ctx);
	editor.handleInput(down);
	await ui.startSubscription(ctx);
	assert.match(status ?? "", /1 job ↓/);
	for (text of [" ", "\n", "draft"]) editor.handleInput(down);
	text = "";
	completing = true;
	editor.handleInput(down);
	completing = false;
	overlay = true;
	editor.handleInput(down);
	overlay = false;
	editor.focused = false;
	editor.handleInput(down);
	editor.focused = true;
	editor.handleInput("\x1b[A");
	editor.handleInput("\x1b[1;5B");
	assert.deepEqual(forwarded, [...Array(7).fill(down), "\x1b[A", "\x1b[1;5B"]);
	assert.equal(opened, 0);
	editor.handleInput(down);
	editor.handleInput(down);
	assert.equal(opened, 1, "Down must open the panel synchronously without waiting on daemon refresh");
	await new Promise(setImmediate);
	assert.equal(opened, 1);
	// Once an overlay is open, Down must not open another panel.
	overlay = true;
	editor.handleInput(down);
	await new Promise(setImmediate);
	assert.equal(opened, 1);
	overlay = false;
	emit({ event: "job_updated", job: { ...job, status: "succeeded" } });
	assert.equal(status, undefined);
	forwarded.length = 0;
	editor.handleInput(down);
	assert.deepEqual(forwarded, [down]);
	ui.dispose();
});

test("Down and /jobs open immediately while refreshes run in parallel, and live events beat stale snapshots", async () => {
	const down = "\x1b[B";
	const job: JobSummary = { sessionId: "s", id: "bg-1", kind: "script", name: "build", status: "running", summary: "", elapsedMs: 10 };
	let resolveList!: (jobs: JobSummary[]) => void;
	let resolveExternal!: (entries: Array<{ id: string; source: string; kind: string; status: string; name: string; summary: string; elapsedMs: number }>) => void;
	let listCalls = 0;
	let externalCalls = 0;
	let blockRefreshes = false;
	let renders = 0;
	let panel!: { render(width: number): string[]; handleInput?(data: string): void };
	let closePanel!: () => void;
	let emit!: (e: DaemonEvent) => void;

	const editor = {
		focused: true,
		getText: () => "",
		isShowingAutocomplete: () => false,
		handleInput: (_data: string) => {},
	};
	const commands = new Map<string, (args: string, ctx: ExtensionContext) => Promise<void>>();
	const pi = {
		registerCommand(name: string, cmd: { handler: (args: string, ctx: ExtensionContext) => Promise<void> }) {
			commands.set(name, cmd.handler);
		},
		events: {
			on: () => () => {},
			emit(name: string, payload?: unknown) {
				if (name === "pi:jobs:collect") {
					(payload as unknown[]).push({
						source: "ext",
						list: () => {
							if (!blockRefreshes) return Promise.resolve([]);
							externalCalls++;
							return new Promise((resolve) => {
								resolveExternal = resolve;
							});
						},
						tail: async () => "",
					});
				}
			},
		},
	};
	const ctx = {
		mode: "tui",
		hasUI: true,
		ui: {
			getEditorComponent: () => () => editor,
			setEditorComponent: (factory: (t: unknown) => unknown) => factory({ hasOverlay: () => false }),
			setStatus() {},
			theme: { fg: (_: string, v: string) => v },
			custom: (factory: (tui: unknown, theme: unknown, keys: unknown, done: () => void) => typeof panel) =>
				new Promise<void>((done) => {
					closePanel = done;
					panel = factory(
						{ terminal: { rows: 20 }, requestRender: () => { renders++; } },
						{ fg: (_: string, v: string) => v },
						{},
						done,
					);
				}),
		},
	} as unknown as ExtensionContext;

	const client = {
		listJobs: () => {
			if (!blockRefreshes) return Promise.resolve([]);
			listCalls++;
			return new Promise<JobSummary[]>((resolve) => {
				resolveList = resolve;
			});
		},
		subscribe: async (_: string, cb: typeof emit) => {
			emit = cb;
			return () => {};
		},
	};

	const ui = registerJobsUi(pi as unknown as ExtensionAPI, client as unknown as CantClient, "s");
	ui.bindContext(ctx);
	await ui.startSubscription(ctx);
	await new Promise(setImmediate);

	// /jobs opens synchronously even while listJobs + external list are still in flight.
	blockRefreshes = true;
	const opened = commands.get("jobs")!("", ctx);
	assert.ok(panel, "/jobs must mount the overlay synchronously before refreshes resolve");
	assert.equal(listCalls, 1);
	assert.equal(externalCalls, 1, "external refresh must run in parallel with listJobs, not after it");
	assert.match(panel.render(80).join("\n"), /Loading jobs/);

	// Live completion arrives while listJobs is still in flight with a stale "running" snapshot.
	emit({ event: "job_updated", job: { ...job, status: "succeeded" } });
	resolveList([job]);
	resolveExternal([{ id: "ext-1", source: "ext", kind: "auto", status: "running", name: "scan", summary: "", elapsedMs: 5 }]);
	await new Promise(setImmediate);

	const rendered = panel.render(80).join("\n");
	assert.match(rendered, /bg-1\s+succeeded/, "live event must not be overwritten by older in-flight listJobs snapshot");
	assert.match(rendered, /ext-1/);
	assert.ok(renders > 0);

	closePanel();
	await opened;

	// Now test Down opens synchronously too when external job is active.
	let openedViaDown = false;
	const origCustom = ctx.ui.custom;
	(ctx.ui as unknown as { custom: typeof origCustom }).custom = ((factory: Parameters<typeof origCustom>[0]) => {
		openedViaDown = true;
		return origCustom(factory);
	}) as typeof origCustom;
	editor.handleInput(down);
	assert.equal(openedViaDown, true, "Down must open synchronously when external job is active");
	closePanel();
	resolveList([job]);
	resolveExternal([]);
	await new Promise(setImmediate);
	ui.dispose();
});

test("installEditorHook re-wraps if another extension replaces or resets the editor component", async () => {
	const down = "\x1b[B";
	let opened = 0;
	let currentFactory: ((tui: unknown, theme: unknown, keys: unknown) => unknown) | undefined;
	const makeEditor = () => ({
		focused: true,
		getText: () => "",
		isShowingAutocomplete: () => false,
		handleInput: (_data: string) => {},
	});
	let activeEditor = makeEditor();
	const tui = { hasOverlay: () => false };
	const ctx = {
		mode: "tui",
		hasUI: true,
		ui: {
			getEditorComponent: () => currentFactory,
			setEditorComponent: (factory: typeof currentFactory) => {
				currentFactory = factory;
				activeEditor = (factory ? factory(tui, {}, {}) : makeEditor()) as typeof activeEditor;
			},
			setStatus() {},
			theme: { fg: (_: string, v: string) => v },
			custom: async () => { opened++; },
		},
	} as unknown as ExtensionContext;

	const job: JobSummary = { sessionId: "s", id: "bg-1", kind: "script", name: "test", status: "running", summary: "", elapsedMs: 0 };
	const client = {
		listJobs: async () => [job],
		subscribe: async () => () => {},
	};
	const ui = registerJobsUi({ registerCommand() {} } as unknown as ExtensionAPI, client as unknown as CantClient, "s");
	ui.bindContext(ctx);
	await ui.startSubscription(ctx);

	// Calling bindContext again when already outermost must be idempotent (same factory).
	const firstFactory = currentFactory;
	ui.bindContext(ctx);
	assert.equal(currentFactory, firstFactory);

	// Simulate another extension clobbering the editor component.
	ctx.ui.setEditorComponent(() => makeEditor() as never);
	activeEditor.handleInput(down);
	assert.equal(opened, 0);

	// Re-binding context must re-install the Down hook around the new editor.
	ui.bindContext(ctx);
	activeEditor.handleInput(down);
	assert.equal(opened, 1);
	ui.dispose();
});

test("footer shows active external jobs and clears when they finish", async () => {
	let extStatus = "running";
	let footer: string | undefined;
	const listeners = new Map<string, () => void>();
	const pi = {
		registerCommand() {},
		events: {
			on(event: string, fn: () => void) {
				listeners.set(event, fn);
				return () => { listeners.delete(event); };
			},
			emit(event: string, payload?: unknown) {
				if (event === "pi:jobs:collect") {
					(payload as unknown[]).push({
						source: "beaver",
						list: async () => [{ id: "run-1", source: "beaver", kind: "auto", status: extStatus, name: "nightly", summary: "", elapsedMs: 100 }],
						tail: async () => "",
					});
				}
			},
		},
	};
	const ctx = {
		mode: "tui",
		hasUI: true,
		ui: {
			getEditorComponent: () => undefined,
			setEditorComponent() {},
			setStatus: (_key: string, value?: string) => { footer = value; },
			theme: { fg: (_: string, v: string) => v },
		},
	} as unknown as ExtensionContext;
	const client = {
		listJobs: async () => [],
		subscribe: async () => () => {},
	};
	const ui = registerJobsUi(pi as unknown as ExtensionAPI, client as unknown as CantClient, "s");
	ui.bindContext(ctx);
	await ui.startSubscription(ctx);
	await new Promise(setImmediate);
	assert.match(footer ?? "", /1 job ↓/, "active external job must populate the footer");

	extStatus = "succeeded";
	listeners.get("pi:jobs:changed")?.();
	await new Promise(setImmediate);
	assert.equal(footer, undefined, "completed external job must clear the footer");
	ui.dispose();
});

test("subscription subscribes before listing and auto-reconnects after unexpected disconnect", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const callOrder: string[] = [];
	let onCloseCb: (() => void) | undefined;
	let subCount = 0;
	let listJobsResult: JobSummary[] = [];
	let footer: string | undefined;

	const ctx = {
		mode: "tui",
		hasUI: true,
		ui: {
			getEditorComponent: () => undefined,
			setEditorComponent() {},
			setStatus: (_key: string, value?: string) => { footer = value; },
			theme: { fg: (_: string, v: string) => v },
		},
	} as unknown as ExtensionContext;

	const client = {
		listJobs: async () => {
			callOrder.push("listJobs");
			return listJobsResult;
		},
		subscribe: async (_sid: string, _cb: (ev: DaemonEvent) => void, opts?: { onClose?: () => void }) => {
			callOrder.push("subscribe");
			subCount++;
			onCloseCb = opts?.onClose;
			return () => {};
		},
	};
	const ui = registerJobsUi({ registerCommand() {} } as unknown as ExtensionAPI, client as unknown as CantClient, "s");
	await ui.startSubscription(ctx);
	assert.deepEqual(callOrder.slice(0, 2), ["subscribe", "listJobs"], "must subscribe before listJobs to avoid startup race");

	// Simulate daemon restart dropping the subscription while a new job starts.
	listJobsResult = [{ sessionId: "s", id: "bg-2", kind: "script", name: "after-restart", status: "running", summary: "", elapsedMs: 0 }];
	onCloseCb?.();
	t.mock.timers.tick(250);
	await new Promise(setImmediate);
	await new Promise(setImmediate);

	assert.equal(subCount, 2, "expected subscription to reconnect after backoff");
	assert.match(footer ?? "", /1 job ↓/, "reconnect must re-sync jobs and restore footer");

	// Disposing must stop future reconnect timers.
	ui.dispose();
	onCloseCb?.();
	t.mock.timers.tick(10_000);
	await new Promise(setImmediate);
	assert.equal(subCount, 2);
});

test("delivers incoming channel_message events via pi.sendMessage with triggerTurn", async () => {
	const sent: Array<{ msg: any; opts: any }> = [];
	let emit: ((ev: DaemonEvent) => void) | undefined;
	const client = {
		listJobs: async () => [],
		subscribe: async (_sid: string, cb: (ev: DaemonEvent) => void) => {
			emit = cb;
			return () => {};
		},
	};
	const pi = {
		registerCommand() {},
		sendMessage(msg: any, opts: any) {
			sent.push({ msg, opts });
		},
	} as unknown as ExtensionAPI;
	const ctx = {
		hasUI: true,
		ui: { setFooter() {}, setStatus() {}, setWidget() {} },
	} as unknown as ExtensionContext;

	const ui = registerJobsUi(pi, client as unknown as CantClient, "pi-1");
	await ui.startSubscription(ctx);
	emit?.({
		event: "channel_message",
		notice: {
			channelId: "ch-1",
			channelName: undefined,
			topic: "login payload",
			seq: 1,
			from: "conv-agy",
			fromHandle: "frontend",
			fromHarness: "antigravity",
			body: "Does /login expect access_token?",
			truncated: false,
			timestampMs: 123,
		},
	});

	assert.equal(sent.length, 1);
	assert.equal(sent[0].msg.customType, "cant-channel");
	assert.match(sent[0].msg.content, /ch-1/);
	assert.match(sent[0].msg.content, /frontend/);
	assert.match(sent[0].msg.content, /Does \/login expect access_token\?/);
	assert.match(sent[0].msg.content, /channels\/send/);
	assert.equal(sent[0].opts.triggerTurn, true);
	assert.equal(sent[0].opts.deliverAs, "followUp");
	ui.dispose();
});

test("a rejected initial subscription retries and syncs jobs without user input", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	t.mock.method(console, "error", () => {});
	const calls: string[] = [];
	const client = {
		subscribe: async () => {
			calls.push("subscribe");
			if (calls.length === 1) throw new Error("daemon unavailable");
			return () => {};
		},
		listJobs: async () => { calls.push("listJobs"); return []; },
	};
	const ui = registerJobsUi({ registerCommand() {} } as unknown as ExtensionAPI, client as unknown as CantClient, "s");
	t.after(() => ui.dispose());
	await ui.startSubscription({ hasUI: false } as ExtensionContext);
	assert.deepEqual(calls, ["subscribe"]);
	t.mock.timers.tick(250);
	await new Promise(setImmediate);
	assert.deepEqual(calls, ["subscribe", "subscribe", "listJobs"]);
});
