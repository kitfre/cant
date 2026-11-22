import { highlightCode, type Theme } from "@earendil-works/pi-coding-agent";
import { Text, type Component } from "@earendil-works/pi-tui";

const TERMINAL_CONTROLS = /[\x00-\x08\x0b-\x1f\x7f-\x9f]/g;

/** Preserve line breaks while escaping untrusted terminal control sequences. */
export function terminalText(text: string): string {
	return text.replace(/\r\n/g, "\n").replace(TERMINAL_CONTROLS, (character) => `\\u{${character.codePointAt(0)!.toString(16)}}`);
}

export function cachedRender(render: Component["render"]): Component {
	let cache: { width: number; lines: string[] } | undefined;
	return {
		invalidate() {
			cache = undefined;
		},
		render(width) {
			if (!cache || cache.width !== width) cache = { width, lines: render(width) };
			return cache.lines;
		},
	};
}

export function renderCodeCall(label: string, source: string, language: string | undefined, theme: Theme): Component {
	return cachedRender((width) =>
		new Text(`${theme.fg("toolTitle", theme.bold(`${label} `))}${highlightCode(terminalText(source), language).join("\n")}`, 0, 0).render(
			width,
		),
	);
}
