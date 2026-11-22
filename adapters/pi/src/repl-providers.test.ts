import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { test } from "node:test";
import { collectReplCapabilities, REPL_COLLECT_EVENT, type ReplCapability } from "./repl-providers.ts";

const provider: ReplCapability = {
	spec: { toolName: "external_test", apiPath: ["example", "inspect"], description: "Example" },
	invoke: async () => "result",
};

test("Pi extensions can contribute explicit REPL-only capabilities in interactive sessions", async () => {
	const events = new EventEmitter();
	events.on(REPL_COLLECT_EVENT, (providers: ReplCapability[]) => providers.push(provider));
	const capabilities = collectReplCapabilities(events, true);
	assert.deepEqual(capabilities, [provider]);
	assert.equal(await capabilities[0]!.invoke({}, {} as never, new AbortController().signal), "result");
	assert.deepEqual(collectReplCapabilities(events, false), []);
});
