import type { UpdateStatus } from "@/api/gen/model";

/** Give up waiting for the host to come back this long after apply started. */
export const APPLY_TIMEOUT_MS = 8 * 60 * 1000;

export type Applying = { target: string; startedAt: number } | null;

/** The "latest version" row. */
export type LatestVersion =
	| { kind: "published"; version: string; notesUrl: string }
	| { kind: "empty-channel" }
	| { kind: "never-checked" };

/** What sits under the two version rows. */
export type UpdatePanel =
	| { kind: "progress" }
	| { kind: "apply" }
	| { kind: "how-to"; optInHint: string | null }
	| { kind: "up-to-date" }
	| { kind: "none" };

/** The check button, or why there is none. */
export type CheckRow =
	| { kind: "disabled" }
	| { kind: "button"; lastChecked: number | null }
	| { kind: "none" };

interface UpdateBody {
	status: UpdateStatus;
	latest: LatestVersion;
	panel: UpdatePanel;
	lastResult: NonNullable<UpdateStatus["last_result"]> | null;
	/** The last verified manifest is old. */
	stale: boolean;
	/** The channel has no release yet; not a failed check. */
	emptyChannel: boolean;
	lastError: string | null;
	check: CheckRow;
}

interface UpdateCardView {
	/** An apply was accepted or the host reports a job. */
	inFlight: boolean;
	/** Waiting for the host to return has outlasted a restart. */
	timedOut: boolean;
	/** The status poll is failing, so the rendered snapshot is the last one received. */
	pollFailing: boolean;
	/** The poll error to show; none while an apply restarts the host. */
	error: unknown;
	showBadge: boolean;
	/** `null` until a status has arrived. */
	body: UpdateBody | null;
}

const latestVersion = (s: UpdateStatus): LatestVersion => {
	if (s.manifest) {
		return {
			kind: "published",
			version: s.manifest.version,
			notesUrl: s.manifest.notes_url,
		};
	}
	return { kind: s.not_published ? "empty-channel" : "never-checked" };
};

const updatePanel = (s: UpdateStatus, inFlight: boolean): UpdatePanel => {
	if (inFlight) return { kind: "progress" };
	if (!s.available) return { kind: s.manifest ? "up-to-date" : "none" };
	if (s.apply === "full" || s.apply === "staged") return { kind: "apply" };
	return { kind: "how-to", optInHint: s.opt_in_hint || null };
};

const checkRow = (s: UpdateStatus, inFlight: boolean): CheckRow => {
	if (s.check_disabled) return { kind: "disabled" };
	if (inFlight) return { kind: "none" };
	return { kind: "button", lastChecked: s.last_checked_unix ?? null };
};

const updateBody = (s: UpdateStatus, inFlight: boolean): UpdateBody => ({
	status: s,
	latest: latestVersion(s),
	panel: updatePanel(s, inFlight),
	lastResult: inFlight ? null : (s.last_result ?? null),
	stale: Boolean(s.manifest?.stale),
	emptyChannel: !inFlight && s.not_published,
	lastError: inFlight ? null : s.last_error || null,
	check: checkRow(s, inFlight),
});

/**
 * Everything the card decides from the last status snapshot, the apply in progress, the poll
 * error and the clock. A failed poll during an apply is expected: the snapshot stays on screen.
 */
export function updateCardView(
	s: UpdateStatus | undefined,
	applying: Applying,
	error: unknown,
	now: number,
): UpdateCardView {
	const inFlight = Boolean(applying) || Boolean(s?.job);
	return {
		inFlight,
		timedOut: applying !== null && now - applying.startedAt > APPLY_TIMEOUT_MS,
		pollFailing: Boolean(error),
		error: inFlight ? undefined : error,
		showBadge: Boolean(s?.available) && !inFlight,
		body: s ? updateBody(s, inFlight) : null,
	};
}
