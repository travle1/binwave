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
  Sample:
    fields:
      - {name: value, type: u16}
actions:
  - read:
      name: samples
      type: Sample
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
assert.throws(()=>session.decode('invalid: schema'));
assert.equal(JSON.parse(session.table(path,String(size/2-1))).rows[0].cells.value.number,65535,'Failed apply retains successful session');
assert.ok(source.cache.size<=4);
assert.throws(()=>source.read('0',1024**2+1));
session.free();
console.log(`Browser WASM: YAML, lazy 40 GiB offsets, sparse edits, tables, exact plots, bounded overview, and failed-apply retention passed (${reads.length} bounded slices).`);

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
