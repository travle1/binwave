import {range, PAGE_SIZE} from './file-backend.mjs';

// FileReaderSync is restricted to this dedicated worker. No source file is copied
// into WASM; a four-page cache amortizes scalar reads made by the shared engine.
export class DecoderSource {
    constructor(file, patches, readSlice) {
        this.file = file;
        this.size = BigInt(file.size);
        this.patches = new Map(patches);
        this.readSlice = readSlice;
        this.cache = new Map();
    }
    exact(at, length) {
        const bytes = new Uint8Array(this.readSlice(this.file.slice(Number(at), Number(at) + length)));
        if (bytes.length !== length) throw Error('Source changed or ended during decoding');
        for (const [position, value] of this.patches) {
            if (position >= at && position < at + BigInt(length)) bytes[Number(position - at)] = value;
        }
        return bytes;
    }
    read(start, length) {
        const [at, count] = range(start, length, this.size);
        if (count > PAGE_SIZE) return this.exact(at, count);
        const aligned = at / BigInt(PAGE_SIZE) * BigInt(PAGE_SIZE);
        const relative = Number(at - aligned);
        if (relative + count > PAGE_SIZE) return this.exact(at, count);
        let page = this.cache.get(aligned);
        if (!page) {
            page = this.exact(aligned, Number((this.size-aligned) < BigInt(PAGE_SIZE) ? this.size-aligned : BigInt(PAGE_SIZE)));
            if (this.cache.size === 4) this.cache.delete(this.cache.keys().next().value);
            this.cache.set(aligned, page);
        }
        return page.subarray(relative, relative + count);
    }
}
