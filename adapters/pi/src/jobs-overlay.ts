import { CustomEditor, type ExtensionAPI, type ExtensionContext, type Theme } from "@earendil-works/pi-coding-agent";
import { isFocusable, matchesKey, truncateToWidth, visibleWidth, type Component, type TUI } from "@earendil-works/pi-tui";
import type { CantClient, ChannelNotice, CompletionNotice, DaemonEvent, JobSummary } from "./client.ts";
import {
	collectExternalJobs,
	formatElapsed,
	formatJobBadge,
	formatJobListText,
	isActiveJob,
	JOBS_CHANGED_EVENT,
	JOBS_COLLECT_EVENT,
	mergeDisplayJobs,
	type CollectedExternalJob,
	type DisplayJob,
	type EventBusLike,
	type ExternalJobEntry,
	type ExternalJobProvider,
} from "./jobs-providers.ts";
import { JobTail } from "./jobs-tail.ts";
import { terminalText } from "./rendering.ts";

export {
	collectExternalJobs,
	JOBS_CHANGED_EVENT,
	JOBS_COLLECT_EVENT,
	type CollectedExternalJob,
	type ExternalJobEntry,
	type ExternalJobProvider,
};

const RECONNECT_INITIAL_MS = 250;
const RECONNECT_MAX_MS = 5_000;

class JobsPanelComponent implements Component {
	private selected = 0;
	private tail?: JobTail;
	private disposed = false;

	private readonly tui: TUI;
	private readonly theme: Theme;
	private readonly getJobs: () => DisplayJob[];
	private readonly isRefreshing: () => boolean;
	private readonly done: () => void;

	constructor(
		tui: TUI,
		theme: Theme,
		getJobs: () => DisplayJob[],
		isRefreshing: () => boolean,
		done: () => void,
	) {
		this.tui = tui;
		this.theme = theme;
		this.getJobs = getJobs;
		this.isRefreshing = isRefreshing;
		this.done = done;
	}

	onJobUpdated(job: JobSummary): void {
		if (this.disposed) return;
		if (this.tail && this.tail.job.source === undefined && this.tail.job.id === job.id) {
			this.tail.updateStatus(job.status);
		} else if (!this.tail) {
			this.tui.requestRender();
		}
	}

	onExternalJobsChanged(): void {
		if (this.disposed) return;
		const tail = this.tail;
		if (tail?.job.source !== undefined) {
			const job = this.getJobs().find((j) => j.source === tail.job.source && j.id === tail.job.id);
			if (job) tail.updateExternalJob(job);
		}
		this.tui.requestRender();
	}

	onJobsRefreshed(): void {
		if (this.disposed) return;
		const tail = this.tail;
		if (tail) {
			const job = this.getJobs().find((j) => j.source === tail.job.source && j.id === tail.job.id);
			if (job) {
				if (job.source !== undefined) tail.updateExternalJob(job);
				else if (job.status !== tail.status) tail.updateStatus(job.status);
			}
		}
		this.tui.requestRender();
	}

	private closeTail(): void {
		this.tail?.dispose();
		this.tail = undefined;
	}

	dispose(): void {
		this.disposed = true;
		this.closeTail();
	}

	private get tailHeight(): number {
		return Math.max(1, Math.floor(this.tui.terminal.rows * 0.8) - 3);
	}

	handleInput(data: string): void {
		if (this.disposed) return;
		const tail = this.tail;
		if (tail) {
			const page = Math.max(1, this.tailHeight - 1);
			if (matchesKey(data, "escape") || data === "q") this.closeTail();
			else if (matchesKey(data, "up")) tail.scrollBy(1);
			else if (matchesKey(data, "down")) tail.scrollBy(-1);
			else if (matchesKey(data, "pageUp")) tail.scrollBy(page);
			else if (matchesKey(data, "pageDown")) tail.scrollBy(-page);
			else if (matchesKey(data, "end")) tail.follow();
			else if (data === "k" && isActiveJob(tail.status) && tail.job.cancel) {
				void tail.job.cancel().catch(() => undefined);
				return;
			} else return;
			this.tui.requestRender();
			return;
		}

		const jobs = this.getJobs();
		if (matchesKey(data, "escape") || data === "q") {
			this.done();
			return;
		}
		if (matchesKey(data, "up") && jobs.length > 0) {
			this.selected = (this.selected - 1 + jobs.length) % jobs.length;
		} else if (matchesKey(data, "down") && jobs.length > 0) {
			this.selected = (this.selected + 1) % jobs.length;
		} else if (matchesKey(data, "enter") && jobs[this.selected]) {
			this.tail = new JobTail(jobs[this.selected]!, () => this.tui.requestRender());
		} else if (data === "k" && jobs[this.selected]?.cancel) {
			void jobs[this.selected]!.cancel!().catch(() => undefined);
		}
		this.tui.requestRender();
	}

	invalidate(): void {
		// Re-rendered dynamically from live daemon state
	}

	render(width: number): string[] {
		const inner = Math.max(1, width - 2);
		const height = Math.max(8, Math.floor(this.tui.terminal.rows * 0.8));
		const frame = (text: string) => {
			const clipped = truncateToWidth(text, inner, "");
			return truncateToWidth(
				`${this.theme.fg("border", "│")}${clipped}${" ".repeat(Math.max(0, inner - visibleWidth(clipped)))}${this.theme.fg("border", "│")}`,
				width,
				"",
			);
		};
		const border = (text: string) => truncateToWidth(this.theme.fg("border", text), width, "");

		const top = (title: string) => {
			const clipped = truncateToWidth(title, inner, "…");
			return border(`╭${clipped}${"─".repeat(Math.max(0, inner - visibleWidth(clipped)))}╮`);
		};
		const bottom = border(`╰${"─".repeat(inner)}╯`);
		if (this.tail) {
			const { job, status } = this.tail;
			const inline = (text: string) => terminalText(text).replace(/[\n\t]/g, " ");
			const suffix = ` · ${inline(status)} `;
			const nameWidth = inner - visibleWidth(suffix) - 1;
			const title = nameWidth > 0
				? ` ${truncateToWidth(inline(job.name || job.id), nameWidth, "…")}${suffix}`
				: ` ${inline(status)} `;
			const tailHeight = this.tailHeight;
			const body = this.tail.render(inner, tailHeight);
			return [
				top(title),
				...body.map(frame),
				...Array<string>(tailHeight - body.length).fill(frame("")),
				frame(this.theme.fg("dim", this.tail.footer())),
				bottom,
			];
		}

		const jobs = this.getJobs();
		if (this.selected >= jobs.length) this.selected = Math.max(0, jobs.length - 1);

		const bodyHeight = Math.max(1, height - 4);
		let bodyLines: string[];
		if (jobs.length === 0) {
			bodyLines = [
				this.theme.fg(
					"muted",
					this.isRefreshing()
						? "Loading jobs…"
						: "No background scripts, delegates, agent flows, or automation runs yet.",
				),
			];
		} else {
			bodyLines = jobs.slice(0, bodyHeight).map((job, idx) => {
				const marker = idx === this.selected ? this.theme.fg("accent", "▸ ") : "  ";
				const statusColor =
					isActiveJob(job.status)
						? "warning"
						: job.status === "succeeded" || job.status === "completed"
							? "success"
							: "error";
				const badge = formatJobBadge(job).padEnd(11);
				const line = `${marker}${job.id}  ${this.theme.fg(statusColor, job.status.padEnd(9))}  ${badge}  ${formatElapsed(job.elapsedMs).padStart(6)}  ${job.name}`;
				return truncateToWidth(line, inner, "…");
			});
		}

		return [
			top(` Jobs (${jobs.length}) `),
			...bodyLines.map(frame),
			frame(this.theme.fg("dim", "↑↓ navigate · Enter tail output · k cancel · Esc/q close")),
			bottom,
		];
	}
}

export function registerJobsUi(pi: ExtensionAPI, client: CantClient, sessionId: string) {
	const jobsById = new Map<string, JobSummary>();
	const activeRefreshTouched = new Set<Set<string>>();
	let externalJobs: CollectedExternalJob[] = [];
	let uiCtx: ExtensionContext | undefined;
	let activePanel: JobsPanelComponent | undefined;
	let panelPending = false;
	let inFlightRefreshes = 0;
	let footerVisible = false;
	let installedEditorFactory: ReturnType<ExtensionContext["ui"]["getEditorComponent"]>;
	let unsubscribe: (() => void) | undefined;
	let subscriptionStarted = false;
	let subscribed = false;
	let subscribing = false;
	let reconnectDelayMs = RECONNECT_INITIAL_MS;
	let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
	let disposed = false;
	const events = (pi as unknown as { events?: EventBusLike }).events;

	const currentJobs = (): DisplayJob[] => mergeDisplayJobs(jobsById.values(), externalJobs, client);

	const updateFooter = () => {
		if (disposed || !uiCtx?.hasUI) return;
		const running =
			[...jobsById.values()].filter((j) => isActiveJob(j.status)).length +
			externalJobs.filter((j) => isActiveJob(j.entry.status)).length;
		footerVisible = running > 0;
		uiCtx.ui.setStatus(
			"cant-jobs",
			footerVisible ? uiCtx.ui.theme.fg("accent", `▶ ${running} job${running === 1 ? "" : "s"} ↓`) : undefined,
		);
	};

	const refreshCant = async () => {
		const touched = new Set<string>();
		activeRefreshTouched.add(touched);
		try {
			const list = await client.listJobs(sessionId);
			if (disposed) return;
			const liveOverrides = new Map<string, JobSummary>();
			for (const id of touched) {
				const live = jobsById.get(id);
				if (live) liveOverrides.set(id, live);
			}
			jobsById.clear();
			for (const j of list) {
				jobsById.set(j.id, liveOverrides.get(j.id) ?? j);
			}
			for (const [id, live] of liveOverrides) {
				if (!jobsById.has(id)) jobsById.set(id, live);
			}
			updateFooter();
			activePanel?.onJobsRefreshed();
		} catch {
			// ignore if daemon unavailable
		} finally {
			activeRefreshTouched.delete(touched);
		}
	};

	const refreshExternal = async () => {
		const collected = await collectExternalJobs(events);
		if (disposed) return;
		externalJobs = collected;
		updateFooter();
		activePanel?.onExternalJobsChanged();
	};

	const trackRefresh = async (fn: () => Promise<void>) => {
		inFlightRefreshes++;
		try {
			await fn();
		} finally {
			inFlightRefreshes--;
			activePanel?.onJobsRefreshed();
		}
	};

	const refreshAll = () => trackRefresh(async () => {
		await Promise.allSettled([refreshCant(), refreshExternal()]);
	});

	const unsubscribeExternal = events?.on(JOBS_CHANGED_EVENT, () => {
		void refreshExternal();
	});

	const deliverCompletion = (notice: CompletionNotice) => {
		if (notice.notify === "off") return;
		pi.sendMessage(
			{
				customType: "cant-job-completion",
				content: `Cant background job completed:\n- [${notice.kind} ${notice.jobId}] ${notice.summary}`,
				display: true,
			},
			{ triggerTurn: notice.notify === "active", deliverAs: "followUp" },
		);
	};

	const deliverChannelMessage = (notice: ChannelNotice) => {
		const label = notice.channelName ? `#${notice.channelName}` : notice.channelId;
		const topic = notice.topic ? ` (${notice.topic})` : "";
		const trunc = notice.truncated ? " [truncated]" : "";
		const content = [
			`[cant channel ${label}${topic} from ${notice.fromHandle}@${notice.fromHarness}]${trunc}`,
			notice.body,
			`Reply with: (channels/send "${notice.channelId}" "...")`,
		].join("\n");
		pi.sendMessage(
			{
				customType: "cant-channel",
				content,
				display: true,
				details: notice,
			},
			{ triggerTurn: true, deliverAs: "followUp" },
		);
	};

	const handleEvent = (ev: DaemonEvent) => {
		if (disposed) return;
		if (ev.event === "job_updated") {
			for (const touched of activeRefreshTouched) touched.add(ev.job.id);
			jobsById.set(ev.job.id, ev.job);
			updateFooter();
			activePanel?.onJobUpdated(ev.job);
		} else if (ev.event === "job_completed") {
			deliverCompletion(ev.notice);
		} else if (ev.event === "channel_message") {
			deliverChannelMessage(ev.notice);
		}
	};

	const scheduleReconnect = () => {
		if (disposed || !subscriptionStarted || subscribed || subscribing || reconnectTimer || !uiCtx) return;
		const delay = reconnectDelayMs;
		reconnectDelayMs = Math.min(reconnectDelayMs * 2, RECONNECT_MAX_MS);
		reconnectTimer = setTimeout(() => {
			reconnectTimer = undefined;
			if (!disposed && uiCtx) void connectSubscription(uiCtx);
		}, delay);
		reconnectTimer.unref?.();
	};

	const connectSubscription = async (ctx: ExtensionContext) => {
		if (disposed || subscribing) return;
		uiCtx = ctx;
		if (reconnectTimer) {
			clearTimeout(reconnectTimer);
			reconnectTimer = undefined;
		}
		subscribing = true;
		unsubscribe?.();
		unsubscribe = undefined;
		subscribed = false;
		try {
			const unsub = await client.subscribe(sessionId, handleEvent, {
				onClose: () => {
					subscribed = false;
					unsubscribe = undefined;
					scheduleReconnect();
				},
			});
			if (disposed) {
				unsub();
				return;
			}
			unsubscribe = unsub;
			subscribed = true;
			reconnectDelayMs = RECONNECT_INITIAL_MS;
			void refreshExternal();
			await refreshCant();
		} catch (err) {
			console.error("[cant-pi] Failed subscribing to job events:", err);
			scheduleReconnect();
		} finally {
			subscribing = false;
		}
	};

	const openPanel = (ctx: ExtensionContext): Promise<void> => {
		if (panelPending) return Promise.resolve();
		uiCtx = ctx;
		panelPending = true;
		if (!ctx.hasUI) {
			return refreshAll()
				.then(() => {
					if (!disposed) ctx.ui.notify(formatJobListText(currentJobs()), "info");
				})
				.finally(() => {
					panelPending = false;
				});
		}

		// Kick off background refresh (and immediate reconnect if disconnected) without delaying overlay mount.
		if (subscriptionStarted && !subscribed && !subscribing) {
			reconnectDelayMs = RECONNECT_INITIAL_MS;
			void trackRefresh(() => connectSubscription(ctx));
		} else {
			void refreshAll();
		}

		return ctx.ui
			.custom<void>(
				(tui, theme, _keys, done) => {
					const panel = new JobsPanelComponent(tui, theme, currentJobs, () => inFlightRefreshes > 0, done);
					activePanel = panel;
					return panel;
				},
				{ overlay: true, overlayOptions: { width: "90%", maxHeight: "85%", anchor: "center" } },
			)
			.finally(() => {
				activePanel?.dispose();
				activePanel = undefined;
				panelPending = false;
			});
	};

	const installEditorHook = (ctx: ExtensionContext) => {
		if (ctx.mode !== "tui") return;
		const previous = ctx.ui.getEditorComponent();
		if (installedEditorFactory && previous === installedEditorFactory) return;
		const wrapped: NonNullable<typeof installedEditorFactory> = (tui, theme, keys) => {
			const editor = previous?.(tui, theme, keys) ?? new CustomEditor(tui, theme, keys, { embedWorkingStatus: true });
			const handleInput = editor.handleInput.bind(editor);
			// Custom editors need not expose autocomplete state. Leave Down alone if we can't tell.
			const eligible = () => footerVisible && isFocusable(editor) && editor.focused && !tui.hasOverlay()
				&& editor.getText() === "" && "isShowingAutocomplete" in editor
				&& typeof editor.isShowingAutocomplete === "function" && editor.isShowingAutocomplete() === false;
			editor.handleInput = (data) => {
				if (!matchesKey(data, "down") || panelPending || !eligible()) return handleInput(data);
				void openPanel(ctx).catch((err) => ctx.ui.notify(`Cannot open jobs: ${err}`, "error"));
			};
			return editor;
		};
		installedEditorFactory = wrapped;
		ctx.ui.setEditorComponent(wrapped);
	};

	const jobsCommandHandler = async (args: string, ctx: ExtensionContext) => {
		uiCtx = ctx;
		const [action, id] = args.trim().split(/\s+/).filter(Boolean);
		if (action === "cancel" || action === "kill") {
			if (!id) {
				ctx.ui.notify(`Usage: /jobs ${action} <id>`, "warning");
				return;
			}
			try {
				await refreshExternal();
				const ext = externalJobs.find((j) => j.entry.id === id);
				if (ext?.provider.cancel) {
					await ext.provider.cancel(id);
				} else {
					await client.cancelJob(id);
				}
				ctx.ui.notify(`Cancelled job ${id}.`, "info");
			} catch (err) {
				ctx.ui.notify(err instanceof Error ? err.message : String(err), "error");
			}
			return;
		}
		if (action === "tail" || action === "output" || action === "result") {
			if (!id) {
				ctx.ui.notify(`Usage: /jobs ${action} <id>`, "warning");
				return;
			}
			try {
				await refreshExternal();
				const ext = externalJobs.find((j) => j.entry.id === id);
				if (ext) {
					const output = await ext.provider.tail(id);
					ctx.ui.notify(`${ext.entry.id} (${ext.entry.source}:${ext.entry.kind}, ${ext.entry.status}):\n${output}`, "info");
				} else {
					const res = await client.tailJob(id, 16_384);
					ctx.ui.notify(`${res.id} (${res.kind}, ${res.status}):\n${res.output}`, "info");
				}
			} catch (err) {
				ctx.ui.notify(err instanceof Error ? err.message : String(err), "error");
			}
			return;
		}
		await openPanel(ctx);
	};

	pi.registerCommand("jobs", {
		description: "Inspect and manage Cant background scripts, delegates, agent flows, and external job providers. Usage: /jobs [tail|cancel] [id]",
		handler: jobsCommandHandler,
	});
	pi.registerCommand("delegates", {
		description: "Alias for /jobs.",
		handler: jobsCommandHandler,
	});

	return {
		bindContext(ctx: ExtensionContext) {
			if (disposed) return;
			uiCtx = ctx;
			installEditorHook(ctx);
			updateFooter();
			void refreshExternal();
		},
		async startSubscription(ctx: ExtensionContext) {
			if (disposed) return;
			subscriptionStarted = true;
			reconnectDelayMs = RECONNECT_INITIAL_MS;
			await connectSubscription(ctx);
		},
		dispose() {
			disposed = true;
			uiCtx = undefined;
			footerVisible = false;
			installedEditorFactory = undefined;
			subscribed = false;
			subscribing = false;
			if (reconnectTimer) {
				clearTimeout(reconnectTimer);
				reconnectTimer = undefined;
			}
			unsubscribe?.();
			unsubscribe = undefined;
			unsubscribeExternal?.();
			activePanel?.dispose();
			activePanel = undefined;
		},
	};
}
