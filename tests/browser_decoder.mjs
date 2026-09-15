// Run after `trunk build --release`; exercises the actual browser WASM engine in
// Node with a synthetic file, not a replacement implementation of the decoder.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {openSync, closeSync, fstatSync, readSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import {resolve} from 'node:path';
import {DecoderSource} from '../web/decoder-source.mjs';
const html=await readFile('dist/web/index.html','utf8');
const js=html.match(/rel="modulepreload" href="\.\/([^"]+)"/)[1];
const wasm=html.match(/href="\.\/([^"]+\.wasm)"/)[1];
const size=40*1024**3;
const reads=[];
const file={size, slice(start,end) {
    assert.ok(end-start<=1024**2,'No read may exceed 1 MiB');
    assert.ok(start>=0 && end<=size);
    reads.push([start,end]);
    return {start,end};
}, arrayBuffer(){throw Error('No whole-file reads');}};
const source=new DecoderSource(file,[[BigInt(size-2),255]],({start,end})=> {
    const bytes=new Uint8Array(end-start);
    for(let i=0;i<bytes.length;i++)bytes[i]=(start+i)%256;
    return bytes.buffer;
});
globalThis.binwaveDecoder={read:(start,length)=>source.read(start,length)};
const bindings=await import(pathToFileURL(resolve('dist/web',js)));
await bindings.default({module_or_path:await readFile(resolve('dist/web',wasm))});
const session=new bindings.DecoderSession(String(size));
const yaml=`endian: little
structures:
  - name: Sample
    fields:
      - {name: value, type: u16}
actions:
  - read:
      name: samples
      type: {structure: Sample}
      at: 0
      repeat: until end
`;
const decoded=JSON.parse(session.decode(yaml));
assert.equal(decoded.collections[0].count,String(size/2));
assert.equal(decoded.nodes[0].end,String(size));
assert.ok(reads.length<10,'Lazy schema must not scan the file');
const path=decoded.collections[0].path;
const table=JSON.parse(session.table(path,String(size/2-2)));
assert.equal(table.rows.length,2);
assert.equal(table.rows[1].start,String(size-2));
assert.equal(table.rows[1].cells.value.number,65535,'Sparse edit appears in decoded values');
const plot=JSON.parse(session.plot(path,String(size/2-2),String(size/2),800));
assert.deepEqual(plot.series[0].points,[[size/2-2,65020],[size/2-1,65535]]);
const overview=JSON.parse(session.plot(path,'0',String(size/2),800));
assert.equal(plot.estimated,false);
assert.equal(plot.series[0].bands.length,0);
assert.equal(overview.estimated,true);
assert.equal(overview.series[0].bands.length,128);
assert.ok(overview.series[0].bands.every(([left,right,low,high])=>left<right && low<=high));
assert.ok(overview.scanned<=65536,'Overview work must be bounded');
assert.ok(overview.series[0].points.length<10000,'Plot response must be bounded');
assert.throws(()=>session.decode('invalid: definition'));
assert.throws(()=>session.decode('structures: {Sample: {fields: []}}\nactions: [{skip: 0}]'));
assert.throws(()=>session.decode('actions: [{read: {name: x, type: Sample}}]'));
assert.equal(JSON.parse(session.table(path,String(size/2-1))).rows[0].cells.value.number,65535,'Failed apply retains successful session');
assert.ok(source.cache.size<=4);
assert.throws(()=>source.read('0',1024**2+1));
assert.equal(JSON.parse(session.probe(path, String(size-1), String(size))).first, String(size/2-1));
assert.equal(JSON.parse(session.pick(path, String(size/2-1))).start, String(size-2));
session.free();
console.log(`Browser WASM: YAML, lazy 40 GiB offsets, sparse edits, tables, exact plots, bounded overview, and failed-apply retention passed (${reads.length} bounded slices).`);

// Incremental reverse engineering: a failed count-based guess can be replaced
// with a header-only decoder, without consuming the unknown remainder.
// Entirely fictional header; unrelated to any user file format.
const headerBytes = new Uint8Array([0x44, 0x45, 0x4d, 0x4f, 0, 0, 0, 0]);
globalThis.binwaveDecoder = {read(start,length) { return headerBytes.slice(Number(start), Number(start)+length); }};
const headerSession = new bindings.DecoderSession('8');
assert.throws(() => headerSession.decode(`endian: little
structures:
  - name: Header
    fields: [{name: count, type: u32}]
actions:
  - read: {name: header, type: {structure: Header}}
  - read: {name: samples, type: f64, repeat: {ref: header.count}}
`));
const partial = JSON.parse(headerSession.decode(`actions:
  - read: {name: signature, type: text, length: 4}
#  - read: {name: samples, type: f64, repeat: until end}
`));
assert.equal(partial.consumed, '4');
assert.equal(partial.collections.length, 0);
assert.equal(partial.nodes[0].value, 'DEMO');
headerSession.free();

// The shipped beginner pair must produce the documented plot and byte links.
const simpleBytes = await readFile('samples/quickstart/simple_wave.bin');
assert.equal(simpleBytes.length, 2048);
globalThis.binwaveDecoder = {read(start, length) {
    return new Uint8Array(simpleBytes.subarray(Number(start), Number(start) + length));
}};
const simple = new bindings.DecoderSession(String(simpleBytes.length));
const simpleYaml = await readFile('samples/quickstart/simple_wave.yaml', 'utf8');
const simpleDecoded = JSON.parse(simple.decode(simpleYaml));
assert.equal(simpleDecoded.collections[0].count, '512');
const simplePlot = JSON.parse(simple.plot('samples', '0', '512', 800));
assert.equal(simplePlot.estimated, false);
assert.equal(simplePlot.series.length, 1);
assert.equal(simplePlot.series[0].points.length, 512);
for (let i = 0; i < 512; i++) {
    assert.ok(Math.abs(simplePlot.series[0].points[i][1] - Math.sin(2 * Math.PI * i / 64)) < 1e-6);
}
const simpleTable = JSON.parse(simple.table('samples', '16'));
assert.equal(simpleTable.rows[0].start, '64');
assert.equal(simpleTable.rows[0].cells.value.number, 1);
assert.deepEqual(JSON.parse(simple.probe('samples', '64', '68')), {path:'samples', first:'16', end:'17'});
assert.equal(JSON.parse(simple.probe('samples', '67', '68')).first, '16');
assert.deepEqual(JSON.parse(simple.pick('samples', '16')), {path:'samples', index:'16', start:'64', end:'68'});
assert.throws(() => simple.pick('samples', '512'));
assert.equal(JSON.parse(simple.probe('samples', '2048', '2049')).first, null);
simple.free();
console.log('Shipped simple wave: 512 float32 samples, eight sine cycles and exact byte links passed.');

// Optional generated PWAV fixture: run the user's real schema through WASM,
// then check both a resolvable waveform and the estimated overview bands.
if (process.argv[2]) {
    const fd=openSync(process.argv[2],'r');
    try {
        const size=fstatSync(fd).size;
        globalThis.binwaveDecoder={read(start,length) {
            assert.ok(length<=1024**2);
            const bytes=new Uint8Array(length);
            assert.equal(readSync(fd,bytes,0,length,Number(start)),length);
            return bytes;
        }};
        const waveform=new bindings.DecoderSession(String(size));
        const decoded=JSON.parse(waveform.decode(await readFile('samples/decoders/yaml/waveform.yaml','utf8')));
        const {path,count}=decoded.collections.find(c=>c.path.endsWith('.payload.records'));
        const detail=JSON.parse(waveform.plot(path,'0','384',800));
        const sine=detail.series.find(c=>c.name==='sine');
        assert.equal(detail.estimated,false);
        assert.equal(sine.points.length,384);
        assert.equal(sine.points[0][1],0);
        assert.equal(sine.points[16][1],4);
        assert.equal(sine.points[48][1],-4);
        const overview=JSON.parse(waveform.plot(path,'0',count,800));
        assert.equal(overview.estimated,true);
        assert.equal(overview.series.length,12);
        assert.ok(overview.scanned<=65536);
        const bands=overview.series.find(c=>c.name==='sine').bands;
        assert.equal(bands.length,128);
        assert.ok(bands.every(([, ,low,high])=>low===-4 && high===4));
        waveform.free();
        console.log('PWAV schema: 12 channels, exact 384-sample sine, and 128 overview amplitude bands passed.');
    } finally {closeSync(fd);}
}

// Direct interpretations use the real engine and preserve the prior session on failure.
const variableBytes = new Uint8Array([3, 65, 66, 67, 0, 0, 0, 0]);
globalThis.binwaveDecoder = {read(start,length) { return variableBytes.slice(Number(start), Number(start)+length); }};
const variableSession = new bindings.DecoderSession('8');
const variableYaml = `structures:
  - name: Header
    fields:
      - {name: count, type: u8}
      - {name: payload, type: bytes, length: {ref: count}}
actions:
  - read: {name: field, type: {structure: Header}, at: 0}
`;
const interpreted = JSON.parse(variableSession.decode_interpretation(variableYaml, 'field', '0', '8'));
assert.equal(interpreted.nodes[0].end, '4');
assert.equal(interpreted.nodes[0].children[1].start, '1');
assert.equal(interpreted.nodes[0].children[1].end, '4');
const previousStructure = JSON.parse(variableSession.table('field', '0'));
assert.throws(() => variableSession.decode_interpretation(variableYaml, 'field', '0', '2'), /Selection is too short/);
assert.throws(() => variableSession.decode_interpretation(variableYaml.replace('ref: count', 'ref: missing'), 'field', '0', '8'));
assert.deepEqual(JSON.parse(variableSession.table('field', '0')), previousStructure, 'Failed interpretation must retain the published structure');
variableSession.free();
console.log('Browser WASM: custom structures, actual variable byte ranges, short selections and missing references passed.');
