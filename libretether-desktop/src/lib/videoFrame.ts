// Decoder for the binary session-frame format produced by the agent. Mirrors
// `libretether-protocol/src/video.rs` byte for byte — the Rust side strips the
// 1-byte message tag, so the ArrayBuffer delivered over the channel starts at
// the frame's `kind` byte. All integers are big-endian (DataView's default).

export interface ParsedTile {
	col: number
	row: number
	/** Raw baseline-JPEG bytes for this tile (a view into the frame buffer). */
	bytes: Uint8Array<ArrayBuffer>
}

export interface ParsedFrame {
	/** A keyframe carries every tile and (re)sizes the canvas; a delta only the changed ones. */
	key: boolean
	width: number
	height: number
	tileSize: number
	tiles: ParsedTile[]
}

export function parseFrame(buf: ArrayBuffer): ParsedFrame {
	const dv = new DataView(buf)
	let o = 0
	const kind = dv.getUint8(o)
	o += 1
	o += 8 // seq — unused by the renderer
	const width = dv.getUint32(o)
	o += 4
	const height = dv.getUint32(o)
	o += 4
	const tileSize = dv.getUint16(o)
	o += 2
	const count = dv.getUint32(o)
	o += 4
	const tiles: ParsedTile[] = []
	for (let i = 0; i < count; i++) {
		const col = dv.getUint16(o)
		o += 2
		const row = dv.getUint16(o)
		o += 2
		const len = dv.getUint32(o)
		o += 4
		tiles.push({ bytes: new Uint8Array(buf, o, len), col, row })
		o += len
	}
	return { height, key: kind === 0, tileSize, tiles, width }
}
