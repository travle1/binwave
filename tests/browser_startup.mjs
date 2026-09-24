import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {File, Blob} from 'node:buffer';
import {readFileSync} from 'node:fs';

function bridge(allowReplace = true) {
    const commands = [];
    let worker;
    const elements = new Map();
    const created = [];
    let savedBlob;
    class BrowserURL extends URL {
        static createObjectURL(blob) { savedBlob = blob; return 'blob:local'; }
        static revokeObjectURL() {}
    }
    const document = {
        body: {append() {}},
        createElement(tag) {
            const el = {tag, children: [], style: {}, value:'', append(...children) { this.children.push(...children); },
                setAttribute() {}, click() { this.clicked = true; }, remove() {}, close() {}, showModal() {}, focus() {}, select() {}};
            created.push(el); return el;
        },
        currentScript: {src: 'http://localhost:8080/web/bridge.js'},
        getElementById(id) {
            if (!elements.has(id)) elements.set(id, {disabled: false, hidden: true, textContent: ''});
            return elements.get(id);
        },
    };
    const menuState = {};
    const listeners = {};
    const window = {addEventListener(name, callback) { listeners[name] = callback; }, location: {href: 'http://localhost:8080/'}};
    window.binwaveMenus={refresh(){},render(model,dispatch,allow){Object.assign(menuState,{model,dispatch,allow});}};
    class Worker { constructor() { worker = this; } postMessage(message) { commands.push(message); } }
    vm.runInNewContext(readFileSync(new URL('../web/bridge.js', import.meta.url), 'utf8'), {
        document, window, Worker, URL:BrowserURL, File, Blob, setTimeout() {}, confirm: () => allowReplace,
    });
    return {menuState,listeners, created, savedBlob: () => savedBlob, receive: data => worker.onmessage({data}), commands, app: window.binwave, get: id => document.getElementById(id)};
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

test('oversized decoder files are rejected before reading', async () => {
    const {app, get} = bridge();
    app.ready();
    get('decoder-file').files = [{name:'data.bin',size:40*1024**3,text:()=>{throw Error('Must not read');}}];
    await get('decoder-file').onchange();
    assert.equal(app.poll(), '');
    assert.match(get('status').textContent, /256 KiB/);
    assert.equal(get('open-decoder').disabled, false);
});

test('Python decoder source opens through the same picker', async () => {
    const {app, get} = bridge();
    app.ready();
    get('decoder-file').files = [{name:'decoder.py',size:13,text:async()=> 'print("hi")\\n'}];
    await get('decoder-file').onchange();
    assert.deepEqual(JSON.parse(app.poll()), {type:'decoder-source',name:'decoder.py',source:'print("hi")\\n'});
    assert.match(get('status').textContent, /decoder.py/);
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

test('successful partial decode replaces a previous range error', () => {
    const {app, get, receive} = bridge();
    app.ready();
    receive({type:'opened', name:'unknown.bin', size:'1024', dirty:false, exportable:false});
    receive({type:'error', message:'read samples exceeds enclosing region'});
    assert.match(get('status').textContent, /exceeds/);
    receive({type:'decoded', result:{consumed:'4', collections:[], nodes:[]}});
    assert.equal(get('status').textContent, 'Decoded 4 of 1024 bytes · inspect Decoded structure');
});

test('decoder save downloads the exact YAML without a filesystem picker', async () => {
    const {app, created, savedBlob} = bridge();
    const yaml = '# notes\nendian: little\nactions: []\n';
    app.saveDecoder(yaml);
    assert.equal(await savedBlob().text(), yaml);
    const link = created.find(e => e.tag === 'a');
    assert.equal(link.download, 'decoder.yaml');
    assert.equal(link.clicked, true);
});
test('Python decoder save uses a .py file and preserves source', async () => {
    const {app, created, savedBlob} = bridge();
    const source = 'print("hello")\\n';
    app.saveDecoder(source, 'python');
    assert.equal(await savedBlob().text(), source);
    assert.equal(created.find(e => e.tag === 'a').download, 'decoder.py');
});

test('paste dialog supports native browser paste and cancellation', async () => {
    const {app, created} = bridge();
    const pending = app.pasteText();
    created.find(e => e.tag === 'textarea').value = 'name: café';
    await created.find(e => e.textContent === 'Insert into decoder').onclick();
    assert.equal(await pending, 'name: café');
    const cancelled = app.pasteText();
    created.filter(e => e.textContent === 'Close').at(-1).onclick();
    assert.equal(await cancelled, null);
});

test('save shortcut downloads the current decoder and suppresses browser Save Page', async () => {
    const {app, listeners, savedBlob} = bridge();
    app.ready(); app.decoderDraft('actions: []\n');
    let prevented = false;
    listeners.keydown({ctrlKey:true, key:'s', preventDefault(){prevented=true;}});
    assert.equal(prevented, true);
    assert.equal(await savedBlob().text(), 'actions: []\n');
});

test('clearing the decoder discards stale results and errors without hiding file errors', () => {
    const {app, receive, get} = bridge();
    app.ready();
    app.send(JSON.stringify({type:'plot'}));
    receive({type:'decoded', result:{consumed:'4',collections:[]}});
    receive({type:'error',message:'old decoder error'});
    app.clearDecoder();
    assert.equal(app.poll(), '');
    receive({type:'decoded',result:{consumed:'4',collections:[]}});
    receive({type:'error',message:'late decoder error'});
    assert.equal(app.poll(), '');
    assert.match(get('status').textContent,/No fields yet/);
    receive({type:'ready'});
    app.send(JSON.stringify({type:'read',offset:'0',length:4}));
    receive({type:'error',message:'File unavailable'});
    assert.match(get('status').textContent,/File unavailable/);
});


test('shared menu file pickers stay synchronous and worker busy state disables mutations',()=>{
    const h=bridge();h.app.ready();
    h.app.menus('[]','example.bin · 16 bytes','Ready');
    let clicked=false;h.get('file').click=()=>{clicked=true;};
    h.menuState.dispatch('open_file',0);
    assert.equal(clicked,true,'Picker must open on the original menu click stack');
    assert.equal(h.get('file-context').textContent,'example.bin · 16 bytes');
    h.app.send(JSON.stringify({type:'page',offset:'0'}));
    assert.equal(h.menuState.allow('open_file',true),false);
    assert.equal(h.menuState.allow('simple_demo',true),false);
    assert.equal(h.menuState.allow('hex',true),true);
});
test('shared menu application commands enter the bounded UI mailbox',()=>{
    const h=bridge();h.app.ready();h.app.menus('[]','','Ready');
    h.menuState.dispatch('example',2);
    assert.deepEqual(JSON.parse(h.app.poll()),{type:'menu',command:'example',argument:2});
});

test('advanced demo opens bundled data with its identity and replacement guard', () => {
    const {app, commands} = bridge();
    app.ready();
    assert.equal(app.openDemo(new Uint8Array(8080), false, 'advanced_wave.bin'), true);
    assert.equal(commands[0].file.name, 'advanced_wave.bin');
    assert.equal(commands[0].demo, true);
    const cancelled = bridge(false);
    cancelled.app.ready();
    assert.equal(cancelled.app.openDemo(new Uint8Array(8080), true, 'advanced_wave.bin'), false);
    assert.equal(cancelled.commands.length, 0);
});
