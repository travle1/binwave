import test from 'node:test';
import assert from 'node:assert/strict';
import {FileBackend, READ_LIMIT, MAX_EDITS, MAX_HISTORY, offset, range} from '../web/file-backend.mjs';

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
test('edit and history limits are independent of source size', async () => {
    const {source} = fixture();
    for (let i = 0; i < MAX_HISTORY + 1; i++) await source.replace(String(i), 255 - (i % 256));
    assert.equal(source.undo.length, MAX_HISTORY);
    while (source.patches.size < MAX_EDITS) source.patches.set(BigInt(source.patches.size), 0);
    await assert.rejects(source.replace(String(MAX_EDITS), 1), /edit limit/);
    assert.equal(source.patches.size, MAX_EDITS);
});
