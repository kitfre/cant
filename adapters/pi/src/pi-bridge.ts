import { randomUUID } from "node:crypto";
import {
	createAgentSession,
	DefaultResourceLoader,
	getAgentDir,
	SessionManager,
	SettingsManager,
	type ExtensionContext,
	type ExtensionToolContext,
	type ToolLoadout,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import type { AdapterHandle, CantClient, ExternalCapabilitySpec, ExternalWorkerSpec } from "./client.ts";

import type { ReplCapability } from "./repl-providers.ts";

const RESOURCE_TOOLS = new Set(["list_mcp_resources", "list_mcp_resource_templates", "read_mcp_resource"]);

function sanitizeSegment(raw: string): string {
	return raw
		.replace(/[^A-Za-z0-9_$-]/g, "_")
		.replace(/^([^A-Za-z_$])/, "_$1");
}

function extractMcpCapabilities(loadout: ToolLoadout): ExternalCapabilitySpec[] {
	const callable = new Set(loadout.callable.map((t) => t.name));
	const specs: ExternalCapabilitySpec[] = [];
	for (const tool of loadout.registered) {
		if (!callable.has(tool.name)) continue;
		const ns = loadout.getNamespace(tool.name);
		if (RESOURCE_TOOLS.has(tool.name)) {
			specs.push({
				toolName: tool.name,
				apiPath: ["mcp", "resources", sanitizeSegment(tool.name)],
				description: tool.description,
				paramsSchema: tool.parameters ?? { type: "object", additionalProperties: false, properties: {} },
				resultSchema: tool.outputSchema ?? {},
			});
			continue;
		}
		if (!tool.name.startsWith("mcp__") || !ns?.name.startsWith("mcp__")) continue;
		const slash = tool.label.indexOf("/");
		if (slash < 1 || slash === tool.label.length - 1) continue;
		const server = sanitizeSegment(tool.label.slice(0, slash));
		const original = sanitizeSegment(tool.label.slice(slash + 1));
		specs.push({
			toolName: tool.name,
			apiPath: ["mcp", server, original],
			description: tool.description,
			paramsSchema: tool.parameters ?? { type: "object", additionalProperties: false, properties: {} },
			resultSchema: tool.outputSchema ?? {},
		});
	}
	return specs;
}

function extractTextResult(result: { content: Array<{ type: string; text?: string }>; details?: unknown; structuredContent?: unknown }): unknown {
	if (result.structuredContent !== undefined) return result.structuredContent;
	const text = result.content
		.filter((b) => b.type === "text" && typeof b.text === "string")
		.map((b) => b.text!)
		.join("\n");
	try {
		return JSON.parse(text);
	} catch {
		return text;
	}
}

function buildWorkerPrompt(spec: ExternalWorkerSpec): string {
	const parts = [spec.task];
	if (spec.input !== undefined) {
		parts.push(`## Structured Input\n\`\`\`json\n${JSON.stringify(spec.input, null, 2)}\n\`\`\``);
	}
	if (spec.outputSchema) {
		parts.push(`## Output Schema\nReturn ONLY valid JSON matching:\n\`\`\`json\n${JSON.stringify(spec.outputSchema)}\n\`\`\``);
	}
	return parts.join("\n\n");
}

function tryExtractJson(text: string): unknown {
	const trimmed = text.trim();
	try {
		return JSON.parse(trimmed);
	} catch {
		const fence = trimmed.match(/^```(?:json)?\s*\n([\s\S]*?)\n```$/);
		if (fence) {
			try {
				return JSON.parse(fence[1]!.trim());
			} catch {
				return undefined;
			}
		}
		return undefined;
	}
}

/**
 * Bridges a live Pi session onto the Cant daemon's reverse-RPC channel:
 * 1. Publishes Pi MCP tools and opted-in extension capabilities in the session REPL.
 * 2. Registers `"pi"` as a `WorkerDriver` so `delegate/*` and `agents/*` run via Pi SDK workers
 *    inheriting Pi's active model/credentials.
 */
export class PiBridge {
	private handle?: AdapterHandle;
	private latestCtx?: ExtensionContext;
	private activeToolCtx?: ExtensionToolContext;
	private activeEval?: { ctx: ExtensionContext; signal: AbortSignal; cancel: AbortController };
	private providers = new Map<string, ReplCapability>();
	private lastMcpSignature = "";
	private latestCapabilities: ExternalCapabilitySpec[] = [];

	constructor(
		private readonly client: CantClient,
		private readonly sessionId: string,
	) {}

	setContext(ctx: ExtensionContext): void {
		this.latestCtx = ctx;
	}

	setActiveEval(ctx?: ExtensionContext, signal?: AbortSignal): void {
		this.activeEval?.cancel.abort();
		const cancel = new AbortController();
		this.activeEval = ctx ? {
			ctx, cancel, signal: signal ? AbortSignal.any([signal, cancel.signal]) : cancel.signal,
		} : undefined;
		this.activeToolCtx = ctx && "executeTool" in ctx && typeof ctx.executeTool === "function"
			? ctx as ExtensionToolContext : undefined;
	}

	setProviders(providers: ReplCapability[]): void {
		this.providers = new Map(providers.map(provider => [provider.spec.toolName, provider]));
		this.handle?.updateCapabilities(this.capabilities());
	}

	private capabilities(): ExternalCapabilitySpec[] {
		return [...this.latestCapabilities, ...[...this.providers.values()].map(p => p.spec)];
	}

	observeLoadout(loadout: ToolLoadout): void {
		const specs = extractMcpCapabilities(loadout);
		const sig = JSON.stringify(specs.map((s) => [s.toolName, s.apiPath]));
		if (sig === this.lastMcpSignature) return;
		this.lastMcpSignature = sig;
		this.latestCapabilities = specs;
		this.handle?.updateCapabilities(this.capabilities());
	}

	async attach(): Promise<void> {
		this.handle?.close();
		const initialCapabilities = this.capabilities();
		this.handle = await this.client.attachAdapter({
			sessionId: this.sessionId,
			workerDriver: "pi",
			initialCapabilities,
			onInvokeCapability: async (toolName, args) => {
				const provider = this.providers.get(toolName);
				if (provider) {
					const evalContext = this.activeEval;
					if (!evalContext || evalContext.ctx.mode !== "tui" || !evalContext.ctx.hasUI) {
						throw new Error(`${toolName} requires an active interactive agent REPL call.`);
					}
					evalContext.signal.throwIfAborted();
					return provider.invoke(args, evalContext.ctx, evalContext.signal);
				}
				if (!this.activeToolCtx) {
					throw new Error(`Pi tool context unavailable while invoking ${toolName}; MCP execution requires an active agent repl turn.`);
				}
				const outcome = await this.activeToolCtx.executeTool(toolName, args, { signal: this.activeToolCtx.signal });
				if (outcome.isError) {
					const msg = outcome.result.content
						.filter((b) => b.type === "text")
						.map((b) => (b as { text: string }).text)
						.join("\n");
					throw new Error(msg || `Pi MCP tool ${toolName} returned an error.`);
				}
				return extractTextResult(outcome.result);
			},
			onRunWorker: (spec, emitProgress, signal) => this.runPiSdkWorker(spec, emitProgress, signal),
		});
		// The loadout may have changed while the adapter was attaching.
		const latest = this.capabilities();
		if (JSON.stringify(latest) !== JSON.stringify(initialCapabilities)) this.handle.updateCapabilities(latest);
	}

	private async runPiSdkWorker(
		spec: ExternalWorkerSpec,
		emitProgress: (chunk: string) => void,
		signal: AbortSignal,
	): Promise<{ text: string; data?: unknown; usage?: unknown }> {
		const agentDir = getAgentDir();
		const settingsManager = SettingsManager.create(spec.cwd, agentDir);
		const workerSessionId = spec.sessionId ?? `pi-worker-${randomUUID().slice(0, 10)}`;
		const cantClient = this.client;

		// Give the fresh Pi SDK worker a scoped `repl` tool backed by cant daemon.
		const workerExtension = {
			name: "cant-worker",
			hidden: true,
			factory: (workerPi: any) => {
				workerPi.registerTool({
					name: "repl",
					label: "Cant Worker REPL",
					description: "Local Steel Scheme REPL for this delegated worker task.",
					parameters: Type.Object({
						op: Type.Union([
							Type.Literal("eval"),
							Type.Literal("inspect"),
							Type.Literal("search"),
							Type.Literal("describe"),
						]),
						code: Type.Optional(Type.String()),
						inputs: Type.Optional(Type.Record(Type.String(), Type.String())),
						name: Type.Optional(Type.String()),
						query: Type.Optional(Type.String()),
					}),
					async execute(_id: string, params: any, toolSignal?: AbortSignal) {
						const reply = await cantClient.repl(
							{
								sessionId: workerSessionId,
								cwd: spec.cwd,
								language: "steel",
								// Prevent recursive delegation inside workers
								disabledNamespaces: ["delegate", "agents", "background"],
							},
							params,
							toolSignal,
						);
						return { content: [{ type: "text", text: reply.text }], details: {}, isError: reply.isError };
					},
				});
				workerPi.on("session_start", () => {
					const hasCaps = spec.capabilities === undefined || spec.capabilities.length > 0;
					workerPi.setActiveTools(hasCaps ? ["repl"] : []);
				});
			},
		};

		const resourceLoader = new DefaultResourceLoader({
			cwd: spec.cwd,
			agentDir,
			settingsManager,
			noExtensions: true,
			extensionFactories: [workerExtension as any],
			appendSystemPrompt: [
				"You are a focused, fresh-context delegated worker. Complete the task concisely and accurately using your available tools.",
			],
		});
		await resourceLoader.reload();

		const parentModel = this.latestCtx?.model;
		const { session } = await createAgentSession({
			cwd: spec.cwd,
			agentDir,
			settingsManager,
			resourceLoader,
			sessionManager: SessionManager.inMemory(spec.cwd),
			...(parentModel ? { model: parentModel } : {}),
		});

		let timer: ReturnType<typeof setTimeout> | undefined;
		const onAbort = () => {
			void session.abort();
		};
		signal.addEventListener("abort", onAbort, { once: true });
		if (spec.timeoutSecs && spec.timeoutSecs > 0) {
			timer = setTimeout(() => void session.abort(), spec.timeoutSecs * 1000);
		}

		let finalText = "";
		const unsubscribe = session.subscribe((event: any) => {
			if (event.type === "message_update" && event.assistantMessageEvent?.type === "text_delta") {
				const delta = String(event.assistantMessageEvent.delta ?? "");
				finalText += delta;
				emitProgress(delta);
			} else if (event.type === "tool_execution_start") {
				emitProgress(`[tool: ${event.toolName}]\n`);
			}
		});

		let unsubComms: (() => void) | undefined;
		if (spec.channels && spec.channels.length > 0) {
			unsubComms = await cantClient
				.subscribe(workerSessionId, (ev) => {
					if (ev.event !== "channel_message") return;
					const n = ev.notice;
					const label = n.channelName ? `#${n.channelName}` : n.channelId;
					const msg = `[cant channel ${label} from ${n.fromHandle}]\n${n.body}\nReply with: (channels/send "${n.channelId}" "...")`;
					void session.followUp(msg).catch(() => undefined);
				})
				.catch(() => undefined);
		}

		try {
			await session.prompt(buildWorkerPrompt(spec));
			if (signal.aborted) throw new Error("Pi worker cancelled.");
			const text = finalText.trim();
			const data = spec.outputSchema ? tryExtractJson(text) : undefined;
			return { text, ...(data !== undefined ? { data } : {}) };
		} finally {
			if (timer) clearTimeout(timer);
			signal.removeEventListener("abort", onAbort);
			unsubComms?.();
			unsubscribe();
			session.dispose();
			void cantClient.repl({ sessionId: workerSessionId, cwd: spec.cwd }, { op: "close" }).catch(() => undefined);
		}
	}

	dispose(): void {
		this.setActiveEval();
		this.handle?.close();
		this.handle = undefined;
	}
}
