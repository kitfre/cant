import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { ExternalCapabilitySpec } from "./client.ts";

/** Trusted Pi extensions append to the array synchronously. No direct Pi tool is registered. */
export const REPL_COLLECT_EVENT = "cant:repl:collect:v1";

export interface ReplCapability {
	spec: ExternalCapabilitySpec;
	invoke(args: unknown, ctx: ExtensionContext, signal: AbortSignal): Promise<unknown>;
}

export function collectReplCapabilities(
	events: { emit(event: string, data: unknown): unknown },
	interactive: boolean,
): ReplCapability[] {
	const providers: ReplCapability[] = [];
	if (interactive) events.emit(REPL_COLLECT_EVENT, providers);
	return providers;
}
