import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { createConnection, type Socket } from "node:net";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export type ReplOrigin = "user" | "all";

export type ReplRequest =
	| { op: "eval"; code: string; inputs?: Record<string, string> }
	| { op: "inspect"; name?: string; origin?: ReplOrigin; offset?: number; limit?: number }
	| { op: "search"; query?: string; origin?: ReplOrigin; offset?: number; limit?: number }
	| { op: "describe"; name: string }
	| { op: "history"; id?: number; offset?: number; limit?: number }
	| { op: "reset" }
	| { op: "close" };

export interface WorkspaceKey {
	sessionId: string;
	cwd: string;
	language?: string;
	disabledNamespaces?: string[];
	workerDriver?: string;
}

export interface ReplToolResponse {
	text: string;
	isError: boolean;
	workspaceId: string;
	generation: number;
	submissionId?: number;
	status: string;
	rawValue?: unknown;
}

export interface JobSummary {
	id: string;
	kind: string;
	name: string;
	sessionId: string;
	status: string;
	elapsedMs: number;
	summary: string;
}

export interface CompletionNotice {
	jobId: string;
	kind: string;
	name: string;
	status: string;
	notify: string;
	summary: string;
	completionToken?: string;
}

export interface ChannelNotice {
	channelId: string;
	channelName?: string;
	topic?: string;
	seq: number;
	from: string;
	fromHandle: string;
	fromHarness: string;
	body: string;
	truncated: boolean;
	timestampMs: number;
}

export type DaemonEvent =
	| { event: "job_updated"; job: JobSummary }
	| { event: "job_output"; id: string; chunk: string }
	| { event: "job_completed"; notice: CompletionNotice }
	| { event: "channel_message"; notice: ChannelNotice };

export interface ExternalCapabilitySpec {
	toolName: string;
	apiPath: string[];
	description: string;
	paramsSchema?: unknown;
	resultSchema?: unknown;
	steelPositional?: string[];
}

export interface ExternalWorkerSpec {
	name: string;
	task: string;
	input?: unknown;
	cwd: string;
	model?: string;
	timeoutSecs?: number;
	outputSchema?: unknown;
	capabilities?: string[];
	sessionId?: string;
	channels?: string[];
}

export interface AdapterHandlers {
	sessionId: string;
	workerDriver?: string;
	initialCapabilities?: ExternalCapabilitySpec[];
	onInvokeCapability: (toolName: string, args: unknown) => Promise<unknown>;
	onRunWorker?: (
		spec: ExternalWorkerSpec,
		emitProgress: (chunk: string) => void,
		signal: AbortSignal,
	) => Promise<{ text: string; data?: unknown; usage?: unknown }>;
}

export interface AdapterHandle {
	adapterId: string;
	updateCapabilities(capabilities: ExternalCapabilitySpec[]): void;
	close(): void;
}

export interface SubscribeOptions {
	timeoutMs?: number;
	onClose?: () => void;
}

export function defaultSocketPath(): string {
	const explicit = process.env.CANT_SOCKET?.trim();
	if (explicit) return explicit;
	if (process.platform === "darwin") {
		return join(homedir(), "Library", "Caches", "cant", "cant.sock");
	}
	const runtime = process.env.XDG_RUNTIME_DIR?.trim();
	if (runtime) return join(runtime, "cant.sock");
	const uid = typeof process.getuid === "function" ? process.getuid() : 0;
	return join(tmpdir(), `cant-${uid}.sock`);
}

export function resolveCantBinary(): string {
	const explicit = process.env.CANT_BIN?.trim();
	if (explicit) return explicit;
	const here = dirname(fileURLToPath(import.meta.url));
	const candidates = [
		resolve(here, "../../../target/release/cant"),
		resolve(here, "../../../target/debug/cant"),
	];
	for (const candidate of candidates) {
		if (existsSync(candidate)) return candidate;
	}
	return "cant";
}

function connectSocket(socketPath: string): Promise<Socket> {
	return new Promise((resolveSocket, reject) => {
		const socket = createConnection(socketPath);
		const onError = (err: Error) => {
			socket.removeListener("connect", onConnect);
			reject(err);
		};
		const onConnect = () => {
			socket.removeListener("error", onError);
			resolveSocket(socket);
		};
		socket.once("error", onError);
		socket.once("connect", onConnect);
	});
}

function readLines(socket: Socket, onLine: (line: string) => void): void {
	let buffer = "";
	socket.setEncoding("utf8");
	socket.on("data", (chunk: string) => {
		buffer += chunk;
		for (;;) {
			const idx = buffer.indexOf("\n");
			if (idx === -1) break;
			const line = buffer.slice(0, idx).trim();
			buffer = buffer.slice(idx + 1);
			if (line) onLine(line);
		}
	});
}

export class CantClient {
	readonly socketPath: string;
	readonly binaryPath: string;

	constructor(options?: { socketPath?: string; binaryPath?: string }) {
		this.socketPath = options?.socketPath ?? defaultSocketPath();
		this.binaryPath = options?.binaryPath ?? resolveCantBinary();
	}

	async connectOrSpawn(): Promise<Socket> {
		try {
			return await connectSocket(this.socketPath);
		} catch {
			const child = spawn(this.binaryPath, ["daemon", "--socket", this.socketPath], {
				detached: true,
				stdio: "ignore",
			});
			child.unref();
			for (let attempt = 0; attempt < 50; attempt++) {
				await new Promise((r) => setTimeout(r, 50));
				try {
					return await connectSocket(this.socketPath);
				} catch {
					// retry
				}
			}
			throw new Error(`Timed out waiting for cant daemon on ${this.socketPath}`);
		}
	}

	async request<T = any>(payload: Record<string, unknown>, signal?: AbortSignal): Promise<T> {
		if (signal?.aborted) throw new DOMException("Aborted", "AbortError");
		const socket = await this.connectOrSpawn();
		return new Promise<T>((resolveResp, rejectResp) => {
			let settled = false;
			const finish = (fn: () => void) => {
				if (settled) return;
				settled = true;
				signal?.removeEventListener("abort", onAbort);
				socket.destroy();
				fn();
			};
			const onAbort = () => finish(() => rejectResp(new DOMException("Aborted", "AbortError")));
			signal?.addEventListener("abort", onAbort, { once: true });
			socket.once("error", (err) => finish(() => rejectResp(err)));
			socket.once("close", () => finish(() => rejectResp(new Error("Cant daemon closed connection before replying."))));
			readLines(socket, (line) => {
				try {
					const parsed = JSON.parse(line);
					if (parsed?.type === "error") {
						finish(() => rejectResp(new Error(parsed.message ?? "Cant daemon error")));
					} else {
						finish(() => resolveResp(parsed as T));
					}
				} catch (err) {
					finish(() => rejectResp(err));
				}
			});
			socket.write(`${JSON.stringify(payload)}\n`);
		});
	}

	async ping(): Promise<{ version: string; pid: number; languages: string[]; drivers: string[] }> {
		return this.request({ type: "ping" });
	}

	async toolInfo(options?: { language?: string; disabledNamespaces?: string[]; sessionId?: string }): Promise<{
		name: string;
		description: string;
		inputSchema: unknown;
	}> {
		const res = await this.request<{ type: string; name: string; description: string; input_schema: unknown }>({
			type: "tool_info",
			language: options?.language ?? "steel",
			disabled_namespaces: options?.disabledNamespaces ?? [],
			...(options?.sessionId ? { session_id: options.sessionId } : {}),
		});
		return { name: res.name, description: res.description, inputSchema: res.input_schema };
	}

	async repl(key: WorkspaceKey, request: ReplRequest, signal?: AbortSignal): Promise<ReplToolResponse> {
		const res = await this.request<{
			type: "repl";
			text: string;
			isError: boolean;
			workspaceId: string;
			generation: number;
			submissionId?: number;
			status: string;
			rawValue?: unknown;
		}>({ type: "repl", key, request }, signal);
		return {
			text: res.text,
			isError: res.isError,
			workspaceId: res.workspaceId,
			generation: res.generation,
			submissionId: res.submissionId,
			status: res.status,
			rawValue: res.rawValue,
		};
	}

	async listJobs(sessionId?: string): Promise<JobSummary[]> {
		const res = await this.request<{ type: "jobs"; jobs: JobSummary[] }>({
			type: "list_jobs",
			...(sessionId ? { session_id: sessionId } : {}),
		});
		return res.jobs ?? [];
	}

	async tailJob(id: string, maxBytes = 16384): Promise<{ id: string; kind: string; status: string; output: string }> {
		return this.request({ type: "job_tail", id, max_bytes: maxBytes });
	}

	async cancelJob(id: string): Promise<void> {
		await this.request({ type: "cancel_job", id });
	}

	/** Opens a live event push stream (`Subscribe`) for job updates, output deltas, and completions. */
	async subscribe(
		sessionId: string,
		onEvent: (event: DaemonEvent) => void,
		options: number | SubscribeOptions = 3_000,
	): Promise<() => void> {
		const timeoutMs = typeof options === "number" ? options : (options.timeoutMs ?? 3_000);
		const onClose = typeof options === "number" ? undefined : options.onClose;
		const socket = await this.connectOrSpawn();
		let closed = false;
		let closeNotified = false;
		await new Promise<void>((resolveSub, rejectSub) => {
			let acked = false;
			const notifyClose = () => {
				if (closed || closeNotified || !acked) return;
				closeNotified = true;
				onClose?.();
			};
			const timer = setTimeout(() => {
				if (acked) return;
				acked = true;
				socket.destroy();
				rejectSub(new Error("Timed out waiting for cant daemon Subscribe ack."));
			}, timeoutMs);
			timer.unref();
			socket.once("error", (err) => {
				clearTimeout(timer);
				if (!acked) {
					acked = true;
					socket.destroy();
					rejectSub(err);
				} else {
					notifyClose();
				}
			});
			socket.once("close", () => {
				clearTimeout(timer);
				if (!acked) {
					acked = true;
					rejectSub(new Error("Cant daemon closed before Subscribe ack."));
				} else {
					notifyClose();
				}
			});
			readLines(socket, (line) => {
				try {
					const msg = JSON.parse(line);
					if (!acked) {
						clearTimeout(timer);
						acked = true;
						if (msg?.type === "error") {
							socket.destroy();
							rejectSub(new Error(msg.message ?? "Subscribe rejected by cant daemon."));
						} else {
							socket.unref();
							resolveSub();
						}
						return;
					}
					if (msg?.type === "event" && msg.event) {
						onEvent(msg as DaemonEvent);
					}
				} catch {
					// ignore malformed frame
				}
			});
			socket.write(`${JSON.stringify({ type: "subscribe", session_id: sessionId })}\n`);
		});
		return () => {
			if (!closed) {
				closed = true;
				socket.destroy();
			}
		};
	}

	/** Opens a persistent bidirectional reverse-RPC connection for external capabilities & workers. */
	async attachAdapter(handlers: AdapterHandlers, timeoutMs = 3_000): Promise<AdapterHandle> {
		const adapterId = `pi_${randomUUID().slice(0, 12)}`;
		const socket = await this.connectOrSpawn();
		const activeWorkers = new Map<string, AbortController>();
		let closed = false;

		const send = (msg: Record<string, unknown>) => {
			if (!closed && !socket.destroyed) {
				socket.write(`${JSON.stringify(msg)}\n`);
			}
		};

		await new Promise<void>((resolveAttach, rejectAttach) => {
			let attached = false;
			const timer = setTimeout(() => {
				if (attached) return;
				attached = true;
				closed = true;
				socket.destroy();
				rejectAttach(new Error("Timed out waiting for cant daemon AttachAdapter ack."));
			}, timeoutMs);
			timer.unref();
			socket.once("error", (err) => {
				clearTimeout(timer);
				if (!attached) {
					attached = true;
					closed = true;
					socket.destroy();
					rejectAttach(err);
				}
			});
			socket.once("close", () => {
				clearTimeout(timer);
				for (const ctrl of activeWorkers.values()) ctrl.abort();
				activeWorkers.clear();
				if (!attached) {
					attached = true;
					closed = true;
					rejectAttach(new Error("Daemon closed before AttachAdapter ack."));
				}
			});

			readLines(socket, (line) => {
				let msg: any;
				try {
					msg = JSON.parse(line);
				} catch {
					return;
				}
				if (msg.type === "error" && !attached) {
					clearTimeout(timer);
					attached = true;
					closed = true;
					socket.destroy();
					rejectAttach(new Error(msg.message ?? "AttachAdapter rejected by cant daemon."));
					return;
				}
				if (msg.type === "adapter_attached") {
					if (!attached) {
						clearTimeout(timer);
						attached = true;
						socket.unref();
						resolveAttach();
					}
					return;
				}
				if (msg.type === "invoke_capability") {
					const callId: string = msg.call_id;
					void handlers
						.onInvokeCapability(msg.tool_name, msg.args)
						.then((value) => send({ type: "adapter_result", call_id: callId, ok: true, value }))
						.catch((err) =>
							send({
								type: "adapter_result",
								call_id: callId,
								ok: false,
								error: err instanceof Error ? err.message : String(err),
							}),
						);
					return;
				}
				if (msg.type === "run_worker") {
					const callId: string = msg.call_id;
					if (!handlers.onRunWorker) {
						send({ type: "adapter_result", call_id: callId, ok: false, error: "Adapter has no worker handler." });
						return;
					}
					const ctrl = new AbortController();
					activeWorkers.set(callId, ctrl);
					void handlers
						.onRunWorker(
							msg.spec as ExternalWorkerSpec,
							(chunk) => send({ type: "adapter_worker_progress", call_id: callId, chunk }),
							ctrl.signal,
						)
						.then((value) => send({ type: "adapter_result", call_id: callId, ok: true, value }))
						.catch((err) =>
							send({
								type: "adapter_result",
								call_id: callId,
								ok: false,
								error: err instanceof Error ? err.message : String(err),
							}),
						)
						.finally(() => activeWorkers.delete(callId));
					return;
				}
				if (msg.type === "cancel_worker") {
					activeWorkers.get(msg.call_id)?.abort();
				}
			});

			send({
				type: "attach_adapter",
				adapter_id: adapterId,
				session_id: handlers.sessionId,
				capabilities: handlers.initialCapabilities ?? [],
				...(handlers.workerDriver ? { worker_driver: handlers.workerDriver } : {}),
			});
		});

		return {
			adapterId,
			updateCapabilities(capabilities: ExternalCapabilitySpec[]) {
				send({ type: "update_capabilities", capabilities });
			},
			close() {
				if (closed) return;
				closed = true;
				for (const ctrl of activeWorkers.values()) ctrl.abort();
				activeWorkers.clear();
				socket.destroy();
			},
		};
	}
}
