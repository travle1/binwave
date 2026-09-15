// Worker-owned file state. Only bounded slices enter memory; positions are BigInt.
export const READ_LIMIT = 1024 * 1024;
export const PAGE_SIZE = 64 * 1024;
export const MAX_EDITS = 65536;
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
        this.size = BigInt(file.size);
        this.patches = new Map();
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
        const bytes = new Uint8Array(await this.file.slice(Number(at), Number(at) + length).arrayBuffer());
        if (bytes.length !== length) throw Error('Source ended during read; reopen it');
        for (const [position, value] of this.patches) {
            if (position >= at && position < at + BigInt(length)) bytes[Number(position - at)] = value;
        }
        return bytes;
    }
    async replace(start, value) {
        const at = offset(start);
        if (at >= this.size || !Number.isInteger(value) || value < 0 || value > 255) throw Error('Invalid byte replacement');
        if (!this.patches.has(at) && this.patches.size >= MAX_EDITS) throw Error('Browser edit limit reached (65,536 bytes)');
        await this.validate();
        const before = (await this.read(start, 1))[0];
        if (before === value) return;
        this.undo.push({at, before, after: value});
        if (this.undo.length > MAX_HISTORY) this.undo.shift();
        this.redo = [];
        this.patches.set(at, value);
        this.dirty = true;
    }
    history(redo = false) {
        const from = redo ? this.redo : this.undo;
        const to = redo ? this.undo : this.redo;
        const edit = from.pop();
        if (!edit) return;
        this.patches.set(edit.at, redo ? edit.after : edit.before);
        to.push(edit);
        this.dirty = true;
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
            for (let at = first; at < last;) {
                if (cancelled()) throw Error('Export cancelled');
                const length = Number((last - at) < BigInt(READ_LIMIT) ? last - at : BigInt(READ_LIMIT));
                const bytes = await this.read(at.toString(), length);
                await writer.write(bytes); // Backpressure: never queue the whole file.
                at += BigInt(bytes.length);
                progress((at - first).toString(), (last - first).toString());
            }
            await this.validate();
            if (cancelled()) throw Error('Export cancelled');
            await writer.close(); // Commit only after successful streaming/validation.
            if (first === 0n && last === this.size) this.dirty = false;
        } catch (error) {
            try { await writer.abort(); } catch { /* Preserve the original failure. */ }
            throw error;
        }
    }
}
