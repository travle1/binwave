import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {File} from 'node:buffer';
import {readFileSync} from 'node:fs';

function bridge(allowReplace = true) {
    const commands = [];
    const elements = new Map();
    const document = {
        currentScript: {src: 'http://localhost:8080/web/bridge.js'},
        getElementById(id) {
            if (!elements.has(id)) elements.set(id, {disabled: false, hidden: true, textContent: ''});
            return elements.get(id);
        },
    };
    const window = {addEventListener() {}, location: {href: 'http://localhost:8080/'}};
    class Worker { postMessage(message) { commands.push(message); } }
    vm.runInNewContext(readFileSync(new URL('../web/bridge.js', import.meta.url), 'utf8'), {
        document, window, Worker, URL, File, confirm: () => allowReplace,
    });
    return {commands, app: window.binwave, get: id => document.getElementById(id)};
}

test('file opening waits until the graphics workspace is ready', () => {
    const {app, get} = bridge();
    assert.equal(get('open').disabled, true);
    app.ready();
    assert.equal(get('open').disabled, false);
    assert.match(get('status').textContent, /Ready/);
    assert.equal(get('save').disabled, true);
});

test('graphics failure presents recovery details and disables inaccessible file operations', () => {
    const {app, get} = bridge();
    app.error('No usable GPU adapter');
    assert.equal(get('graphics-error').hidden, false);
    assert.equal(get('graphics-detail').textContent, 'No usable GPU adapter');
    assert.equal(get('graphics-url').textContent, 'http://localhost:8080/');
    assert.equal(get('binwave').hidden, true);
    assert.equal(get('open').disabled, true);
    assert.equal(get('save').disabled, true);
    assert.equal(get('extract').disabled, true);
});

test('decoder picker loads YAML independently of the data file', async () => {
    const {app, get} = bridge();
    assert.equal(get('open-decoder').disabled, true);
    app.ready();
    assert.equal(get('open-decoder').disabled, false);
    let clicks = 0;
    get('decoder-file').click = () => clicks++;
    get('open-decoder').onclick();
    assert.equal(clicks, 1);
    get('decoder-file').files = [{name:'samples.yaml',size:16,text:async ()=>'endian: little\n'}];
    await get('decoder-file').onchange();
    assert.deepEqual(JSON.parse(app.poll()),{type:'decoder-source',name:'samples.yaml',source:'endian: little\n'});
    assert.equal(get('decoder-file').value, '');
    assert.equal(get('save').disabled, true, 'Opening a decoder must not open a data file');
});

test('oversized and Python decoder files are rejected before reading', async () => {
    const {app, get} = bridge();
    app.ready();
    for (const file of [{name:'data.bin',size:40*1024**3},{name:'decoder.py',size:100}]) {
        get('decoder-file').files = [{...file,text:()=>{throw Error('Must not read');}}];
        await get('decoder-file').onchange();
        assert.equal(app.poll(), '');
        assert.match(get('status').textContent, /256 KiB|Python decoders require/);
        assert.equal(get('open-decoder').disabled, false);
    }
});

test('cancelled and failed decoder reads leave the current draft alone', async () => {
    const {app, get} = bridge();
    app.ready();
    get('decoder-file').files = [];
    await get('decoder-file').onchange();
    assert.equal(app.poll(), '');
    get('decoder-file').files = [{name:'decoder.yaml',size:16,text:async()=>{throw Error('File unavailable');}}];
    await get('decoder-file').onchange();
    assert.equal(app.poll(), '');
    assert.match(get('status').textContent,/File unavailable/);
    assert.equal(get('open-decoder').disabled, false);
});

test('embedded demo opens its bytes without a file picker and respects cancellation', async () => {
    const {app, commands} = bridge();
    const data = readFileSync(new URL('../samples/quickstart/simple_wave.bin', import.meta.url));
    assert.equal(app.openDemo(data), false, 'Graphics startup gates demo opening too');
    app.ready();
    assert.equal(app.openDemo(data), true);
    const command = commands.at(-1);
    assert.equal(command.type, 'open');
    assert.equal(command.demo, true);
    assert.equal(command.file.name, 'simple_wave.bin');
    assert.equal(command.file.size, 2048);
    assert.deepEqual(Buffer.from(await command.file.arrayBuffer()), data);
    assert.equal(app.openDemo(data), false, 'Do not overlap an in-flight file operation');
    const cancelled = bridge(false);
    cancelled.app.ready();
    assert.equal(cancelled.app.openDemo(data, true), false);
    assert.equal(cancelled.commands.length, 0);
});

test('graphics fallback replaces the claimed canvas and gates controls until ready', () => {
    const {app, get} = bridge();
    const old = get('binwave');
    const fresh = {hidden:true};
    let replacement;
    old.cloneNode = deep => { assert.equal(deep, false); return fresh; };
    old.replaceWith = canvas => { replacement = canvas; };
    app.ready();
    app.freshCanvas();
    assert.equal(replacement, fresh, 'Retry needs a new canvas, not the WebGPU-bound context');
    assert.equal(fresh.hidden, false);
    assert.equal(get('open').disabled, true);
    assert.equal(get('graphics-error').hidden, true);
    assert.match(get('status').textContent, /WebGL2/);
    app.ready();
    assert.equal(get('open').disabled, false);
});
