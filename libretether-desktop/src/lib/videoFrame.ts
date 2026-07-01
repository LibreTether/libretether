// Decoder for the binary session-frame format produced by the agent. Mirrors
// `libretether-protocol/src/video.rs` byte for byte — the Rust side strips the
// 1-byte message tag, so the ArrayBuffer delivered over the channel starts at
// the frame's `kind` byte. All integers are big-endian (DataView's default).

export interface ParsedFrame {
	/** A keyframe (IDR) is self-contained; a delta (P-frame) builds on prior frames. */
	key: boolean
	/** Coded width (after downscale, always even). */
	width: number
	/** Coded height (after downscale, always even). */
	height: number
	/** The H.264 Annex-B access unit, fed straight to a WebCodecs `VideoDecoder`. */
	data: Uint8Array<ArrayBuffer>
}

// Header layout: kind(1) seq(8) width(4) height(4) len(4) = 21 bytes, then `len`
// bytes of access unit.
const HEADER_LEN = 21

export function parseFrame(buf: ArrayBuffer): ParsedFrame {
	const dv = new DataView(buf)
	const kind = dv.getUint8(0)
	const width = dv.getUint32(9)
	const height = dv.getUint32(13)
	const len = dv.getUint32(17)
	return { data: new Uint8Array(buf, HEADER_LEN, len), height, key: kind === 0, width }
}

/** Derive a WebCodecs `avc1.PPCCLL` codec string from a keyframe's in-band SPS
 *  (NAL type 7): the three bytes after the NAL header are profile_idc,
 *  constraint flags, and level_idc. Returns null if no SPS is found. */
export function avcCodecFromKeyframe(data: Uint8Array): string | null {
	for (let i = 0; i + 6 < data.length; i++) {
		// Annex-B start code (00 00 01 covers both 3- and 4-byte prefixes).
		if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
			if ((data[i + 3] & 0x1f) === 7) {
				const hex = (n: number) => n.toString(16).padStart(2, "0")
				return `avc1.${hex(data[i + 4])}${hex(data[i + 5])}${hex(data[i + 6])}`
			}
		}
	}
	return null
}
