// `/api/v1/profiles/<id>/proxy/<rest>` reaches a seat's own host through the box. The box forwards
// it with the admin bearer, so the routes that ask for the console password on the box's own host
// ask on the seat's too: `seatGate` in gates.ts names the one that guards `rest`.
import { normalizePath } from "./paths";

/** A profile id as the host accepts one: ASCII letters and digits, at most 64. */
const ID = /^[A-Za-z0-9]{1,64}$/;

export interface SeatCall {
	id: string;
	/** The path on the seat's host after `/api/v1/`, dots and escapes resolved. */
	rest: string;
}

/**
 * The seat call `pathname` makes once its dots and escapes are resolved, or `null` when it points
 * anywhere else. The gate and the forward both read the resolved path, so they cannot disagree
 * about it.
 */
export function seatCall(pathname: string): SeatCall | null {
	const m = /^\/api\/v1\/profiles\/([^/]+)\/proxy\/(.+)$/.exec(
		normalizePath(pathname),
	);
	if (!m?.[1] || !m[2] || !ID.test(m[1])) return null;
	return { id: m[1], rest: m[2] };
}

/** `rest`, each segment escaped, as the tail of a seat path. */
export function encodeRest(rest: string): string {
	return rest.split("/").map(encodeURIComponent).join("/");
}

/** The box's own `/api/v1/profiles/<id>/proxy/<tail>`; `tail` is already escaped. */
export function seatPath(id: string, tail: string): string {
	return `/api/v1/profiles/${id}/proxy/${tail}`;
}

/** A plugin UI's credential never reaches a browser, from a seat's host either. */
export function isUiCredential(rest: string): boolean {
	return /^plugins\/[^/]+\/ui-credential$/i.test(rest);
}
