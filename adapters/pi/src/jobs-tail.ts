import { Text } from "@earendil-works/pi-tui";
import { isActiveJob, type DisplayJob } from "./jobs-providers.ts";
import { terminalText } from "./rendering.ts";

const REFRESH_MS = 500;

export class JobTail {
	job: DisplayJob;
	status: string;
	private output?: string;
	private error?: string;
	private paused?: { output: string; offset: number };
	private rendered?: { output: string; maxOffset: number };
	private readonly text = new Text("", 0, 0);
	private timer?: ReturnType<typeof setTimeout>;
	private refreshing = false;
	private refreshQueued = false;
	private disposed = false;

	private readonly requestRender: () => void;

	constructor(job: DisplayJob, requestRender: () => void) {
		this.job = job;
		this.requestRender = requestRender;
		this.status = job.status;
		void this.refresh();
	}

	updateStatus(status: string): void {
		if (this.status !== status) {
			this.status = status;
			this.requestRender();
		}
		void this.refresh();
	}

	updateExternalJob(job: DisplayJob): void {
		this.job = job;
		// Active jobs already refresh on a timer. Fetch immediately on a status change.
		if (job.status !== this.status) this.updateStatus(job.status);
	}

	private async refresh(): Promise<void> {
		if (this.disposed) return;
		clearTimeout(this.timer);
		if (this.refreshing) {
			this.refreshQueued = true;
			return;
		}
		this.refreshing = true;
		let changed = false;
		try {
			const { output, status } = await this.job.tail();
			// A status event during the read requests a fresher snapshot.
			if (!this.disposed && !this.refreshQueued) {
				const nextOutput = terminalText(output);
				const nextStatus = status ?? this.status;
				changed = this.output !== nextOutput || this.status !== nextStatus || this.error !== undefined;
				this.output = nextOutput;
				this.status = nextStatus;
				this.error = undefined;
			}
		} catch (err) {
			if (!this.disposed && !this.refreshQueued) {
				const error = terminalText(err instanceof Error ? err.message : String(err));
				changed = this.error !== error;
				this.error = error;
			}
		} finally {
			this.refreshing = false;
			if (!this.disposed) {
				if (this.refreshQueued) {
					this.refreshQueued = false;
					void this.refresh();
				} else {
					if (isActiveJob(this.status) || this.error !== undefined) {
						this.timer = setTimeout(() => void this.refresh(), REFRESH_MS);
					}
					if (changed) this.requestRender();
				}
			}
		}
	}

	scrollBy(delta: number): void {
		const rendered = this.rendered;
		if (!rendered) return;
		if (!this.paused && delta > 0 && rendered.maxOffset > 0) {
			// Freeze what the user actually saw, even if a new snapshot arrived before the next render.
			this.paused = { output: rendered.output, offset: 0 };
		}
		if (this.paused) {
			this.paused.offset = Math.max(0, Math.min(rendered.maxOffset, this.paused.offset + delta));
		}
	}

	follow(): void {
		this.paused = undefined;
	}

	render(width: number, height: number): string[] {
		const output = this.paused?.output ?? this.output;
		this.text.setText(output === undefined
			? this.error === undefined ? "Loading…" : `Error: ${this.error}`
			: output || "(no output yet)");
		const lines = this.text.render(width);
		const maxOffset = Math.max(0, lines.length - height);
		this.rendered = output === undefined ? undefined : { output, maxOffset };
		if (this.paused) this.paused.offset = Math.min(this.paused.offset, maxOffset);
		const end = lines.length - (this.paused?.offset ?? 0);
		return lines.slice(Math.max(0, end - height), end);
	}

	footer(): string {
		const mode = this.paused
			? `Paused · End to follow${this.paused.output !== this.output ? " · New output" : ""}`
			: "Following · ↑/PgUp pause";
		const error = this.error === undefined ? "" : " · Refresh failed · retrying";
		return `${mode}${error}${this.job.cancel && isActiveJob(this.status) ? " · k cancel" : ""} · Esc back`;
	}

	dispose(): void {
		this.disposed = true;
		clearTimeout(this.timer);
	}
}
