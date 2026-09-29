// The edited file as pieces over the original, so inserts and deletes shift
// later offsets without copying it. Pieces are plain objects so they can be
// posted to the decoder worker. Positions and lengths are BigInt.
//   {kind:'base', start, len}                  original bytes
//   {kind:'added', bytes, len}                 typed or pasted bytes (Uint8Array)
//   {kind:'pattern', pattern, phase, len}      `pattern` repeated from `phase`
//   {kind:'file', file, start, len}            part of another File
export const MERGE_LIMIT = 64 * 1024;

export function original(size) {
    return size > 0n ? [{kind: 'base', start: 0n, len: size}] : [];
}
export function added(bytes) {
    return {kind: 'added', bytes: Uint8Array.from(bytes), len: BigInt(bytes.length)};
}
export function sizeOf(pieces) {
    return pieces.reduce((sum, piece) => sum + piece.len, 0n);
}
export function slice(piece, from, to) {
    const len = to - from;
    switch (piece.kind) {
        case 'base': return {kind: 'base', start: piece.start + from, len};
        case 'added': return {kind: 'added', bytes: piece.bytes.slice(Number(from), Number(to)), len};
        case 'pattern': return {kind: 'pattern', pattern: piece.pattern,
            phase: (piece.phase + Number(from % BigInt(piece.pattern.length))) % piece.pattern.length, len};
        case 'file': return {kind: 'file', file: piece.file, start: piece.start + from, len};
    }
    throw Error(`Unknown piece ${piece.kind}`);
}
function merged(list) {
    const out = [];
    for (const piece of list) {
        if (piece.len === 0n) continue;
        const last = out.at(-1);
        if (last?.kind === 'base' && piece.kind === 'base' && last.start + last.len === piece.start) {
            out[out.length - 1] = {...last, len: last.len + piece.len};
        } else if (last?.kind === 'added' && piece.kind === 'added'
            && last.bytes.length + piece.bytes.length <= MERGE_LIMIT) {
            const bytes = new Uint8Array(last.bytes.length + piece.bytes.length);
            bytes.set(last.bytes); bytes.set(piece.bytes, last.bytes.length);
            out[out.length - 1] = {kind: 'added', bytes, len: BigInt(bytes.length)};
        } else out.push(piece);
    }
    return out;
}
// Replace `remove` bytes at `at` with `insert`; returns the new list and the
// removed pieces so the edit can be undone.
export function splice(pieces, at, remove, insert) {
    const end = at + remove;
    const kept = [], removed = [], tail = [];
    let start = 0n;
    for (const piece of pieces) {
        const stop = start + piece.len;
        if (stop <= at) kept.push(piece);
        else if (start >= end) tail.push(piece);
        else {
            if (start < at) kept.push(slice(piece, 0n, at - start));
            const from = (at > start ? at : start) - start, to = (end < stop ? end : stop) - start;
            if (from < to) removed.push(slice(piece, from, to));
            if (stop > end) tail.push(slice(piece, (end > start ? end : start) - start, piece.len));
        }
        start = stop;
    }
    return {pieces: merged([...kept, ...insert, ...tail]), removed};
}
// Read `length` bytes at `at`. `base(start, len)` and `file(file, start, len)`
// return Uint8Arrays (or Promises of them when `async` readers are used).
export function segments(pieces, at, length) {
    const end = at + BigInt(length), out = [];
    let start = 0n;
    for (const piece of pieces) {
        const stop = start + piece.len;
        if (start >= end) break;
        if (stop > at) {
            const from = (at > start ? at : start) - start, to = (end < stop ? end : stop) - start;
            out.push({piece, from, to});
        }
        start = stop;
    }
    return out;
}
export function patternBytes(piece, from, to) {
    const bytes = new Uint8Array(Number(to - from)), n = piece.pattern.length;
    let index = (piece.phase + Number(from % BigInt(n))) % n;
    for (let i = 0; i < bytes.length; i++) { bytes[i] = piece.pattern[index]; index = (index + 1) % n; }
    return bytes;
}
// Bytes the edits hold in memory: typed bytes and pattern sources.
export function memory(pieces) {
    return pieces.reduce((sum, piece) => sum + (piece.kind === 'added' ? piece.bytes.length
        : piece.kind === 'pattern' ? piece.pattern.length : 0), 0);
}
