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
const embeddedRunner = bindings.python_runner();
assert.equal(embeddedRunner, await readFile('python/runner.py','utf8'),
    'Browser and native Python adapters must use the same runner source');
const plan={endian:'little',structures:[{name:'Sample',fields:[{name:'value',type:'u16'}]}],
    actions:[{read:{name:'samples',type:{structure:'Sample'},at:0,length:2,repeat:size/2}}]};
const pythonDecoded=JSON.parse(session.decode_plan_json(JSON.stringify(plan)));
assert.equal(pythonDecoded.collections[0].count,String(size/2));
const decoded=JSON.parse(session.decode(yaml));
assert.equal(decoded.collections[0].count,String(size/2));
assert.equal(decoded.nodes[0].end,String(size));
assert.ok(reads.length<10,'Lazy schema must not scan the file');
const path=decoded.collections[0].path;
const table=JSON.parse(session.table(path,String(size/2-2)));
assert.equal(table.rows.length,2);
assert.equal(table.rows[1].start,String(size-2));
assert.equal(table.rows[1].cells.value.number,65535,'Sparse edit appears in decoded values');
const treePage=JSON.parse(session.tree_page(path,String(size/2-2)));
assert.equal(treePage.nodes.length,2);
assert.equal(treePage.nodes[1].start,String(size-2));
assert.equal(treePage.count,String(size/2));
assert.equal(treePage.page_size,64);
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
const inspected=JSON.parse(session.inspect(String(size-2),String(size)));
assert.equal(inspected.field.name,'value');
assert.equal(inspected.field.value,'65535');
assert.equal(inspected.field.sample,size/2-1);
assert.equal(inspected.field.sample_start,size-2);
assert.equal(inspected.field.sample_end,size);
assert.equal(inspected.field.editable,true);
assert.deepEqual(JSON.parse(session.encode(String(size-2),String(size),'1234')).bytes,[210,4]);
assert.throws(()=>session.encode(String(size-2),String(size),'65536'));
assert.throws(()=>session.encode(String(size-1),String(size),'3'));
session.free();
console.log(`Browser WASM: YAML, lazy 40 GiB offsets, sparse edits, tables, exact plots, bounded overview, and failed-apply retention passed (${reads.length} bounded slices).`);

// Incremental reverse engineering: a failed count-based guess can be replaced
// with a header-only decoder, without consuming the unknown remainder.
// Entirely fictional header; unrelated to any user file format.
const headerBytes = new Uint8Array([0x44, 0x45, 0x4d, 0x4f, 0, 0, 0, 0]);
globalThis.binwaveDecoder = {read(start,length) { return headerBytes.slice(Number(start), Number(start)+length); }};
const headerSession = new bindings.DecoderSession('8');
const failedGuess = JSON.parse(headerSession.decode(`endian: little
structures:
  - name: Header
    fields: [{name: count, type: u32}]
actions:
  - read: {name: header, type: {structure: Header}}
  - read: {name: samples, type: f64, repeat: {ref: header.count}}
`));
assert.equal(failedGuess.partial, true);
assert.ok(failedGuess.error);
const partial = JSON.parse(headerSession.decode(`actions:
  - read: {name: signature, type: string, length: 4}
#  - read: {name: samples, type: f64, repeat: until end}
`));
assert.equal(partial.consumed, '4');
assert.equal(partial.collections.length, 0);
assert.equal(partial.nodes[0].value, 'DEMO');
headerSession.free();

// A mismatched fixed-size repeat exposes the first complete-shaped record in
// the partial structure instead of discarding the entire collection.
const partialBytes = new Uint8Array(2 * 129 * 2 + 2);
new DataView(partialBytes.buffer).setUint16(0, 1, true);
partialBytes.set([9, 9], partialBytes.length - 2);
globalThis.binwaveDecoder = {read(start,length) { return partialBytes.slice(Number(start), Number(start)+length); }};
const partialSession = new bindings.DecoderSession(String(partialBytes.length));
const partialBlocks = JSON.parse(partialSession.decode(`endian: little
structures:
  - name: Block
    fields:
      - {name: samples, type: u16, repeat: 129}
actions:
  - read: {name: blocks, type: {structure: Block}, repeat: until end}
`));
assert.equal(partialBlocks.partial, true);
assert.equal(partialBlocks.partial_preview_path, 'blocks[0].samples');
assert.equal(partialBlocks.nodes[0].count, '2');
assert.equal(partialBlocks.nodes[0].children[0].path, 'blocks[0]');
assert.equal(partialBlocks.nodes[0].children[0].children[0].count, '129');
assert.equal(partialBlocks.nodes[0].children[0].children[0].children[0].value, '1');
const nestedTreePage=JSON.parse(partialSession.tree_page('blocks[0].samples','0'));
assert.equal(nestedTreePage.count,'129');
assert.equal(nestedTreePage.nodes[0].value,'1');
partialSession.free();

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
assert.deepEqual(JSON.parse(simple.pick_range('samples', '16', '19')), {path:'samples', first:'16', last:'19', start:'64', end:'80'});
assert.throws(() => simple.pick('samples', '512'));
assert.equal(JSON.parse(simple.probe('samples', '2048', '2049')).first, null);
simple.free();
console.log('Shipped simple wave: 512 float32 samples, eight sine cycles and exact byte links passed.');

// Nested fixed-size arrays must be available across their parent records in
// the browser, matching the desktop collection browser without whole-file reads.
const nestedBytes = new Uint8Array(129 * 129 * 2);
const nestedView = new DataView(nestedBytes.buffer);
for (let i = 0; i < 129 * 129; i++) nestedView.setUint16(i * 2, i, true);
globalThis.binwaveDecoder = {read(start, length) {
    return nestedBytes.slice(Number(start), Number(start) + length);
}};
const nested = new bindings.DecoderSession(String(nestedBytes.length));
const nestedDecoded = JSON.parse(nested.decode(`endian: little
structures:
  - name: Block
    fields:
      - {name: samples, type: u16, repeat: 129}
actions:
  - read: {name: blocks, type: {structure: Block}, repeat: 129}
`));
assert.deepEqual(nestedDecoded.collections.map(c => [c.path, c.count]),
    [['blocks', '129'], ['blocks.samples', String(129 * 129)]]);
assert.deepEqual(JSON.parse(nested.plot('blocks.samples', '129', '133', 800)).series[0].points,
    Array.from({length: 4}, (_, i) => [129 + i, 129 + i]));
assert.equal(JSON.parse(nested.table('blocks.samples', '5')).rows[0].cells.value.number, 5);
assert.equal(JSON.parse(nested.pick('blocks.samples', '5')).start, '10');
nested.free();

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
assert.deepEqual(JSON.parse(variableSession.table('field', '0')), previousStructure, 'Failed interpretation must retain the published structure');
const missingReference = JSON.parse(variableSession.decode_interpretation(variableYaml.replace('ref: count', 'ref: missing'), 'field', '0', '8'));
assert.equal(missingReference.partial, true);
assert.match(missingReference.error, /missing/);
variableSession.free();
console.log('Browser WASM: custom structures, actual variable byte ranges, short selections and missing references passed.');

// Variable-size choose records stay lazy well past the old 20,000-node cap.
const interleavedBytes = new Uint8Array(30_000 * 4);
const interleavedView = new DataView(interleavedBytes.buffer);
for (let i = 0, at = 0; i < 30_000; i++) {
    interleavedBytes[at++] = i % 2 ? 2 : 1;
    if (i % 2) { interleavedView.setUint32(at, i, true); at += 4; }
    else { interleavedView.setUint16(at, i, true); at += 2; }
}
globalThis.binwaveDecoder = {read(start,length) {
    return interleavedBytes.slice(Number(start), Number(start) + length);
}};
const interleaved = new bindings.DecoderSession(String(interleavedBytes.length));
const chooseYaml = [
    'structures:',
    '  - name: Short',
    '    fields: [{name: value, type: u16}]',
    '  - name: Long',
    '    fields: [{name: value, type: u32}]',
    '  - name: Record',
    '    fields:',
    '      - {name: tag, type: u8}',
    "      - name: payload",
    "        type: {choose: {ref: tag, cases: {'1': {structure: Short}, '2': {structure: Long}}}}",
    'actions:',
    '  - read: {name: records, type: {structure: Record}, repeat: until end}',
].join('\n');
const chooseDecoded = JSON.parse(interleaved.decode(chooseYaml));
assert.equal(chooseDecoded.collections[0].count, '30000');
const oddRecord = JSON.parse(interleaved.pick('records', '1457'));
assert.equal(oddRecord.start, '5827');
const oddField = JSON.parse(interleaved.inspect('5828', '5832'));
assert.equal(oddField.field.value, '1457');
assert.equal(oddField.field.sample, 1457);
assert.deepEqual(JSON.parse(interleaved.encode('5828','5832','12345')).bytes,[57,48,0,0]);
interleaved.free();
console.log('Browser WASM: 30,000 interleaved choose records, on-demand Inspector and editing passed.');


const advancedBytes = await readFile('samples/quickstart/advanced_wave.bin');
globalThis.binwaveDecoder = {read(start, length) {
    return new Uint8Array(advancedBytes.subarray(Number(start), Number(start) + length));
}};
const advanced = new bindings.DecoderSession(String(advancedBytes.length));
const decodedAdvanced = JSON.parse(advanced.decode(await readFile('samples/advanced/decoder.yaml', 'utf8')));
assert.deepEqual(decodedAdvanced.collections.map(c => [c.path,c.count]), [['waves','384'],['measurements','192'],['events','96']]);
const overlay = JSON.parse(advanced.plot_many(JSON.stringify(['waves','measurements','events']), '0','384',800));
assert.equal(overlay.series.length, 9);
assert.equal(overlay.estimated,false);
const temperature = overlay.series.find(c => c.name === 'measurements / temperature');
assert.equal(temperature.points.length,192);
assert.deepEqual(temperature.points[0],[0,20]);
assert.deepEqual(temperature.bytes[0],['6160','6164']);
assert.equal(overlay.series.find(c => c.name === 'events / code').points.length,96);
const tail = JSON.parse(advanced.plot_many('["waves","measurements","events"]','200','384',800));
assert.ok(tail.series.filter(c => c.path !== 'waves').every(c=>c.points.length===0));
assert.equal(JSON.parse(advanced.plot_many('["waves","waves"]','0','4',800)).series.length,5);
advanced.free();
const manyBytes = new Uint8Array(65*2*10000);
const manyView = new DataView(manyBytes.buffer);
for(let i=0;i<10000;i++) for(let f=0;f<65;f++) manyView.setUint16((i*65+f)*2,f+i,true);
globalThis.binwaveDecoder={read(start,length){return manyBytes.slice(Number(start),Number(start)+length);}};
const many=new bindings.DecoderSession(String(manyBytes.length));
const manyYaml='endian: little\nstructures:\n  - name: Wide\n    fields:\n'+Array.from({length:65},(_,i)=>'      - {name: f'+i+', type: u16}').join('\n')+'\nactions:\n  - read: {name: samples, type: {structure: Wide}, repeat: until end}\n';
many.decode(manyYaml);
for(const end of ['4','10000']) {
    const plotted=JSON.parse(many.plot('samples','0',end,128));
    assert.equal(plotted.series.length,65);
    const last=plotted.series.find(c=>c.name==='f64');
    assert.deepEqual(last.points[0],[0,64]);
    assert.deepEqual(last.bytes[0],['128','130']);
}
many.free();
console.log('Multiple collections: unequal lengths, index alignment, byte links, and 65 exact/reduced fields passed.');
