import {range} from './file-backend.mjs';
import * as edits from './byte-pieces.mjs';
import './cache-settings.js';

// FileReaderSync is restricted to this dedicated worker. No source file is copied
// into WASM. Retain bounded read tiles across plot queries so returning to a
// viewport does not reread every sensor's bytes. This is session RAM only.
export const DECODER_PAGE_SIZE = 1024 * 1024;
export const DECODER_CACHE_BYTES = globalThis.binwaveCacheSettings.DEFAULT_BYTES;
export class DecoderSource {
    // `pieces` is the edited file from the file worker; null reads the file as is.
    constructor(file, pieces, readSlice, cacheBytes = DECODER_CACHE_BYTES) {
        this.file = file;
        this.pieces = pieces ?? edits.original(BigInt(file.size));
        this.size = edits.sizeOf(this.pieces);
        this.readSlice = readSlice;
        this.cache = new Map();
        this.cachedBytes = 0;
        this.setCacheBudget(cacheBytes);
    }
    setCacheBudget(bytes) {
        if (!Number.isSafeInteger(bytes) || bytes < 0) throw Error('Invalid file cache budget');
        this.cacheBytes = bytes;
        this.evictTo(bytes);
    }
    evictTo(bytes) {
        while (this.cachedBytes > bytes && this.cache.size) {
            const oldest = this.cache.keys().next().value;
            this.cachedBytes -= this.cache.get(oldest).byteLength;
            this.cache.delete(oldest);
        }
    }
    exact(at, length) {
        const bytes = new Uint8Array(length);
        let written = 0;
        for (const {piece, from, to} of edits.segments(this.pieces, at, length)) {
            let part;
            if (piece.kind === 'added') part = piece.bytes.subarray(Number(from), Number(to));
            else if (piece.kind === 'pattern') part = edits.patternBytes(piece, from, to);
            else {
                const file = piece.kind === 'base' ? this.file : piece.file;
                const first = Number(piece.start + from), last = Number(piece.start + to);
                part = new Uint8Array(this.readSlice(file.slice(first, last)));
                if (part.length !== last - first) throw Error('Source changed or ended during decoding');
            }
            bytes.set(part, written);
            written += part.length;
        }
        if (written !== length) throw Error('Source changed or ended during decoding');
        return bytes;
    }
    read(start, length) {
        const [at, count] = range(start, length, this.size);
        if (this.cacheBytes < DECODER_PAGE_SIZE || count > DECODER_PAGE_SIZE) return this.exact(at, count);
        const aligned = at / BigInt(DECODER_PAGE_SIZE) * BigInt(DECODER_PAGE_SIZE);
        const relative = Number(at - aligned);
        if (relative + count > DECODER_PAGE_SIZE) return this.exact(at, count);
        let page = this.cache.get(aligned);
        if (page) this.cache.delete(aligned); // Touch the LRU entry.
        if (!page) {
            page = this.exact(aligned, Number((this.size-aligned) < BigInt(DECODER_PAGE_SIZE) ? this.size-aligned : BigInt(DECODER_PAGE_SIZE)));
            this.evictTo(this.cacheBytes - page.byteLength);
            this.cachedBytes += page.byteLength;
        }
        this.cache.set(aligned, page);
        return page.subarray(relative, relative + count);
    }
}
