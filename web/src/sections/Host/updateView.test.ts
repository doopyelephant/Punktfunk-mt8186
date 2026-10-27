import { expect, test } from "bun:test";
import type { UpdateStatus } from "@/api/gen/model";
import { APPLY_TIMEOUT_MS, updateCardView } from "./updateView";

const manifest = {
	version: "0.46.0",
	notes_url: "https://example.test/notes",
	published_at: "2026-10-01T00:00:00Z",
	serial: 7,
	stale: false,
};

const idle: UpdateStatus = {
	apply: "notify",
	available: false,
	channel: "stable",
	channel_hint: "apt upgrade punktfunk",
	check_disabled: false,
	current_version: "0.45.0",
	install_kind: "apt",
	not_published: false,
};

const status = (over: Partial<UpdateStatus>): UpdateStatus => ({
	...idle,
	...over,
});

const job = {
	received_bytes: 10,
	stage: "downloading",
	started_unix: 1,
	target_version: "0.46.0",
};

const failed = {
	ok: false,
	from: "0.45.0",
	to: "0.46.0",
	finished_unix: 5,
	stage: "applying",
};

const applying = { target: "0.46.0", startedAt: 1_000_000 };

const view = (
	s: UpdateStatus | undefined,
	a: typeof applying | null = null,
	error: unknown = undefined,
	now = 0,
) => updateCardView(s, a, error, now);

test("no status yet: nothing to draw, in flight only while an apply runs", () => {
	expect(view(undefined)).toMatchObject({
		inFlight: false,
		showBadge: false,
		body: null,
	});
	expect(view(undefined, applying).inFlight).toBe(true);
});

test("the version row says published, empty channel or never checked", () => {
	const latest = (o: Partial<UpdateStatus>) => view(status(o)).body?.latest;
	expect(latest({ manifest })).toEqual({
		kind: "published",
		version: "0.46.0",
		notesUrl: "https://example.test/notes",
	});
	expect(latest({ not_published: true })).toEqual({ kind: "empty-channel" });
	expect(latest({})).toEqual({ kind: "never-checked" });
	// A manifest wins over the flag; the host never sets both.
	expect(latest({ manifest, not_published: true })?.kind).toBe("published");
});

const panelCases: [string, Partial<UpdateStatus>, boolean, object][] = [
	["up to date", { manifest }, false, { kind: "up-to-date" }],
	["nothing known", {}, false, { kind: "none" }],
	["empty channel", { not_published: true }, false, { kind: "none" }],
	[
		"available, one-click",
		{ manifest, available: true, apply: "full" },
		false,
		{ kind: "apply" },
	],
	[
		"available, staged",
		{ manifest, available: true, apply: "staged" },
		false,
		{ kind: "apply" },
	],
	[
		"available, notify",
		{ manifest, available: true },
		false,
		{ kind: "how-to", optInHint: null },
	],
	[
		"available, notify, opt-in offered",
		{ manifest, available: true, opt_in_hint: "usermod -aG punktfunk-update" },
		false,
		{ kind: "how-to", optInHint: "usermod -aG punktfunk-update" },
	],
	[
		"an empty opt-in hint draws nothing",
		{ manifest, available: true, opt_in_hint: "" },
		false,
		{ kind: "how-to", optInHint: null },
	],
	[
		"unknown apply mode falls back to the command",
		{ manifest, available: true, apply: "other" },
		false,
		{ kind: "how-to", optInHint: null },
	],
	["host job running", { manifest, job }, false, { kind: "progress" }],
	[
		"progress beats an available one-click",
		{ manifest, available: true, apply: "full", job },
		false,
		{ kind: "progress" },
	],
	["apply accepted, no job yet", { manifest }, true, { kind: "progress" }],
];
for (const [name, s, applyingNow, panel] of panelCases) {
	test(`panel: ${name}`, () => {
		expect(view(status(s), applyingNow ? applying : null).body?.panel).toEqual(
			panel,
		);
	});
}

test("the badge shows for an available update unless one is in flight", () => {
	const badge = (s: Partial<UpdateStatus>, a = false) =>
		view(status(s), a ? applying : null).showBadge;
	expect(badge({ manifest, available: true })).toBe(true);
	expect(badge({ manifest })).toBe(false);
	expect(badge({ manifest, available: true, job })).toBe(false);
	expect(badge({ manifest, available: true }, true)).toBe(false);
});

test("an apply in flight hides the outcome notes and the check button", () => {
	const s = status({
		manifest,
		last_result: failed,
		last_error: "feed down",
		not_published: true,
		last_checked_unix: 99,
	});
	const idleBody = view(s).body;
	expect(idleBody).toMatchObject({
		lastResult: failed,
		lastError: "feed down",
		emptyChannel: true,
		check: { kind: "button", lastChecked: 99 },
	});
	expect(view(s, applying).body).toMatchObject({
		lastResult: null,
		lastError: null,
		emptyChannel: false,
		check: { kind: "none" },
	});
});

test("a stale manifest warns in flight too", () => {
	// pins current behaviour: every other note is hidden while an apply runs, this one is not.
	const s = status({ manifest: { ...manifest, stale: true }, job });
	expect(view(s).body?.stale).toBe(true);
	expect(view(status({ manifest })).body?.stale).toBe(false);
	expect(view(status({})).body?.stale).toBe(false);
});

test("an empty error string draws no failure line", () => {
	expect(view(status({ last_error: "" })).body?.lastError).toBeNull();
	expect(view(status({ last_error: null })).body?.lastError).toBeNull();
});

test("the check row: disabled note, button with last time, or nothing", () => {
	const check = (o: Partial<UpdateStatus>, a = false) =>
		view(status(o), a ? applying : null).body?.check;
	expect(check({})).toEqual({ kind: "button", lastChecked: null });
	expect(check({ last_checked_unix: 0 })).toEqual({
		kind: "button",
		lastChecked: 0,
	});
	expect(check({ last_checked_unix: null })).toEqual({
		kind: "button",
		lastChecked: null,
	});
	expect(check({}, true)).toEqual({ kind: "none" });
	expect(check({ check_disabled: true })).toEqual({ kind: "disabled" });
	// The disabled note outlasts an apply in flight.
	expect(check({ check_disabled: true }, true)).toEqual({ kind: "disabled" });
});

test("a failing poll is shown, except while the host restarts under an apply", () => {
	const err = new Error("502");
	expect(view(status({}), null, err)).toMatchObject({
		error: err,
		pollFailing: true,
	});
	expect(view(status({}), applying, err)).toMatchObject({
		error: undefined,
		pollFailing: true,
	});
	expect(view(status({ job }), null, err).error).toBeUndefined();
	expect(view(status({}), null, undefined).pollFailing).toBe(false);
	expect(view(status({}), null, null).pollFailing).toBe(false);
});

test("the timeout starts after eight minutes and only while an apply runs", () => {
	const at = (ms: number) => view(status({}), applying, undefined, ms).timedOut;
	expect(at(applying.startedAt + APPLY_TIMEOUT_MS)).toBe(false);
	expect(at(applying.startedAt + APPLY_TIMEOUT_MS + 1)).toBe(true);
	// A host job without a local apply (the page was reloaded) never times out.
	expect(view(status({ job }), null, undefined, 1e12).timedOut).toBe(false);
});
