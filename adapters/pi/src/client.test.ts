import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { CantClient, type DaemonEvent } from "./client.ts";

test("attachAdapter rejects immediately when daemon returns an error frame (e.g. stale daemon)", async () => {
	const sock = join(tmpdir(), `cant-stale-test-${randomUUID().slice(0, 8)}.sock`);
	const server = createServer((conn) => {
		conn.once("data", () => {
			conn.write(`${JSON.stringify({ type: "error", message: "Invalid request: unknown variant `attach_adapter`" })}\n`);
		});
	});
	await new Promise<void>((resolve) => server.listen(sock, resolve));
	try {
		const client = new CantClient({ socketPath: sock });
		const raced = await Promise.race([
			client
				.attachAdapter({
					sessionId: "s1",
					onInvokeCapability: async () => null,
				})
				.then(() => "attached")
				.catch((err) => `rejected:${err instanceof Error ? err.message : String(err)}`),
			new Promise<string>((r) => setTimeout(() => r("HUNG"), 500)),
		]);
		assert.notEqual(raced, "HUNG", "attachAdapter hung on daemon error response!");
		assert.match(raced, /^rejected:/);
	} finally {
		server.close();
		rmSync(sock, { force: true });
	}
});

test("subscribe notifies onClose when daemon drops the connection after ack, but not on manual unsubscribe", async () => {
	const sock = join(tmpdir(), `cant-sub-close-${randomUUID().slice(0, 8)}.sock`);
	const conns: import("node:net").Socket[] = [];
	const server = createServer((conn) => {
		conns.push(conn);
		conn.once("data", () => {
			conn.write(`${JSON.stringify({ type: "ok" })}\n`);
		});
	});
	await new Promise<void>((resolve) => server.listen(sock, resolve));
	try {
		const client = new CantClient({ socketPath: sock });
		let dropCloses = 0;
		const unsub1 = await client.subscribe("s1", () => {}, {
			onClose: () => {
				dropCloses++;
			},
		});
		conns[0]!.destroy();
		for (let i = 0; i < 20 && dropCloses === 0; i++) {
			await new Promise((r) => setTimeout(r, 10));
		}
		assert.equal(dropCloses, 1, "expected onClose to fire once on unexpected disconnect");
		unsub1();

		let manualCloses = 0;
		const unsub2 = await client.subscribe("s1", () => {}, {
			onClose: () => {
				manualCloses++;
			},
		});
		unsub2();
		await new Promise((r) => setTimeout(r, 30));
		assert.equal(manualCloses, 0, "manual unsubscribe must not fire onClose");
	} finally {
		for (const c of conns) c.destroy();
		server.close();
		rmSync(sock, { force: true });
	}
});

test("CantClient reverse-RPC external capabilities, Pi worker driver, and live Subscribe events", async () => {
	const sock = join(tmpdir(), `cant-pi-test-${randomUUID().slice(0, 8)}.sock`);
	const client = new CantClient({ socketPath: sock });
	const sessionId = `pi-test-${randomUUID().slice(0, 8)}`;
	const cwd = process.cwd();

	try {
		const pong = await client.ping();
		assert.ok(pong.languages.includes("steel"));
		assert.ok(pong.drivers.includes("pi"));
		assert.ok(pong.drivers.includes("agy"));

		const events: DaemonEvent[] = [];
		const unsub = await client.subscribe(sessionId, (ev) => events.push(ev));

		const adapter = await client.attachAdapter({
			sessionId,
			workerDriver: "pi",
			initialCapabilities: [
				{
					toolName: "mcp__demo__echo",
					apiPath: ["mcp", "demo", "echo"],
					description: "Echo a message back from the Pi adapter",
					paramsSchema: {
						type: "object",
						additionalProperties: false,
						required: ["msg"],
						properties: { msg: { type: "string" } },
					},
					steelPositional: ["msg"],
				},
			],
			async onInvokeCapability(toolName, args) {
				assert.equal(toolName, "mcp__demo__echo");
				const record = args as { msg: string };
				return { echoed: `pi:${record.msg}` };
			},
			async onRunWorker(spec, emitProgress) {
				emitProgress(`working on ${spec.name}\n`);
				return {
					text: `done:${spec.task}`,
					data: { driver: "pi", ok: true },
				};
			},
		});

		try {
			// 1. Call Pi's reverse-RPC external MCP capability from from the Steel runner!
			const mcpReply = await client.repl(
				{ sessionId, cwd, language: "steel", workerDriver: "pi" },
				{ op: "eval", code: `(get (mcp/demo/echo "hello") 'echoed)` },
			);
			assert.equal(mcpReply.isError, false, mcpReply.text);
			assert.match(mcpReply.text, /pi:hello/);

			// 2. Call foreground agents/run routed to the attached "pi" worker driver!
			const runReply = await client.repl(
				{ sessionId, cwd, language: "steel", workerDriver: "pi" },
				{ op: "eval", code: `(get (get (agents/run "check-it") 'data) 'driver)` },
			);
			assert.equal(runReply.isError, false, runReply.text);
			assert.match(runReply.text, /pi/);

			// 3. Launch a background delegate via "pi" driver and verify Subscribe receives events!
			const delReply = await client.repl(
				{ sessionId, cwd, language: "steel", workerDriver: "pi" },
				{ op: "eval", code: `(define d (delegate/start "bg-task" #:name "bg-test")) (get d 'id)` },
			);
			assert.equal(delReply.isError, false, delReply.text);

			// Wait briefly for the async worker & Subscribe events to arrive.
			for (let i = 0; i < 30 && !events.some((e) => e.event === "job_completed"); i++) {
				await new Promise((r) => setTimeout(r, 50));
			}
			assert.ok(events.some((e) => e.event === "job_updated"), "expected job_updated event");
			assert.ok(events.some((e) => e.event === "job_output"), "expected job_output event");
			assert.ok(events.some((e) => e.event === "job_completed"), "expected job_completed event");

			const resReply = await client.repl(
				{ sessionId, cwd, language: "steel", workerDriver: "pi" },
				{ op: "eval", code: `(get (delegate/result (get d 'id)) 'output)` },
			);
			assert.equal(resReply.isError, false, resReply.text);
			assert.match(resReply.text, /done:bg-task/);
		} finally {
			adapter.close();
			unsub();
		}
	} finally {
		await client.request({ type: "shutdown" }).catch(() => undefined);
		rmSync(sock, { force: true });
	}
});
