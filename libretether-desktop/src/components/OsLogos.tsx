// Colorful per-OS brand tiles used by OS_META (the launch screen, machine list,
// add-machine drawer, …). Each is a full-bleed 64×64 rounded tile with a white
// glyph, sized by the caller via `className` (e.g. "h-4 w-4"). Gradient ids are
// namespaced per icon so several tiles on one page never collide.
//
// Windows 11 and macOS Ventura marks are adapted from the quickvm asset set; the
// Linux (Tux) mark is bespoke.

type IconProps = { className?: string }

export function WindowsLogo({ className }: IconProps) {
	return (
		<svg aria-hidden="true" className={className} role="img" viewBox="0 0 64 64">
			<defs>
				<linearGradient id="lt-os-win" x1="0" x2="1" y1="0" y2="1">
					<stop offset="0" stopColor="#1f7ff0" />
					<stop offset="1" stopColor="#0a4aab" />
				</linearGradient>
			</defs>
			<rect fill="url(#lt-os-win)" height="64" rx="14" width="64" />
			<g fill="#fff">
				<rect height="13.5" rx="1.6" width="13.5" x="16" y="16" />
				<rect height="13.5" rx="1.6" width="13.5" x="34.5" y="16" />
				<rect height="13.5" rx="1.6" width="13.5" x="16" y="34.5" />
				<rect height="13.5" rx="1.6" width="13.5" x="34.5" y="34.5" />
			</g>
		</svg>
	)
}

export function MacosLogo({ className }: IconProps) {
	return (
		<svg
			aria-label="macOS Tahoe"
			className={className}
			role="img"
			viewBox="0 0 64 64"
			xmlns="http://www.w3.org/2000/svg"
		>
			<defs>
				<linearGradient id="g" x1="0" x2="1" y1="0" y2="1">
					<stop offset="0" stop-color="#ff8a3d" />
					<stop offset="0.55" stop-color="#ff5e7e" />
					<stop offset="1" stop-color="#b14bd8" />
				</linearGradient>
			</defs>
			<rect fill="url(#g)" height="64" width="64" />
			<g fill="#fff" fill-opacity="0.96" transform="translate(21.4 19) scale(0.026)">
				<path d="M788.1 340.9c-5.8 4.5-108.2 62.2-108.2 190.5 0 148.4 130.3 200.9 134.2 202.2-.6 3.2-20.7 71.9-68.7 141.9-42.8 61.6-87.5 123.1-155.5 123.1s-85.5-39.5-164-39.5c-76.5 0-103.7 40.8-165.9 40.8s-105.6-57-155.5-127C46.7 790.7 0 663 0 541.8c0-194.4 126.4-297.5 250.8-297.5 66.1 0 121.2 43.4 162.7 43.4 39.5 0 101.1-46 176.3-46 28.5 0 130.9 2.6 198.5 99.2zm-234-181.5c31.1-36.9 53.1-88.1 53.1-139.3 0-7.1-.6-14.3-1.9-20.1-50.6 1.9-110.8 33.7-147.1 75.8-28.5 32.4-55.1 83.6-55.1 135.5 0 7.8 1.3 15.6 1.9 18.1 3.2.6 8.4 1.3 13.6 1.3 45.4 0 102.5-30.4 135.5-71.2z" />
			</g>
		</svg>
	)
}

/** Bespoke Tux mark — a chunky white penguin (amber beak + feet) on a slate tile,
 *  distinct from the two blue Windows/macOS tiles. */
export function LinuxLogo({ className }: IconProps) {
	return (
		<svg aria-hidden="true" className={className} role="img" viewBox="0 0 64 64">
			<defs>
				<linearGradient id="lt-os-lin" x1="0" x2="1" y1="0" y2="1">
					<stop offset="0" stopColor="#c9424f" />
					<stop offset="1" stopColor="#f0262e" />
				</linearGradient>
			</defs>
			<rect fill="url(#lt-os-lin)" height="64" rx="14" width="64" />
			{/* feet (behind the body) */}
			<g fill="#f6ab3d">
				<ellipse cx="27" cy="50.5" rx="4.2" ry="2.3" />
				<ellipse cx="37" cy="50.5" rx="4.2" ry="2.3" />
			</g>
			{/* body + head */}
			<path
				d="M32 14.5C38.5 14.5 43 19.5 43 26C46.5 27.5 48.5 32 47 36.5C46 41 44 45 40 47.5C37.5 49 35 49.5 32 49.5C29 49.5 26.5 49 24 47.5C20 45 18 41 17 36.5C15.5 32 17.5 27.5 21 26C21 19.5 25.5 14.5 32 14.5Z"
				fill="#fff"
			/>
			{/* beak */}
			<path d="M28.5 26 L35.5 26 L32 30 Z" fill="#f6ab3d" />
			{/* eyes */}
			<g fill="#232b34">
				<circle cx="28.7" cy="22.6" r="1.7" />
				<circle cx="35.3" cy="22.6" r="1.7" />
			</g>
		</svg>
	)
}
