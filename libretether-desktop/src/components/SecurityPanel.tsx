import { Fingerprint, KeyRound, Lock, Route, ShieldCheck } from "lucide-react"
import { fingerprint } from "../lib/format"
import type { ActiveInfo, ClientDto, ControllerKind } from "../lib/types"
import { CopyRow } from "./CopyRow"
import { Badge } from "./ui"

/** How the agent's traffic reaches this controller, and what that path
 *  guarantees. Derived from the controller's connection kind — the same QUIC +
 *  TLS 1.3 transport and mutual Ed25519 auth applies in every mode; what differs
 *  is whether a third party (a relay) sits in the path. */
function pathSummary(kind: ControllerKind): { e2e: boolean; label: string; detail: string } {
	switch (kind.type) {
		case "direct":
			return {
				detail: "The agent dials this controller directly over QUIC. The link is encrypted end-to-end with TLS 1.3, and both ends prove their identity with pinned Ed25519 keys — nobody on the network path can read the traffic or impersonate either side.",
				e2e: true,
				label: "End-to-end encrypted"
			}
		case "tailscale":
			return {
				detail: "The agent reaches this controller across your Tailscale tailnet (WireGuard, encrypted device-to-device). LibreTether layers its own TLS 1.3 transport and mutual Ed25519 authentication on top, so the link stays end-to-end secured regardless of the tailnet.",
				e2e: true,
				label: "End-to-end encrypted"
			}
		case "relay":
			return {
				detail: "Traffic is forwarded by libretether-relay. Each leg (controller↔relay and relay↔agent) is TLS 1.3-encrypted, and the two ends are mutually authenticated with pinned Ed25519 keys — so the relay can't impersonate either side, and holding the relay secret alone can't drive the agent. The relay does forward the stream, though, so run one you trust.",
				e2e: false,
				label: "Encrypted & authenticated"
			}
	}
}

/** Compact badge for the machine row: a shield indicating the link is encrypted
 *  and mutually authenticated. Only meaningful once the machine has enrolled (it
 *  has a pinned identity); render nothing before then. The full story lives in
 *  the detail drawer's [`SecurityPanel`]. */
export function SecurityBadge({ client, kind }: { client: ClientDto; kind: ControllerKind }) {
	if (!client.enrolled) return null
	const { e2e, detail } = pathSummary(kind)
	return (
		<Badge className="gap-1" title={detail} tone="success">
			<ShieldCheck className="h-3 w-3" />
			{e2e ? "E2E encrypted" : "Encrypted"}
		</Badge>
	)
}

function FactGrid({ rows }: { rows: [string, string][] }) {
	return (
		<div className="grid grid-cols-2 gap-px overflow-hidden rounded-xl border border-border bg-border">
			{rows.map(([k, v]) => (
				<div className="bg-surface-2 px-3 py-2.5" key={k}>
					<div className="eyebrow">{k}</div>
					<div className="mt-1 truncate font-mono text-[0.82rem] text-text" title={v}>
						{v}
					</div>
				</div>
			))}
		</div>
	)
}

function IdentityRow({
	icon,
	label,
	caption,
	publicKey
}: {
	icon: React.ReactNode
	label: string
	caption: string
	publicKey: string
}) {
	return (
		<div className="flex flex-col gap-1.5">
			<div className="flex items-baseline justify-between gap-2">
				<span className="flex items-center gap-1.5 text-xs font-semibold text-muted">
					<span className="text-primary dark:text-primary-strong">{icon}</span>
					{label}
				</span>
				<span className="font-mono text-[0.7rem] text-subtle" title="fingerprint (first 12 chars of the key)">
					{fingerprint(publicKey)}…
				</span>
			</div>
			<CopyRow value={publicKey} />
			<span className="text-[0.72rem] leading-relaxed text-subtle">{caption}</span>
		</div>
	)
}

/** The encryption / identity read-out for a single machine. Shows how the link
 *  is secured (badges + a path explanation), and the two pinned identities that
 *  make the channel mutually authenticated: the agent's own Ed25519 key and the
 *  `controller_key` the agent pins. Shown whenever the machine has enrolled —
 *  the identities are known even while it's offline. */
export function SecurityPanel({ client, active }: { client: ClientDto; active: ActiveInfo }) {
	const summary = pathSummary(active.kind)

	if (!client.enrolled) {
		return (
			<section className="flex flex-col gap-2.5">
				<div className="eyebrow">Encryption &amp; identity</div>
				<p className="rounded-xl border border-border border-dashed bg-surface-2 px-3.5 py-3 text-sm text-subtle">
					Not enrolled yet. The agent's identity key is pinned the first time it connects with the one-time
					enrollment token; until then there's nothing to verify against.
				</p>
			</section>
		)
	}

	return (
		<section className="flex flex-col gap-3">
			<div className="eyebrow">Encryption &amp; identity</div>

			<div className="flex flex-wrap gap-1.5">
				<Badge className="gap-1" tone="success">
					<Lock className="h-3 w-3" />
					{summary.label}
				</Badge>
				<Badge className="gap-1" tone="success">
					<ShieldCheck className="h-3 w-3" />
					Mutually authenticated
				</Badge>
				<Badge className="gap-1" tone="neutral">
					<KeyRound className="h-3 w-3" />
					Identity pinned
				</Badge>
			</div>

			<div className="flex gap-2.5 rounded-xl border border-border bg-surface-2 px-3.5 py-3">
				<Route className="mt-0.5 h-4 w-4 shrink-0 text-primary dark:text-primary-strong" />
				<p className="text-[0.82rem] leading-relaxed text-muted">{summary.detail}</p>
			</div>

			<FactGrid
				rows={[
					["Transport", "QUIC · TLS 1.3"],
					["Identity", "Ed25519"],
					["Authentication", "Mutual challenge–response"],
					["Protocol", `v${active.protocol_version}`]
				]}
			/>

			{client.public_key && (
				<IdentityRow
					caption="The agent's stable public key. The controller checks the signature on every connection's challenge against it — no trust-on-first-use."
					icon={<Fingerprint className="h-3.5 w-3.5" />}
					label="This machine (agent key)"
					publicKey={client.public_key}
				/>
			)}

			<IdentityRow
				caption="The controller_key this agent pins at deploy time and verifies the controller's signature against, so a stranger (or a relay-secret holder) can't drive it."
				icon={<ShieldCheck className="h-3.5 w-3.5" />}
				label="Controller (pinned key)"
				publicKey={active.public_key}
			/>
		</section>
	)
}
