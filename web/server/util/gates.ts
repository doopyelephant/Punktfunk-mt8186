// The console's password gate, as one table: every mutating call on the management API, and
// whether a session alone may make it. A gated row's file under `routes/api/v1/` asks for the
// console password before it forwards; `seat` is that file's handler when the same call can
// arrive for a seat's host through `/api/v1/profiles/<id>/proxy/`, where the box holds the bearer.
// gates.test.ts fails on an operation in api/openapi.json with no row, and pins the gated rows to
// the route files and to the paths a seat's host refuses.
import type { EventHandler } from "h3";
import hooksPut from "../routes/api/v1/hooks.put";
import customPut from "../routes/api/v1/library/custom/[id].put";
import customPost from "../routes/api/v1/library/custom.post";
import providerPut from "../routes/api/v1/library/provider/[provider].put";
import installPost from "../routes/api/v1/store/install.post";
import sourcesPut from "../routes/api/v1/store/sources/[name].put";

type Method = "POST" | "PUT" | "PATCH" | "DELETE";

export interface Route {
	method: Method;
	/** The path after `/api/v1/`, spelled as `api/openapi.json` spells it. */
	pattern: string;
	/** Why a session alone may make the call. Absent on a gated row. */
	open?: string;
	/** A gated row's handler for a seat call. Absent when the seat's host refuses the path. */
	seat?: EventHandler;
}

const gated = (
	method: Method,
	pattern: string,
	seat?: EventHandler,
): Route => ({
	method,
	pattern,
	seat,
});

const open = (method: Method, pattern: string, reason: string): Route => ({
	method,
	pattern,
	open: reason,
});

/** Open today with no decision on record. The test prints these rows; none is gated meanwhile. */
export const UNDECIDED = "undecided";
const NARROWS = "only narrows what the host trusts or runs";
const STEERS = "steers a session, seat or display that already exists";
const WITHIN =
	"moves a live session within its pairing's grants, never past them";
const SETTING = "a setting or a label: no command, device or identity in it";
const REPORT = "a plugin's or a device's own report: grants nothing";
const RECORD = "diagnostics: re-reads, records or drops a record";
const REFETCH = "stops or removes a download: saves stay";
const REFRESH = "re-reads a source the host already trusts";
const PUBLIC = "open on the host too: a device proving its pairing";

export const GATES: readonly Route[] = [
	gated("POST", "actions/{id}"),
	open("POST", "auth/device/challenge", PUBLIC),
	open("POST", "auth/device/token", PUBLIC),
	open("POST", "client-logs", REPORT),
	open("DELETE", "client-logs/{id}", RECORD),
	open("DELETE", "clients", NARROWS),
	open("PATCH", "clients/{fingerprint}", SETTING),
	open("DELETE", "clients/{fingerprint}", NARROWS),
	open("POST", "diagnostics/refresh", RECORD),
	open("PUT", "display/clients/{fingerprint}", SETTING),
	open("DELETE", "display/clients/{fingerprint}", SETTING),
	open("PUT", "display/layout", SETTING),
	open("POST", "display/presets", SETTING),
	open("PUT", "display/presets/{id}", SETTING),
	open("DELETE", "display/presets/{id}", SETTING),
	open("POST", "display/release", STEERS),
	open("PUT", "display/settings", SETTING),
	open("POST", "emulators/{id}/adopt", UNDECIDED),
	open("POST", "emulators/{id}/content", UNDECIDED),
	open("POST", "emulators/{id}/install", UNDECIDED),
	open("POST", "emulators/{id}/prepare", UNDECIDED),
	open("POST", "emulators/{id}/remove", UNDECIDED),
	open("POST", "emulators/{id}/saves/export", UNDECIDED),
	open("POST", "emulators/{id}/saves/import", UNDECIDED),
	open("POST", "game/end", STEERS),
	open("PUT", "gpus/preference", SETTING),
	gated("PUT", "hooks", hooksPut),
	open("PATCH", "host/settings", UNDECIDED),
	gated("POST", "library/custom", customPost),
	gated("PUT", "library/custom/{id}", customPut),
	open("DELETE", "library/custom/{id}", NARROWS),
	open("PUT", "library/hidden/{id}", SETTING),
	open("POST", "library/install/{id}", UNDECIDED),
	open("DELETE", "library/install/{id}", REFETCH),
	open("POST", "library/install/{id}/cancel", REFETCH),
	open("POST", "library/install/{id}/pause", REFETCH),
	open("PUT", "library/metadata", SETTING),
	open("PUT", "library/metadata/{source}", REPORT),
	open("DELETE", "library/metadata/{source}", NARROWS),
	open("PUT", "library/picks/{id}", SETTING),
	gated("PUT", "library/provider/{provider}", providerPut),
	open("DELETE", "library/provider/{provider}", NARROWS),
	open("PUT", "library/provider/{provider}/downloads", REPORT),
	open("PUT", "library/provider/{provider}/running", REPORT),
	open("PUT", "library/scanners/{id}", SETTING),
	open("DELETE", "native/clients", NARROWS),
	open("PATCH", "native/clients/{fingerprint}", UNDECIDED),
	open("DELETE", "native/clients/{fingerprint}", NARROWS),
	open("DELETE", "native/pair", NARROWS),
	gated("POST", "native/pair/arm"),
	gated("POST", "native/pending/{id}/approve"),
	open("POST", "native/pending/{id}/deny", NARROWS),
	gated("POST", "pair/pin"),
	open("POST", "plugin-access/requests", REPORT),
	open("POST", "plugin-access/{plugin}/decide", UNDECIDED),
	open("POST", "plugin-access/{plugin}/release", NARROWS),
	open("POST", "plugins/logs", REPORT),
	open("PUT", "plugins/{id}", UNDECIDED),
	open("DELETE", "plugins/{id}", NARROWS),
	open("POST", "profiles", UNDECIDED),
	open("PUT", "profiles/default", SETTING),
	gated("PUT", "profiles/door"),
	open("PUT", "profiles/seating", UNDECIDED),
	open("PUT", "profiles/{id}", SETTING),
	gated("DELETE", "profiles/{id}"),
	open("PUT", "profiles/{id}/avatar", SETTING),
	open("DELETE", "profiles/{id}/avatar", SETTING),
	open("POST", "profiles/{id}/end", STEERS),
	open("POST", "profiles/{id}/start", STEERS),
	open("POST", "profiles/{id}/stop", STEERS),
	open("POST", "profiles/{id}/wake", STEERS),
	open("DELETE", "session", STEERS),
	open("POST", "session/idr", STEERS),
	open("PUT", "session/settings", SETTING),
	open("DELETE", "session/{id}", STEERS),
	open("PUT", "session/{id}/access", WITHIN),
	open("PUT", "session/{id}/audio", STEERS),
	open("POST", "session/{id}/idr", STEERS),
	open("PUT", "session/{id}/player", STEERS),
	open("POST", "stats/capture/start", RECORD),
	open("POST", "stats/capture/stop", RECORD),
	open("DELETE", "stats/recordings/{id}", RECORD),
	gated("POST", "store/install", installPost),
	open("POST", "store/refresh", REFRESH),
	open("POST", "store/runtime", UNDECIDED),
	gated("PUT", "store/sources/{name}", sourcesPut),
	open("DELETE", "store/sources/{name}", NARROWS),
	open("POST", "store/uninstall", NARROWS),
	gated("POST", "update/apply"),
	open("POST", "update/check", REFRESH),
];

/** The file under `routes/api/v1/` that serves a row, without its `.ts`. */
export function routeFile(route: Route): string {
	const path = route.pattern.replace(/\{(\w+)\}/g, "[$1]");
	return `${path}.${route.method.toLowerCase()}`;
}

/** The route params `rest` gives `pattern`, or `null` when it is another path. */
function match(pattern: string, rest: string): Record<string, string> | null {
	const want = pattern.split("/");
	const got = rest.split("/");
	if (want.length !== got.length) return null;
	const params: Record<string, string> = {};
	for (const [i, segment] of want.entries()) {
		const value = got[i] ?? "";
		const name = /^\{(\w+)\}$/.exec(segment)?.[1];
		if (name && value) params[name] = value;
		else if (segment !== value) return null;
	}
	return params;
}

export interface SeatGate {
	handler: EventHandler;
	/** The route params the handler reads. */
	params: Record<string, string>;
}

/** The box's own password gate that guards `method` `rest` on a seat's host, if any. */
export function seatGate(method: string, rest: string): SeatGate | null {
	const verb = method.toUpperCase();
	for (const route of GATES) {
		if (!route.seat || route.method !== verb) continue;
		const params = match(route.pattern, rest);
		if (params) return { handler: route.seat, params };
	}
	return null;
}
