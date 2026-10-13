import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { Glob } from "bun";
import { GATES, routeFile, seatGate, UNDECIDED } from "./gates";

const routes = join(import.meta.dir, "../routes/api/v1");
const repo = join(import.meta.dir, "../../..");
const gated = GATES.filter((r) => !r.open);

describe("seatGate", () => {
	test("names the box's gate for each call it guards", () => {
		const gate = (method: string, rest: string) => {
			const found = seatGate(method, rest);
			const route = GATES.find((r) => r.seat === found?.handler);
			return (
				found && route && { route: routeFile(route), params: found.params }
			);
		};
		expect(gate("PUT", "hooks")).toEqual({ route: "hooks.put", params: {} });
		expect(gate("POST", "store/install")?.route).toBe("store/install.post");
		expect(gate("PUT", "store/sources/mine")).toEqual({
			route: "store/sources/[name].put",
			params: { name: "mine" },
		});
		expect(gate("POST", "library/custom")?.route).toBe("library/custom.post");
		expect(gate("put", "library/custom/7")).toEqual({
			route: "library/custom/[id].put",
			params: { id: "7" },
		});
		expect(gate("PUT", "library/provider/romm")).toEqual({
			route: "library/provider/[provider].put",
			params: { provider: "romm" },
		});
	});

	test("leaves the reads and the ungated writes to the relay", () => {
		expect(seatGate("GET", "hooks")).toBeNull();
		expect(seatGate("DELETE", "store/sources/mine")).toBeNull();
		expect(seatGate("DELETE", "library/custom/7")).toBeNull();
		expect(seatGate("POST", "library/page")).toBeNull();
		expect(seatGate("PUT", "library/custom/7/extra")).toBeNull();
		expect(seatGate("PUT", "store/sources/")).toBeNull();
	});
});

test("the rows are the route files that ask for the password", () => {
	const asking = [...new Glob("**/*.ts").scanSync(routes)]
		.filter((file) =>
			/from "(\.\.\/)+util\/(confirm|libraryConfirm)"/.test(
				readFileSync(join(routes, file), "utf8"),
			),
		)
		.map((file) => file.replace(/\.ts$/, ""));
	expect(gated.map(routeFile).sort()).toEqual(asking.sort());
});

test("a seat call goes through the handler of its own row's file", async () => {
	for (const route of GATES.filter((r) => r.seat)) {
		const file = await import(join(routes, `${routeFile(route)}.ts`));
		expect(route.seat, routeFile(route)).toBe(file.default);
	}
});

test("a row with no seat gate is a path the seat's host refuses", () => {
	const rust = readFileSync(
		join(repo, "crates/host/punktfunk-host/src/mgmt/profiles.rs"),
		"utf8",
	);
	const list = /fn proxy_refuses[^}]*matches!\(\s*first,([^)]*)\)/.exec(
		rust,
	)?.[1];
	expect(list, "proxy_refuses in mgmt/profiles.rs").toBeDefined();
	const refused = [...(list ?? "").matchAll(/"([^"]+)"/g)].map((m) => m[1]);
	const relayed = gated.filter(
		(r) => !r.seat && !refused.includes(r.pattern.split("/")[0]),
	);
	expect(relayed.map(routeFile)).toEqual([]);
});

test("every mutating operation in api/openapi.json has one row", () => {
	const spec: { paths: Record<string, Record<string, unknown>> } = JSON.parse(
		readFileSync(join(repo, "api/openapi.json"), "utf8"),
	);
	const operations = Object.entries(spec.paths).flatMap(([path, item]) =>
		["POST", "PUT", "PATCH", "DELETE"]
			.filter((method) => method.toLowerCase() in item)
			.map((method) => `${method} ${path.replace(/^\/api\/v1\//, "")}`),
	);
	const rows = GATES.map((r) => `${r.method} ${r.pattern}`);
	expect(rows.filter((row, i) => rows.indexOf(row) !== i)).toEqual([]);
	expect(operations.filter((op) => !rows.includes(op))).toEqual([]);
	expect(rows.filter((row) => !operations.includes(row))).toEqual([]);

	const undecided = GATES.filter((r) => r.open === UNDECIDED);
	console.warn(
		[
			`password gate: ${undecided.length} mutating routes are open with no decision on record`,
			...undecided.map((r) => `  ${r.method} ${r.pattern}`),
		].join("\n"),
	);
});
