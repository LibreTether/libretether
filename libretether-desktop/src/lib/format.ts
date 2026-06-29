/** Human-readable duration from seconds, e.g. "3d 4h", "12m". */
export function formatUptime(secs: number): string {
	if (secs <= 0) return "0s"
	const d = Math.floor(secs / 86400)
	const h = Math.floor((secs % 86400) / 3600)
	const m = Math.floor((secs % 3600) / 60)
	const s = secs % 60
	if (d > 0) return `${d}d ${h}h`
	if (h > 0) return `${h}h ${m}m`
	if (m > 0) return `${m}m ${s}s`
	return `${s}s`
}

/** Relative time from a unix-seconds timestamp, e.g. "3m ago", "just now". */
export function relativeTime(unixSecs: number | null): string {
	if (!unixSecs) return "never"
	const diff = Math.floor(Date.now() / 1000) - unixSecs
	if (diff < 5) return "just now"
	if (diff < 60) return `${diff}s ago`
	if (diff < 3600) return `${Math.floor(diff / 60)}m ago`
	if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`
	return `${Math.floor(diff / 86400)}d ago`
}

/** Split a command line into program + args, honoring single/double quotes so a
 *  value with spaces (e.g. `echo "a b"`) stays one token. Args are passed to the
 *  agent as an argv array (no shell), so this only needs to handle quoting/spaces. */
export function tokenizeCommand(input: string): string[] {
	const tokens: string[] = []
	let cur = ""
	let quote: '"' | "'" | null = null
	let started = false
	for (const ch of input) {
		if (quote) {
			if (ch === quote) quote = null
			else cur += ch
		} else if (ch === '"' || ch === "'") {
			quote = ch
			started = true
		} else if (/\s/.test(ch)) {
			if (started) {
				tokens.push(cur)
				cur = ""
				started = false
			}
		} else {
			cur += ch
			started = true
		}
	}
	if (started) tokens.push(cur)
	return tokens
}

/** A filesystem-safe slug for a client name. */
export function slug(name: string): string {
	return (
		name
			.toLowerCase()
			.replace(/[^a-z0-9]+/g, "-")
			.replace(/^-+|-+$/g, "") || "client"
	)
}
