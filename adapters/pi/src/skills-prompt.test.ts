import assert from "node:assert/strict";
import test from "node:test";
import { fileURLToPath } from "node:url";
import {
	loadSkillsFromDir,
	type BeforeAgentStartEvent,
	type BeforeAgentStartEventResult,
	type ExtensionAPI,
	type ExtensionContext,
	type ExtensionHandler,
	type Skill,
} from "@earendil-works/pi-coding-agent";
import { createCantPiExtension, type CantPiOptions } from "./index.ts";

const { skills } = loadSkillsFromDir({
	dir: fileURLToPath(new URL("../../../skills/steel/", import.meta.url)),
	source: "test",
});
const steel = skills[0]!;
const explicitOnly = { ...steel, name: "manual-only", disableModelInvocation: true };

async function preparePrompt(options: CantPiOptions, selectedTools: string[], loadedSkills: Skill[]) {
	let beforeStart: ExtensionHandler<BeforeAgentStartEvent, BeforeAgentStartEventResult> | undefined;
	const pi = {
		registerCommand() {},
		registerTool() {},
		on(name: string, handler: typeof beforeStart) {
			if (name === "before_agent_start") beforeStart = handler;
		},
	} as unknown as ExtensionAPI;
	createCantPiExtension(options)(pi);
	const systemPromptOptions: BeforeAgentStartEvent["systemPromptOptions"] = {
		cwd: process.cwd(),
		selectedTools: [...selectedTools],
		hiddenTools: [],
		toolSnippets: {},
		toolGuidelines: {},
		promptGuidelines: [],
		appendSystemPrompt: "",
		sections: {},
		contextFiles: [],
		skills: loadedSkills,
	};
	await beforeStart?.({
		type: "before_agent_start", messageType: "user", prompt: "test", systemPrompt: "", systemPromptOptions,
	}, {} as ExtensionContext);
	assert.deepEqual(systemPromptOptions.selectedTools, selectedTools);
	return systemPromptOptions.sections.skills;
}

test("strict mode advertises discovered skills through the REPL without adding direct tools", async () => {
	const section = await preparePrompt({}, ["repl"], [steel, explicitOnly]);
	assert.ok(section);
	assert.match(section, /repl.*files\/read/);
	assert.match(section, /<name>cant-steel<\/name>/);
	assert.ok(section.includes(steel.filePath));
	assert.doesNotMatch(section, /manual-only/);
});

test("Cant leaves the skills section alone when Pi handles it or there is no REPL reader", async () => {
	assert.equal(await preparePrompt({ defaultMode: "hybrid" }, ["repl", "read", "bash"], skills), undefined);
	assert.equal(await preparePrompt({ defaultMode: "off" }, ["read"], skills), undefined);
	assert.equal(await preparePrompt({}, ["repl", "read"], skills), undefined);
	assert.equal(await preparePrompt({}, [], skills), undefined);
	assert.equal(await preparePrompt({ disabledNamespaces: ["files/read"] }, ["repl"], skills), undefined);
});

test("no skills section is injected when every skill is explicit-only", async () => {
	assert.equal(await preparePrompt({}, ["repl"], [explicitOnly]), undefined);
});
