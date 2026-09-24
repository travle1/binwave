// Run after npm ci. Exercise the actual bundled Python interpreter and the
// exact runner source embedded into BinWave's WASM build.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import {runPythonDecoder} from '../web/python-runtime.mjs';

const bytes = new Uint8Array([7, 0, 8, 0]);
const reads = [];
const source = {
    read(start, length) {
        const offset = Number(start);
        reads.push([offset, length]);
        assert.ok(Number.isSafeInteger(offset) && offset >= 0 && length <= 1024 * 1024);
        return bytes.slice(offset, offset + length);
    },
};
const runnerSource = await readFile('python/runner.py', 'utf8');
const runtimeBase = new URL('../node_modules/pyodide/', import.meta.url).href;
const runtimeIndex = fileURLToPath(runtimeBase) + '/';
const answer = await runPythonDecoder({
    file: {size: bytes.length},
    filename: 'decoder.py',
    script: 'from binwave import Struct\n'
        + 'print(file.read_at(0, 2).hex())\n'
        + "file.records('samples', Struct('Sample', [('value', 'u16')]))\n",
}, source, runnerSource, runtimeBase, runtimeIndex);
assert.equal(answer.done.stdout, '0700\n');
assert.equal(answer.done.program.actions[0].read.repeat, 2);
assert.deepEqual(reads, [[0, 4]]);
console.log('Bundled Pyodide executes the shared BinWave Python runner and bounded reads.');
