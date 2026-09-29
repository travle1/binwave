// Worker-owned file state. Only bounded slices enter memory; positions are BigInt.
// Edits are pieces over the original (see byte-pieces.mjs), so inserts, deletes,
// patterns and inserted files never copy the file.
import * as edits from './byte-pieces.mjs';
export const READ_LIMIT = 1024 * 1024;
export const PAGE_SIZE = 64 * 1024;
// Typed bytes held in memory, and pieces, across all edits.
export const MAX_EDIT_BYTES = 64 * 1024 * 1024;
export const MAX_PIECES = 65536;
export const MAX_HISTORY = 4096;
const MAX_OFFSET = BigInt(Number.MAX_SAFE_INTEGER);

export function offset(value) {
    if (typeof value !== 'string' || !/^\d+$/.test(value)) throw Error('Invalid byte offset');
    const n = BigInt(value);
    // Blob.slice accepts Number. Refuse rounded offsets rather than truncating.
    if (n > MAX_OFFSET) throw Error('File offset exceeds the browser exact-integer limit (2^53 - 1)');
    return n;
}
export function range(start, count, size) {
    const at = offset(start);
    if (!Number.isSafeInteger(count) || count < 0 || count > READ_LIMIT) throw Error('Read exceeds 1 MiB limit');
    if (at > size) throw Error('Byte offset is outside the file');
    return [at, Number((size - at) < BigInt(count) ? size - at : BigInt(count))];
}

export class FileBackend {
    constructor(file, handle = null) {
        if (!Number.isSafeInteger(file.size) || file.size < 0) throw Error('File size is not exactly representable');
        this.file = file;
        this.handle = handle;
        this.originalSize = BigInt(file.size);
        this.pieces = edits.original(this.originalSize);
        this.size = this.originalSize;
        this.undo = [];
        this.redo = [];
        this.dirty = false;
    }
    async validate() {
        if (!this.handle) return;
        const current = await this.handle.getFile();
        if (current.size !== this.file.size || current.lastModified !== this.file.lastModified) {
            throw Error('Source file changed; reopen it');
        }
    }
    async read(start, count) {
        const [at, length] = range(start, count, this.size);
        const bytes = new Uint8Array(length);
        let written = 0;
        for (const {piece, from, to} of edits.segments(this.pieces, at, length)) {
            let part;
            if (piece.kind === 'added') part = piece.bytes.subarray(Number(from), Number(to));
            else if (piece.kind === 'pattern') part = edits.patternBytes(piece, from, to);
            else {
                const file = piece.kind === 'base' ? this.file : piece.file;
                const first = Number(piece.start + from), last = Number(piece.start + to);
                part = new Uint8Array(await file.slice(first, last).arrayBuffer());
                if (part.length !== last - first) {
                    throw Error(piece.kind === 'base' ? 'Source ended during read; reopen it' : 'An inserted file changed; insert it again');
                }
            }
            bytes.set(part, written);
            written += part.length;
        }
        return bytes;
    }
    // Splice without recording history; refuses edits over the limits unchanged.
    apply(at, remove, insert) {
        const {pieces, removed} = edits.splice(this.pieces, at, remove, insert);
        if (edits.memory(pieces) > MAX_EDIT_BYTES || pieces.length > MAX_PIECES) {
            throw Error('Browser edit limit reached (64 MiB of typed bytes). Save, then keep editing.');
        }
        this.pieces = pieces;
        this.size = edits.sizeOf(pieces);
        return removed;
    }
    async record(at, remove, insert) {
        await this.validate();
        const removed = this.apply(at, remove, insert);
        this.undo.push({at, removed, inserted: insert});
        if (this.undo.length > MAX_HISTORY) this.undo.shift();
        this.redo = [];
        this.dirty = true;
    }
    async replace(start, value) {
        return this.replaceRange(start, [value]);
    }
    // One multi-byte field edit is one undo step. Validate before changing anything.
    async replaceRange(start, values) {
        const at = offset(start);
        if (!validBytes(values) || values.length < 1 || values.length > PAGE_SIZE || at + BigInt(values.length) > this.size) {
            throw Error('Invalid byte replacement');
        }
        const before = await this.read(start, values.length);
        if (Array.from(values).every((value, i) => value === before[i])) return;
        await this.record(at, BigInt(values.length), [edits.added(values)]);
    }
    async insert(start, values) {
        const at = offset(start);
        if (!validBytes(values) || values.length < 1 || values.length > READ_LIMIT || at > this.size) {
            throw Error('Invalid byte insert');
        }
        await this.record(at, 0n, [edits.added(values)]);
    }
    // `pattern` repeated `count` times, never expanded in memory.
    async insertPattern(start, pattern, count) {
        const at = offset(start);
        if (!validBytes(pattern) || pattern.length < 1 || pattern.length > READ_LIMIT || at > this.size
            || !Number.isSafeInteger(count) || count < 1) throw Error('Invalid pattern insert');
        const len = BigInt(pattern.length) * BigInt(count);
        await this.record(at, 0n, [{kind: 'pattern', pattern: Uint8Array.from(pattern), phase: 0, len}]);
    }
    async fill(start, end, pattern) {
        const at = offset(start), last = offset(end);
        if (!validBytes(pattern) || pattern.length < 1 || pattern.length > READ_LIMIT || at >= last || last > this.size) {
            throw Error('Invalid fill');
        }
        await this.record(at, last - at, [{kind: 'pattern', pattern: Uint8Array.from(pattern), phase: 0, len: last - at}]);
    }
    // Another file's bytes; they stay in that file until the edited file is saved.
    async insertFile(start, file) {
        const at = offset(start);
        if (!file || !Number.isSafeInteger(file.size) || file.size < 1 || at > this.size) throw Error('Choose a file that is not empty');
        await this.record(at, 0n, [{kind: 'file', file, start: 0n, len: BigInt(file.size)}]);
    }
    async remove(start, end) {
        const at = offset(start), last = offset(end);
        if (at >= last || last > this.size) throw Error('Invalid delete range');
        await this.record(at, last - at, []);
    }
    history(redo = false) {
        const from = redo ? this.redo : this.undo;
        const to = redo ? this.undo : this.redo;
        const entry = from.at(-1);
        if (!entry) return;
        if (redo) this.apply(entry.at, edits.sizeOf(entry.removed), entry.inserted);
        else this.apply(entry.at, edits.sizeOf(entry.inserted), entry.removed);
        to.push(from.pop());
        this.dirty = true;
    }
    async stream(writer, first, last, cancelled, progress) {
        for (let at = first; at < last;) {
            if (cancelled()) throw Error('Export cancelled');
            const length = Number((last - at) < BigInt(READ_LIMIT) ? last - at : BigInt(READ_LIMIT));
            const bytes = await this.read(at.toString(), length);
            await writer.write(bytes); // Backpressure: never queue the whole file.
            at += BigInt(bytes.length);
            progress((at - first).toString(), (last - first).toString());
        }
    }
    async export(handle, start, end, cancelled = () => false, progress = () => {}) {
        const first = offset(start), last = offset(end);
        if (first > last || last > this.size) throw Error('Export range is outside the file');
        if (!this.handle) throw Error('Safe streaming export requires opening with the file-system picker');
        if (await this.handle.isSameEntry(handle)) throw Error('Choose a different destination: cannot replace the open source');
        await this.validate();
        if (cancelled()) throw Error('Export cancelled');
        const writer = await handle.createWritable();
        try {
            await this.stream(writer, first, last, cancelled, progress);
            await this.validate();
            if (cancelled()) throw Error('Export cancelled');
            await writer.close(); // Commit only after successful streaming/validation.
            if (first === 0n && last === this.size) this.dirty = false;
        } catch (error) {
            try { await writer.abort(); } catch { /* Preserve the original failure. */ }
            throw error;
        }
    }
    // Rewrite the open file. The browser writes a complete temporary copy and
    // swaps it in only when the writer closes, so a failure leaves it intact.
    async replaceSource(cancelled = () => false, progress = () => {}) {
        if (!this.handle) throw Error('Replacing the file requires opening it with the file-system picker');
        await this.validate();
        const writer = await this.handle.createWritable({keepExistingData: false});
        try {
            await this.stream(writer, 0n, this.size, cancelled, progress);
            await this.validate();
            if (cancelled()) throw Error('Save cancelled');
            await writer.close();
        } catch (error) {
            try { await writer.abort(); } catch { /* Preserve the original failure. */ }
            throw error;
        }
    }
}

function validBytes(values) {
    return (Array.isArray(values) || values instanceof Uint8Array)
        && Array.from(values).every(value => Number.isInteger(value) && value >= 0 && value <= 255);
}
