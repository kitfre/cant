import type { ExtensionAPI, ExtensionContext, KeybindingsManager, Theme } from "@earendil-works/pi-coding-agent";
import { Editor, Text, truncateToWidth, visibleWidth, type Component, type Focusable, type OverlayHandle, type TUI } from "@earendil-works/pi-tui";
import type { ReplRequest, ReplToolResponse } from "./client.ts";
import { terminalText } from "./rendering.ts";

const HELP = `Enter Steel Scheme to evaluate in the agent's shared workspace.
:vars [name]       Inspect user bindings or one named binding
:history [id]      List recent submissions or show retained source
:load id           Prefill retained source in the editor without running it
:reset             Confirm before discarding the workspace
:help              Show this help
Up/Down recalls recent source; PageUp/PageDown scrolls output.
Escape closes when idle; during execution it cancels.`;

export class ReplTranscript {
	entries: Array<{ text: string; source?: string; bytes: number }> = [];
	state?: { workspaceId: string; generation: number; status: string };
	version = 0;
	private bytes = 0;

	clear(): void {
		this.entries = [];
		this.bytes = 0;
		this.state = undefined;
		this.version++;
	}

	record(request: ReplRequest, reply: ReplToolResponse, actor: "agent" | "you"): void {
		if (this.state && (reply.workspaceId !== this.state.workspaceId || reply.generation !== this.state.generation)) {
			this.clear();
		}
		this.state = { workspaceId: reply.workspaceId, generation: reply.generation, status: reply.status };
		const source = request.op === "eval" ? request.code : undefined;
		const input = source ?? JSON.stringify(request);
		const inputsHint = request.op === "eval" && Object.keys(request.inputs ?? {}).length ? " [inputs aren't recalled]" : "";
		this.append(`${actor} > ${input}${inputsHint}\n${reply.text}`, source);
	}

	append(text: string, source?: string): void {
		if (text.length > 300_000) text = `${text.slice(0, 300_000)}\n[UI scrollback entry truncated.]`;
		const bytes = Buffer.byteLength(text) + Buffer.byteLength(source ?? "");
		this.entries.push({ text, source, bytes });
		this.bytes += bytes;
		while (this.entries.length > 100 || this.bytes > 1024 * 1024) this.bytes -= this.entries.shift()!.bytes;
		this.version++;
	}
}

class ReplWindow implements Component, Focusable {
	readonly editor: Editor;
	private pending?: Promise<void>;
	private controller?: AbortController;
	private closing = false;
	private disposed = false;
	private scroll = 0;
	private pageSize = 10;
	private cache?: { width: number; version: number; lines: ReturnType<Component["render"]> };

	get focused(): boolean {
		return this.editor.focused;
	}
	set focused(value: boolean) {
		this.editor.focused = value;
	}

	constructor(
		private readonly tui: TUI,
		private readonly theme: Theme,
		private readonly keys: KeybindingsManager,
		private readonly options: {
			language: string;
			transcript: ReplTranscript;
			submit: (source: string, signal: AbortSignal) => Promise<string | undefined>;
			done: () => void;
		},
	) {
		this.editor = new Editor(tui, {
			borderColor: (s) => theme.fg("borderMuted", s),
			selectList: {
				selectedPrefix: (s) => theme.fg("accent", s),
				selectedText: (s) => theme.fg("accent", s),
				description: (s) => theme.fg("muted", s),
				scrollInfo: (s) => theme.fg("dim", s),
				noMatch: (s) => theme.fg("warning", s),
			},
		});
		for (const entry of options.transcript.entries) if (entry.source) this.editor.addToHistory(entry.source);
		this.editor.onSubmit = (source) => {
			void this.submit(source);
		};
	}

	submit(source: string): Promise<void> {
		if (this.pending) return this.pending;
		if (this.closing || this.disposed || !source.trim()) return Promise.resolve();
		this.controller = new AbortController();
		this.editor.setText("");
		this.editor.addToHistory(source);
		this.scroll = 0;
		this.pending = this.options
			.submit(source, this.controller.signal)
			.then((prefill) => {
				if (prefill !== undefined) this.editor.setText(prefill);
			})
			.catch((error: unknown) => {
				this.options.transcript.append(`Error: ${error instanceof Error ? error.message : String(error)}`);
				this.editor.setText(source);
			})
			.finally(() => {
				this.pending = undefined;
				this.controller = undefined;
				if (!this.disposed) this.tui.requestRender();
			});
		this.tui.requestRender();
		return this.pending;
	}

	async close(): Promise<void> {
		if (this.closing || this.disposed) return this.pending;
		this.closing = true;
		this.controller?.abort();
		await this.pending;
		if (!this.disposed) this.options.done();
	}

	dispose(): void {
		this.disposed = true;
		this.controller?.abort();
	}

	handleInput(data: string): void {
		if (this.closing || this.disposed) return;
		if (this.keys.matches(data, "tui.editor.pageUp")) this.scroll += this.pageSize;
		else if (this.keys.matches(data, "tui.editor.pageDown")) this.scroll = Math.max(0, this.scroll - this.pageSize);
		else if (this.keys.matches(data, "tui.select.cancel")) {
			if (this.pending) this.controller?.abort();
			else void this.close();
		} else if (!this.pending) this.editor.handleInput(data);
		this.tui.requestRender();
	}

	invalidate(): void {
		this.cache = undefined;
		this.editor.invalidate();
	}

	render(width: number): string[] {
		const inner = Math.max(1, width - 2);
		const height = Math.max(1, Math.floor(this.tui.terminal.rows * 0.85));
		const state = this.options.transcript.state;
		const identity = state ? `${state.workspaceId.slice(0, 8)} · g${state.generation} · ${state.status}` : "not started";
		const title = ` Cant REPL · ${this.options.language} · shared · ${identity} `;
		const frame = (text: string) => {
			const clipped = truncateToWidth(text, inner, "");
			return truncateToWidth(
				`${this.theme.fg("border", "│")}${clipped}${" ".repeat(Math.max(0, inner - visibleWidth(clipped)))}${this.theme.fg("border", "│")}`,
				width,
				"",
			);
		};
		const border = (text: string) => truncateToWidth(this.theme.fg("border", text), width, "");
		const editor = this.editor.render(inner);
		if (!this.cache || this.cache.width !== inner || this.cache.version !== this.options.transcript.version) {
			const text =
				this.options.transcript.entries.map((entry) => terminalText(entry.text)).join("\n\n") ||
				"Same workspace as the agent (hosted in cant daemon). :help for commands.\nClosing this window keeps bindings alive.";
			this.cache = { width: inner, version: this.options.transcript.version, lines: new Text(text, 0, 0).render(inner) };
		}
		this.pageSize = Math.max(1, height - editor.length - 5);
		const lines = this.cache.lines;
		this.scroll = Math.min(this.scroll, Math.max(0, lines.length - this.pageSize));
		const end = Math.max(0, lines.length - this.scroll);
		const output = lines.slice(Math.max(0, end - this.pageSize), end);
		const hint = (key: Parameters<KeybindingsManager["getKeys"]>[0]) => this.keys.getKeys(key).join("/");
		const help = this.pending
			? [`Running… ${hint("tui.select.cancel")} cancels`]
			: [
					`${hint("tui.input.submit")} run · ${hint("tui.input.newLine")} newline`,
					`↑↓ history · PgUp/PgDn output · ${hint("tui.select.cancel")} back`,
				];
		const rendered = [
			border(`╭${truncateToWidth(title, inner, "").padEnd(inner, "─")}╮`),
			...output.map(frame),
			...editor.map(frame),
			...help.map((line) => frame(this.theme.fg("dim", line))),
			border(`╰${"─".repeat(inner)}╯`),
		];
		return rendered.length <= height ? rendered : [rendered[0]!, ...rendered.slice(-(height - 1))].slice(0, height);
	}
}

function parsePositiveId(value: string | undefined): number {
	const id = Number(value);
	if (!value || !/^\d+$/.test(value) || !Number.isSafeInteger(id) || id < 1) {
		throw new Error("Expected a positive submission ID.");
	}
	return id;
}

export function registerReplOverlayCommand(
	pi: ExtensionAPI,
	options: {
		language: () => string;
		transcript: ReplTranscript;
		run: (request: ReplRequest, ctx: ExtensionContext, signal: AbortSignal) => Promise<ReplToolResponse>;
	},
) {
	let activeWindow: ReplWindow | undefined;
	let opening: Promise<void> | undefined;

	const runAndRecord = async (request: ReplRequest, ctx: ExtensionContext, signal: AbortSignal): Promise<ReplToolResponse> => {
		const reply = await options.run(request, ctx, signal);
		options.transcript.record(request, reply, "you");
		return reply;
	};

	pi.registerCommand("repl", {
		description: "Open an interactive Cant REPL window sharing the agent's live workspace",
		async handler(_args, ctx) {
			if (!ctx.hasUI) {
				ctx.ui.notify("The /repl window requires an interactive terminal UI.", "warning");
				return;
			}
			if (opening) return opening;
			let overlayHandle: OverlayHandle | undefined;
			opening = ctx.ui
				.custom<void>(
					(tui, theme, keys, done) => {
						const win = new ReplWindow(tui, theme, keys, {
							language: options.language(),
							transcript: options.transcript,
							done: () => {
								activeWindow = undefined;
								done();
							},
							async submit(source, signal) {
								const trimmed = source.trim();
								if (trimmed === ":help") {
									options.transcript.append(HELP);
									return undefined;
								}
								if (trimmed === ":reset") {
									overlayHandle?.setHidden(true);
									let confirmed = false;
									try {
										confirmed = await ctx.ui.confirm("Reset Cant REPL?", "Discard all retained bindings and history in this workspace?");
									} finally {
										overlayHandle?.setHidden(false);
									}
									if (confirmed) await runAndRecord({ op: "reset" }, ctx, signal);
									return undefined;
								}
								if (trimmed.startsWith(":vars")) {
									const name = trimmed.slice(":vars".length).trim() || undefined;
									await runAndRecord({ op: "inspect", ...(name ? { name } : { origin: "user" }) }, ctx, signal);
									return undefined;
								}
								if (trimmed.startsWith(":history")) {
									const rest = trimmed.slice(":history".length).trim();
									const id = rest ? parsePositiveId(rest) : undefined;
									await runAndRecord({ op: "history", ...(id ? { id } : {}) }, ctx, signal);
									return undefined;
								}
								if (trimmed.startsWith(":load")) {
									const id = parsePositiveId(trimmed.slice(":load".length).trim());
									const reply = await options.run({ op: "history", id }, ctx, signal);
									const match = reply.text.match(/\n\nSource:\n([\s\S]*)$/);
									if (!match) {
										options.transcript.append(reply.text);
										return undefined;
									}
									return match[1];
								}
								await runAndRecord({ op: "eval", code: source }, ctx, signal);
								return undefined;
							},
						});
						activeWindow = win;
						return win;
					},
					{
						overlay: true,
						overlayOptions: { width: "90%", maxHeight: "85%", anchor: "center" },
						onHandle: (handle) => {
							overlayHandle = handle;
						},
					},
				)
				.finally(() => {
					activeWindow?.dispose();
					activeWindow = undefined;
					opening = undefined;
				});
			return opening;
		},
	});

	return {
		async close() {
			await activeWindow?.close();
		},
	};
}
