import test from 'node:test';
import assert from 'node:assert/strict';
import {FileBackend, READ_LIMIT, MAX_EDIT_BYTES, MAX_HISTORY, PAGE_SIZE, offset, range} from '../web/file-backend.mjs';
import * as edits from '../web/byte-pieces.mjs';

function fixture(size = 40 * 1024 ** 3) {
    const reads = [];
    const file = {
        name: 'sparse.bin', size, lastModified: 1,
        slice(start, end) {
            assert.ok(end - start <= READ_LIMIT);
            reads.push([start, end]);
            return {async arrayBuffer() {
                const bytes = new Uint8Array(end - start);
                for (let i = 0; i < bytes.length; i++) bytes[i] = (start + i) % 256;
                return bytes.buffer;
            }};
        },
        arrayBuffer() { throw Error('Whole-file reads are forbidden'); },
    };
    const handle = {getFile: async () => file, isSameEntry: async other => other === handle};
    return {source: new FileBackend(file, handle), file, handle, reads};
}
function destination({fail = false, write = () => {}} = {}) {
    const state = {count: 0, max: 0, closed: false, aborted: false, pending: 0};
    const handle = {async createWritable() { return {
        async write(bytes) {
            state.pending++;
            assert.equal(state.pending, 1, 'writes must be awaited for backpressure');
            state.max = Math.max(state.max, bytes.length);
            state.count += bytes.length;
            write(bytes);
            await Promise.resolve();
            state.pending--;
            if (fail) throw Error('disk full');
        },
        async close() { state.closed = true; },
        async abort() { state.aborted = true; },
    }; }};
    return {handle, state};
}

test('40 GiB source reads only requested tail ranges, including offsets beyond wasm32', async () => {
    const {source, reads} = fixture();
    assert.equal(reads.length, 0);
    const at = source.size - 4n;
    assert.deepEqual(await source.read(at.toString(), 65536), new Uint8Array([252, 253, 254, 255]));
    assert.deepEqual(reads, [[Number(at), Number(source.size)]]);
    assert.equal((await source.read(source.size.toString(), 1)).length, 0);
    await assert.rejects(source.read((source.size + 1n).toString(), 1), /outside/);
    await assert.rejects(source.read('0', READ_LIMIT + 1), /limit/);
});
test('integer validation rejects rounding, negative offsets, malformed values and oversized lengths', () => {
    assert.equal(offset('9007199254740991'), 9007199254740991n);
    for (const value of ['9007199254740992', '-1', '1.5', '1e3', 42]) assert.throws(() => offset(value));
    for (const length of [-1, 1.5, Infinity, READ_LIMIT + 1]) assert.throws(() => range('0', length, 100n));
    assert.throws(() => new FileBackend({size: Number.MAX_SAFE_INTEGER + 1}), /exact/);
});
test('high-offset edits, undo and redo read no whole file', async () => {
    const {source, reads} = fixture();
    const at = (source.size - 1n).toString();
    await source.replace(at, 7);
    assert.deepEqual(await source.read(at, 1), new Uint8Array([7]));
    source.history();
    assert.deepEqual(await source.read(at, 1), new Uint8Array([255]));
    source.history(true);
    assert.deepEqual(await source.read(at, 1), new Uint8Array([7]));
    assert.ok(reads.every(([start, end]) => end - start === 1));
    assert.equal(source.dirty, true);
});
test('whole-field edits are atomic, bounded, and one undo step', async () => {
    const {source} = fixture(16);
    await source.replaceRange('4', [9, 8, 7]);
    assert.deepEqual(await source.read('4', 3), new Uint8Array([9, 8, 7]));
    assert.equal(source.undo.length, 1);
    source.history();
    assert.deepEqual(await source.read('4', 3), new Uint8Array([4, 5, 6]));
    source.history(true);
    assert.deepEqual(await source.read('4', 3), new Uint8Array([9, 8, 7]));
    const pieces = source.pieces;
    const steps = source.undo.length;
    await assert.rejects(source.replaceRange('15', [1, 2]), /Invalid byte replacement/);
    await assert.rejects(source.replaceRange('4', [1, 256]), /Invalid byte replacement/);
    assert.equal(source.pieces, pieces);
    assert.equal(source.undo.length, steps);
});
test('edits over the memory limit fail without changing the source', async () => {
    const {source} = fixture(64);
    source.pieces = [edits.added(new Uint8Array(MAX_EDIT_BYTES - 1)), ...edits.splice(source.pieces, 0n, 0n, []).pieces];
    source.size = edits.sizeOf(source.pieces);
    const pieces = source.pieces;
    await assert.rejects(source.insert(source.size.toString(), [1, 2]), /edit limit/);
    assert.equal(source.pieces, pieces);
    assert.equal(source.undo.length, 0);
});
test('streams an edited high-offset selection in bounded chunks with backpressure', async () => {
    const {source} = fixture();
    const first = source.size - BigInt(READ_LIMIT * 2 + 3);
    await source.replace(first.toString(), 7);
    let firstByte;
    const output = destination({write: bytes => { firstByte ??= bytes[0]; }});
    await source.export(output.handle, first.toString(), source.size.toString());
    assert.equal(output.state.count, READ_LIMIT * 2 + 3);
    assert.equal(output.state.max, READ_LIMIT);
    assert.equal(output.state.closed, true);
    assert.equal(output.state.aborted, false);
    assert.equal(firstByte, 7);
    assert.equal(source.dirty, true, 'selection export does not mark full file saved');
});
test('empty export commits and full copy clears dirty state', async () => {
    const {source} = fixture(0);
    const output = destination();
    await source.export(output.handle, '0', '0');
    assert.equal(output.state.count, 0);
    assert.equal(output.state.closed, true);
    const {source: small} = fixture(8);
    await small.replace('2', 55);
    await small.export(destination().handle, '0', '8');
    assert.equal(small.dirty, false);
    small.history();
    assert.equal(small.dirty, true);
});
test('source aliases, changed source and unsupported safe export fail before opening output', async () => {
    const {source, handle, file} = fixture();
    await assert.rejects(source.export(handle, '0', '8'), /open source/);
    source.handle = {...handle, getFile: async () => ({...file, lastModified: 2})};
    await assert.rejects(source.export(destination().handle, '0', '8'), /changed/);
    source.handle = null;
    await assert.rejects(source.export(destination().handle, '0', '8'), /picker/);
});
test('disk errors, cancellation and changes during export abort without committing', async () => {
    for (const mode of ['error', 'cancel', 'changed']) {
        const {source, file, handle} = fixture(READ_LIMIT * 2);
        let cancelled = false;
        const output = destination({fail: mode === 'error', write: () => {
            if (mode === 'cancel') cancelled = true;
            if (mode === 'changed') source.handle = {...handle, getFile: async () => ({...file, lastModified: 2})};
        }});
        await assert.rejects(source.export(output.handle, '0', source.size.toString(), () => cancelled));
        assert.equal(output.state.closed, false);
        assert.equal(output.state.aborted, true);
    }
});
test('history limits are independent of source size', async () => {
    const {source} = fixture();
    for (let i = 0; i < MAX_HISTORY + 1; i++) await source.replace(String(i * 2), 255 - ((i * 2) % 256));
    assert.equal(source.undo.length, MAX_HISTORY);
});
test('inserts and deletes shift later bytes on a 40 GiB file without reading it', async () => {
    const {source, reads} = fixture();
    const size = source.size;
    await source.insert('2', [0xAA, 0xBB]);
    assert.equal(source.size, size + 2n);
    assert.equal(source.originalSize, size);
    assert.deepEqual(await source.read('0', 6), new Uint8Array([0, 1, 0xAA, 0xBB, 2, 3]));
    await source.remove('1', (size + 1n).toString());
    assert.equal(source.size, 2n);
    assert.deepEqual(await source.read('0', 8), new Uint8Array([0, 255]));
    assert.ok(reads.every(([start, end]) => end - start <= 6), 'no bulk reads');
    source.history();
    assert.equal(source.size, size + 2n);
    source.history();
    assert.equal(source.size, size);
    assert.deepEqual(await source.read('0', 4), new Uint8Array([0, 1, 2, 3]));
    source.history(true);
    assert.deepEqual(await source.read('0', 4), new Uint8Array([0, 1, 0xAA, 0xBB]));
    await assert.rejects(source.insert((source.size + 1n).toString(), [1]), /Invalid/);
    await assert.rejects(source.remove('5', '5'), /Invalid/);
});
test('patterns and fills repeat without expanding, and inserted files stay in their file', async () => {
    const {source} = fixture(8);
    await source.insertPattern('2', [0xDE, 0xAD], 3);
    assert.deepEqual(await source.read('0', 16), new Uint8Array([0, 1, 0xDE, 0xAD, 0xDE, 0xAD, 0xDE, 0xAD, 2, 3, 4, 5, 6, 7]));
    assert.deepEqual(await source.read('3', 3), new Uint8Array([0xAD, 0xDE, 0xAD]), 'reads start mid-pattern');
    await source.fill('0', '2', [9]);
    assert.deepEqual(await source.read('0', 3), new Uint8Array([9, 9, 0xDE]));
    const part = new File([new Uint8Array([80, 65, 82, 84])], 'part.bin');
    await source.insertFile('14', part);
    assert.deepEqual(await source.read('12', 8), new Uint8Array([6, 7, 80, 65, 82, 84]));
    await assert.rejects(source.insertFile('0', new File([], 'empty.bin')), /not empty/);
    const {source: large} = fixture(16);
    await large.insertPattern('8', [0], 2 ** 40);
    assert.equal(large.size, 2n ** 40n + 16n);
    assert.ok(edits.memory(large.pieces) < 16, 'a 1 TiB pattern is not expanded');
    assert.deepEqual(await large.read((large.size - 10n).toString(), 10), new Uint8Array([0, 0, 8, 9, 10, 11, 12, 13, 14, 15]));
    await assert.rejects(large.insertPattern('0', [], 3), /Invalid/);
    await assert.rejects(large.fill('4', (large.size + 1n).toString(), [1]), /Invalid/);
});
test('saving in place streams through the open file handle and aborts on failure', async () => {
    const {source, file} = fixture(READ_LIMIT + 4);
    await source.insert('0', [7]);
    const writes = [];
    let closed = false;
    source.handle = {getFile: async () => file, async createWritable(options) {
        assert.equal(options.keepExistingData, false);
        return {async write(bytes) { writes.push(bytes.length); }, async close() { closed = true; }, async abort() {}};
    }};
    await source.replaceSource();
    assert.equal(closed, true);
    assert.equal(writes.reduce((a, b) => a + b, 0), READ_LIMIT + 5);
    const {source: failing, file: failingFile} = fixture(8);
    let aborted = false;
    failing.handle = {getFile: async () => failingFile, async createWritable() {
        return {async write() { throw Error('disk full'); }, async close() { throw Error('must not commit'); },
            async abort() { aborted = true; }};
    }};
    await assert.rejects(failing.replaceSource(), /disk full/);
    assert.equal(aborted, true);
    failing.handle = null;
    await assert.rejects(failing.replaceSource(), /picker/);
});

test('decoder workers reuse bounded session tiles and evict least recently used pages', async () => {
    const {DecoderSource, DECODER_PAGE_SIZE, DECODER_CACHE_BYTES} = await import('../web/decoder-source.mjs');
    let reads = 0;
    const file = {size: 64 * DECODER_PAGE_SIZE, slice(start, end) {
        assert.ok(end - start <= READ_LIMIT);
        return {start, end};
    }, arrayBuffer() { throw Error('No whole-file copy'); }};
    const budget = 4 * DECODER_PAGE_SIZE;
    assert.equal(DECODER_CACHE_BYTES, 1024 ** 3);
    const pieces = edits.splice(edits.original(BigInt(file.size)), 4n, 1n, [edits.added([19])]).pieces;
    const source = new DecoderSource(file, pieces, ({start, end}) => {
        reads++;
        return new Uint8Array(end-start).buffer;
    }, budget);
    assert.equal(source.read('4', 1)[0], 19);
    const tile = reads;
    source.read('100', 4000);
    assert.equal(reads, tile, 'Nearby queries share their file tile');
    const pages = budget / DECODER_PAGE_SIZE;
    for (let page = 1; page < pages; page++) source.read(String(page * DECODER_PAGE_SIZE), 1);
    source.read('4', 1); // Touch the first page before filling the next.
    source.read(String(pages * DECODER_PAGE_SIZE), 1);
    assert.ok(source.cache.has(0n));
    assert.ok(!source.cache.has(BigInt(DECODER_PAGE_SIZE)));
    assert.ok([...source.cache.values()].reduce((sum, page) => sum + page.byteLength, 0) <= budget);
    source.setCacheBudget(DECODER_PAGE_SIZE);
    assert.equal(source.cache.size, 1);
    assert.equal(source.cachedBytes, DECODER_PAGE_SIZE);
    source.setCacheBudget(0);
    assert.equal(source.cachedBytes, 0);
    assert.equal(source.read('4', 1)[0], 19);
    assert.equal(source.cache.size, 0);
});

test('default decoder cache retains more than the former 32 MiB ceiling', async () => {
    const {DecoderSource, DECODER_PAGE_SIZE} = await import('../web/decoder-source.mjs');
    const file = {size: 64 * DECODER_PAGE_SIZE, slice(start, end) { return {start, end}; }};
    let reads = 0;
    const source = new DecoderSource(file, null, ({start, end}) => {
        reads++;
        return new Uint8Array(end - start).buffer;
    });
    assert.equal(source.cachedBytes, 0, 'Budget is not an upfront allocation');
    for (let page = 0; page < 40; page++) source.read(String(page * DECODER_PAGE_SIZE), 1);
    source.read('0', 1);
    assert.equal(reads, 40, 'First tile survives reads beyond the old cap');
    assert.equal(source.cachedBytes, 40 * DECODER_PAGE_SIZE);
});

test('decoder workers read inserted, deleted and patterned bytes at their new offsets', async () => {
    const {DecoderSource} = await import('../web/decoder-source.mjs');
    const file = {size: 8, slice(start, end) { return {start, end}; }};
    let pieces = edits.original(8n);
    pieces = edits.splice(pieces, 2n, 3n, [{kind: 'pattern', pattern: new Uint8Array([0xAB]), phase: 0, len: 4n}]).pieces;
    const source = new DecoderSource(file, pieces, ({start, end}) =>
        Uint8Array.from({length: end - start}, (_, i) => start + i).buffer, 0);
    assert.equal(source.size, 9n);
    assert.deepEqual(source.read('0', 9), new Uint8Array([0, 1, 0xAB, 0xAB, 0xAB, 0xAB, 5, 6, 7]));
});
