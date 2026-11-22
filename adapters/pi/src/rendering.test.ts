import assert from "node:assert/strict";
import { test } from "node:test";
import { cachedRender } from "./rendering.ts";

test("cachedRender reuses the component's readonly frame until invalidated", () => {
	let renders = 0;
	const component = cachedRender((width) => Object.freeze([`frame ${++renders} at ${width}`]));
	const frame = component.render(40);
	assert.deepEqual(frame, ["frame 1 at 40"]);
	assert.equal(component.render(40), frame);
	assert.deepEqual(component.render(80), ["frame 2 at 80"]);
	component.invalidate();
	assert.deepEqual(component.render(80), ["frame 3 at 80"]);
});
