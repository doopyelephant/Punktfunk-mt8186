// The console's password gate, as one table. A row's file under `routes/api/v1/` asks for the
// console password before it forwards; `seat` is that file's handler when the same call can
// arrive for a seat's host through `/api/v1/profiles/<id>/proxy/`, where the box holds the bearer.
// gates.test.ts pins the rows to the route files and to the paths a seat's host refuses.
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
	/** The gate a seat call goes through. Absent when the seat's host refuses the path. */
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

export const GATES: readonly Route[] = [
	gated("POST", "actions/{id}"),
	gated("PUT", "hooks", hooksPut),
	gated("POST", "library/custom", customPost),
	gated("PUT", "library/custom/{id}", customPut),
	gated("PUT", "library/provider/{provider}", providerPut),
	gated("POST", "native/pair/arm"),
	gated("POST", "native/pending/{id}/approve"),
	gated("POST", "pair/pin"),
	gated("PUT", "profiles/door"),
	gated("DELETE", "profiles/{id}"),
	gated("POST", "store/install", installPost),
	gated("PUT", "store/sources/{name}", sourcesPut),
	gated("POST", "update/apply"),
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
