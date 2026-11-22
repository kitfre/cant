import { randomUUID } from "node:crypto";
import { StringEnum } from "@earendil-works/pi-ai";
import {
	formatSkillsForPrompt,
	keyHint,
	type ExtensionAPI,
	type ExtensionContext,
	type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Text, type Component } from "@earendil-works/pi-tui";
import { Type } from "typebox";
import { CantClient, type ReplRequest } from "./client.ts";
import { registerJobsUi } from "./jobs-overlay.ts";
import { PiBridge } from "./pi-bridge.ts";
import { collectReplCapabilities } from "./repl-providers.ts";
import { cachedRender, renderCodeCall, terminalText } from "./rendering.ts";
import { registerReplOverlayCommand, ReplTranscript } from "./repl-overlay.ts";

export type CantMode = "strict" | "hybrid" | "off";

const REPLACED_DIRECT_TOOLS = new Set(["read", "write", "edit", "grep", "find", "ls", "bash"]);

const REPL_PARAMETERS = Type.Object(
	{
		op: StringEnum(["eval", "inspect", "search", "describe", "history", "reset", "close"] as const),
		code: Type.Optional(Type.String({ maxLength: 262_144, description: "Required for eval: Steel Scheme expressions." })),
		inputs: Type.Optional(Type.Record(Type.String(), Type.String(), { description: "Eval-only literal strings; (input 'key)." })),
		name: Type.Optional(Type.String({ maxLength: 1024, description: "Binding name for inspect; required for describe." })),
		query: Type.Optional(Type.String({ maxLength: 4096, description: "Search text." })),
		origin: Type.Optional(StringEnum(["user", "all"] as const)),
		id: Type.Optional(Type.Integer({ minimum: 1, maximum: Number.MAX_SAFE_INTEGER, description: "History submission id." })),
		offset: Type.Optional(Type.Integer({ minimum: 0, maximum: Number.MAX_SAFE_INTEGER })),
		limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 50 })),
	},
	{ additionalProperties: false },
);

export interface CantPiOptions {
	socketPath?: string;
	binaryPath?: string;
	defaultMode?: CantMode;
	defaultLanguage?: string;
	defaultDriver?: "pi" | "agy" | string;
	disabledNamespaces?: string[];
}

export function createCantPiExtension(options: CantPiOptions = {}) {
	return (pi: ExtensionAPI) => {
		const client = new CantClient({
			socketPath: options.socketPath,
			binaryPath: options.binaryPath,
		});
		const transcript = new ReplTranscript();
		const sessionId = `pi-${randomUUID().slice(0, 12)}`;
		let mode: CantMode = options.defaultMode ?? "strict";
		let language = options.defaultLanguage ?? "steel";
		let workerDriver = options.defaultDriver ?? "pi";
		const disabledNamespaces = options.disabledNamespaces ?? [];
		let cachedDescription =
			"A persistent Steel Scheme REPL hosted by the local cant daemon. Call with op='eval', 'inspect', 'search', 'describe', 'history', 'reset', or 'close'.";
		let startupTools: string[] | undefined;
		let connecting: Promise<void> | undefined;

		const bridge = new PiBridge(client, sessionId);
		const jobsUi = registerJobsUi(pi, client, sessionId);

		const workspaceKey = (cwd: string) => ({
			sessionId,
			cwd,
			language,
			disabledNamespaces,
			workerDriver,
		});

		const syncActiveTools = () => {
			if (!startupTools) return;
			const current = pi.getActiveTools();
			const base = [...new Set([...startupTools, ...current])];
			if (mode === "off") {
				pi.setActiveTools(base.filter((name) => name !== "repl"));
				return;
			}
			if (mode === "strict") {
				const filtered = base.filter((name) => !REPLACED_DIRECT_TOOLS.has(name) && !name.startsWith("mcp__"));
				if (!filtered.includes("repl")) filtered.push("repl");
				pi.setActiveTools(filtered);
				return;
			}
			// hybrid
			const hybrid = [...base];
			if (!hybrid.includes("repl")) hybrid.push("repl");
			pi.setActiveTools(hybrid);
		};

		const refreshDescription = async () => {
			try {
				const info = await client.toolInfo({ language, disabledNamespaces, sessionId });
				if (info.description) cachedDescription = info.description;
			} catch {
				// Daemon might not be started yet; first call will auto-start it.
			}
		};

		const replOverlay = registerReplOverlayCommand(pi, {
			language: () => language,
			transcript,
			run: (request, ctx, signal) => client.repl(workspaceKey(ctx.cwd), request, signal),
		});

		pi.registerTool({
			name: "repl",
			label: "Cant Steel REPL",
			description: cachedDescription,
			promptSnippet: "Evaluate and inspect persistent session-local Steel definitions via cant daemon",
			promptGuidelines: [
				"REPL compounds iterative work by retaining parsed data, query results, narrowed candidates, definitions, and helpers across turns.",
				"Use background/start, delegate/start, and agents/start-flow for background work; completed jobs deliver notifications automatically — do not poll.",
				"Use peers/list, peers/announce, and channels/* (open, send, read, wait, join, close) to discover and converse with other local agents across repos/harnesses, or pass #:channel to delegate/start and agents/start* to join workers into a shared channel.",
				"Use (capabilities/search \"...\") or (capabilities/describe 'files/read) for unfamiliar capabilities.",
			],
			parameters: REPL_PARAMETERS,
			executionMode: "sequential",
			prepareLoadout: (loadout) => {
				bridge.observeLoadout(loadout);
				return {
					descriptions: { repl: cachedDescription },
				};
			},
			async execute(_toolCallId, params, signal, _onUpdate, ctx) {
				await connecting;
				signal?.throwIfAborted();
				bridge.setContext(ctx);
				bridge.setActiveEval(ctx, signal);
				const req = params as ReplRequest;
				try {
					const reply = await client.repl(workspaceKey(ctx.cwd), req, signal);
					transcript.record(req, reply, "agent");
					return {
						content: [{ type: "text", text: reply.text }],
						details: {
							workspaceId: reply.workspaceId,
							generation: reply.generation,
							submissionId: reply.submissionId,
							status: reply.status,
						},
						isError: reply.isError,
					};
				} finally {
					bridge.setActiveEval();
				}
			},
			renderCall(args, theme) {
				if (args.op === "eval") return renderCodeCall("repl eval", args.code ?? "", "scheme", theme);
				return cachedRender((width) => {
					const parts = ["repl", args.op ?? ""];
					if (args.name) parts.push(args.name);
					if (args.query !== undefined) parts.push(JSON.stringify(args.query));
					if (args.id !== undefined) parts.push(`#${args.id}`);
					for (const field of ["origin", "offset", "limit"] as const) {
						if (args[field] !== undefined) parts.push(`${field}=${args[field]}`);
					}
					return new Text(theme.fg("toolTitle", theme.bold(terminalText(parts.join(" ")))), 0, 0).render(width);
				});
			},
			renderResult(result, { isPartial, expanded }, theme): Component {
				return cachedRender((width) => {
					if (isPartial) return new Text(theme.fg("warning", "Cant REPL working…"), 0, 0).render(width);
					const text = result.content
						.filter((block) => block.type === "text")
						.map((block) => block.text)
						.join("\n");
					const lines = new Text(terminalText(text), 0, 0).render(width);
					if (expanded || lines.length <= 12) return lines;
					return [...lines.slice(0, 12), ...new Text(theme.fg("muted", keyHint("app.tools.expand", "show full output")), 0, 0).render(width)];
				});
			},
		} as ToolDefinition<typeof REPL_PARAMETERS>);

		const cantCommandHandler = async (args: string, ctx: ExtensionContext) => {
			bridge.setContext(ctx);
			jobsUi.bindContext(ctx);
			const [sub, arg] = args.trim().split(/\s+/).filter(Boolean);
			if (!sub || sub === "status") {
				try {
					const pong = await client.ping();
					ctx.ui.notify(
						`Cant v${pong.version} (pid ${pong.pid}) · mode=${mode} · lang=${language} · driver=${workerDriver} · drivers=[${pong.drivers.join(", ")}] · socket=${client.socketPath}`,
						"info",
					);
				} catch (err) {
					ctx.ui.notify(`Cant daemon unavailable: ${err instanceof Error ? err.message : String(err)}`, "error");
				}
				return;
			}
			if (sub === "strict" || sub === "hybrid" || sub === "off") {
				mode = sub;
				syncActiveTools();
				ctx.ui.notify(`Cant mode set to ${mode}.`, "info");
				return;
			}
			if (sub === "driver") {
				if (!arg) {
					ctx.ui.notify(`Active worker driver: ${workerDriver}. Usage: /cant driver pi|agy`, "info");
					return;
				}
				workerDriver = arg;
				ctx.ui.notify(`Cant worker driver set to ${workerDriver}.`, "info");
				return;
			}
			ctx.ui.notify("Usage: /cant [status|strict|hybrid|off|driver <pi|agy>]", "warning");
		};

		pi.registerCommand("cant", {
			description: "Inspect or configure Cant REPL mode and worker driver. Usage: /cant [status|strict|hybrid|off|driver <pi|agy>]",
			handler: cantCommandHandler,
		});
		pi.registerCommand("code-mode", {
			description: "Alias for /cant.",
			handler: cantCommandHandler,
		});

		const connectDaemon = async (ctx: ExtensionContext) => {
			const tryConnect = async () => {
				await bridge.attach();
				await jobsUi.startSubscription(ctx);
				await refreshDescription();
			};
			try {
				await tryConnect();
			} catch (err) {
				const msg = err instanceof Error ? err.message : String(err);
				// Auto-upgrade a stale daemon from an older build instead of hanging or failing.
				if (/unknown variant|missing field|AttachAdapter|Subscribe/i.test(msg)) {
					await client.request({ type: "shutdown" }).catch(() => undefined);
					await new Promise((r) => setTimeout(r, 100));
					await tryConnect();
					return;
				}
				throw err;
			}
		};

		pi.on("before_agent_start", ({ systemPromptOptions: prompt }) => {
			if (mode !== "strict" || !prompt || prompt.sections.skills) return;
			const hasDirectTool = (name: string) => prompt.selectedTools.includes(name) && !prompt.hiddenTools.includes(name);
			if (!hasDirectTool("repl") || hasDirectTool("read") || hasDirectTool("bash")) return;
			if (disabledNamespaces.some((name) => ["files", "files.read"].includes(name.trim().replaceAll("/", ".")))) return;

			// Pi omits its default skills section when neither read nor bash is active.
			const skills = formatSkillsForPrompt(prompt.skills, "indirect").trim();
			if (!skills) return;
			prompt.sections.skills = `Use repl with op="eval" and files/read to load a skill's SKILL.md by its absolute path.\n\n${skills}`;
		});

		pi.on("session_start", (_event, ctx) => {
			startupTools ??= pi.getActiveTools();
			bridge.setProviders(collectReplCapabilities(pi.events, ctx.mode === "tui" && ctx.hasUI));
			bridge.setContext(ctx);
			jobsUi.bindContext(ctx);
			syncActiveTools();
			// Connect asynchronously so daemon startup/upgrade can never hang Pi's UI.
			connecting = connectDaemon(ctx).catch((err) => {
				console.error("[cant-pi] Failed connecting to cant daemon:", err);
			});
		});

		pi.on("session_shutdown", async () => {
			await replOverlay.close();
			jobsUi.dispose();
			bridge.dispose();
		});
	};
}

export default createCantPiExtension();
