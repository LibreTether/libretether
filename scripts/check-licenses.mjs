#!/usr/bin/env node
//
// License compliance gate. LibreTether ships under AGPL-3.0-only, so every
// dependency we redistribute must be license-compatible with that. This walks
// the whole Rust (cargo) + JS (pnpm) dependency tree and fails if any package's
// license isn't on the allowlist of AGPL-3.0-compatible SPDX identifiers.
//
// No external tooling required (no cargo-deny install): it reads `cargo
// metadata` and `pnpm licenses list --json`, both of which are already present.
//
// Tuning: add a compatible SPDX id to ALLOW, or — for a dep whose SPDX metadata
// is missing/odd but you've manually verified — add it to ALLOW_PACKAGES.

import { execSync } from "node:child_process"

// SPDX ids accepted as compatible with an AGPL-3.0 outbound license:
//   - permissive (MIT/BSD/ISC/Zlib/Apache-2.0/Boost/Unicode/public-domain),
//   - copyleft that is one-way compatible *into* (A)GPLv3 (MPL-2.0, LGPL, GPLv3), and
//   - font licenses compatible by aggregation (OFL-1.1): the bundled @fontsource
//     webfonts ship as separate woff2 assets (referenced from CSS), not merged into
//     the code, so the OFL coexists with our AGPL output. Verified via the SIL OFL
//     FAQ §1.2/1.3 and the FSF, which lists OFL-1.1 as a free, GPL-compatible license.
const ALLOW = new Set([
	"MIT",
	"MIT-0",
	"Apache-2.0",
	"0BSD",
	"BSD-1-Clause",
	"BSD-2-Clause",
	"BSD-3-Clause",
	"ISC",
	"Zlib",
	"BSL-1.0",
	"Unicode-3.0",
	"Unicode-DFS-2016",
	"Unlicense",
	"CC0-1.0",
	"WTFPL",
	"MPL-2.0",
	"LGPL-2.1-only",
	"LGPL-2.1-or-later",
	"LGPL-3.0-only",
	"LGPL-3.0-or-later",
	"GPL-3.0-only",
	"GPL-3.0-or-later",
	"AGPL-3.0-only",
	"AGPL-3.0-or-later",
	// SIL Open Font License — the bundled @fontsource webfonts (Geist, Geist Mono,
	// Space Grotesk). Compatible by aggregation; see the note above.
	"OFL-1.1",
	"(MIT OR Apache-2.0)",
	"IJG"
])

// Escape hatch for individual deps verified by hand. Use "name" or "name@version".
const ALLOW_PACKAGES = new Set([])

// Evaluate an SPDX expression: it passes when every AND-clause offers at least
// one allowed operand. Handles `/` as OR, parentheses, and `WITH <exception>`.
function isAllowed(expr) {
	if (!expr) {
		return false
	}

	const normalized = expr.replace(/\//g, " OR ").replace(/[()]/g, " ")

	return normalized.split(/\s+AND\s+/i).every((clause) =>
		clause
			.split(/\s+OR\s+/i)
			.map((id) =>
				id
					.trim()
					.replace(/\s+WITH\s+.*/i, "")
					.trim()
			)
			.filter(Boolean)
			.some((id) => ALLOW.has(id))
	)
}

const violations = []

// ---- Rust workspace (cargo) -------------------------------------------------
const meta = JSON.parse(
	execSync("cargo metadata --format-version 1", { encoding: "utf8", maxBuffer: 256 * 1024 * 1024 })
)
const ourCrates = new Set(meta.workspace_members)

for (const pkg of meta.packages) {
	if (ourCrates.has(pkg.id)) {
		continue
	}

	const id = `${pkg.name}@${pkg.version}`

	if (ALLOW_PACKAGES.has(pkg.name) || ALLOW_PACKAGES.has(id)) {
		continue
	}

	if (!pkg.license) {
		violations.push(["rust", id, pkg.license_file ? "(license file, no SPDX expression)" : "(no license)"])
	} else if (!isAllowed(pkg.license)) {
		violations.push(["rust", id, pkg.license])
	}
}

// ---- JS workspace (pnpm) ----------------------------------------------------
let pnpmData = {}

try {
	pnpmData = JSON.parse(execSync("pnpm licenses list --json", { encoding: "utf8", maxBuffer: 256 * 1024 * 1024 }))
} catch (err) {
	// Some pnpm versions exit non-zero while still emitting the JSON on stdout.
	if (err.stdout) {
		pnpmData = JSON.parse(err.stdout.toString())
	} else {
		throw err
	}
}

for (const [license, packages] of Object.entries(pnpmData)) {
	if (isAllowed(license)) {
		continue
	}

	for (const pkg of packages) {
		const name = pkg.name || pkg.packageName || "?"

		if (ALLOW_PACKAGES.has(name)) {
			continue
		}

		const versions = Array.isArray(pkg.versions) ? pkg.versions.join(",") : ""

		violations.push(["js", versions ? `${name}@${versions}` : name, license || "(unknown)"])
	}
}

// ---- Report -----------------------------------------------------------------
if (violations.length === 0) {
	const total = meta.packages.length - ourCrates.size

	console.log(
		`✓ license check passed — all dependencies are compatible with AGPL-3.0 (${total} crates + JS deps scanned)`
	)
	process.exit(0)
}

console.error(
	`✗ license check FAILED — ${violations.length} dependency license(s) not on the AGPL-compatible allowlist:\n`
)

for (const [ecosystem, id, license] of violations.sort((a, b) => a[2].localeCompare(b[2]))) {
	console.error(`  [${ecosystem}] ${id} — ${license}`)
}

console.error(
	"\nIf a flagged license is in fact AGPL-3.0-compatible, add its SPDX id to ALLOW" +
		"\n(or the specific package to ALLOW_PACKAGES) in scripts/check-licenses.mjs."
)
process.exit(1)
