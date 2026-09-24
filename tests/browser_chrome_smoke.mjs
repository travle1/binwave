// Optional local smoke: serve dist/web, launch Chrome with remote debugging on
// port 9223, then run this file. It exercises the actual UI and both workers.
import assert from 'node:assert/strict';

const endpoint = process.env.BINWAVE_CDP_URL || 'http://127.0.0.1:9223';
const targets = await fetch(endpoint + '/json').then(response => response.json());
const target = targets.find(item => item.type === 'page' && item.url.includes('127.0.0.1:8765'));
assert.ok(target, 'Open the local BinWave build in Chrome before running this smoke test');
const socket = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, {once:true});
    socket.addEventListener('error', reject, {once:true});
});
let sequence = 0;
const waiting = new Map();
socket.addEventListener('message', event => {
    const message = JSON.parse(event.data);
    if (!message.id) return;
    const next = waiting.get(message.id);
    if (next) { waiting.delete(message.id); next(message); }
});
function send(method, params = {}) {
    return new Promise((resolve, reject) => {
        const id = ++sequence;
        const timeout = setTimeout(() => { waiting.delete(id); reject(Error(method + ' timed out')); }, 30000);
        waiting.set(id, message => {
            clearTimeout(timeout);
            if (message.error) reject(Error(message.error.message));
            else resolve(message.result);
        });
        socket.send(JSON.stringify({id, method, params}));
    });
}
async function evaluate(expression) {
    const result = await send('Runtime.evaluate', {expression, returnByValue:true, awaitPromise:true});
    if (result.exceptionDetails) throw Error(result.exceptionDetails.text);
    return result.result.value;
}
async function until(expression, timeoutMs = 45000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
        const value = await evaluate(expression);
        if (value) return value;
        await new Promise(resolve => setTimeout(resolve, 150));
    }
    throw Error('Timed out waiting for: ' + expression + '; status='
        + await evaluate("document.getElementById('status').textContent"));
}
try {
    await send('Runtime.enable');
    await until("document.querySelector('#app-menus button') !== null");
    await evaluate("window.confirm = () => true");
    await evaluate("([...document.querySelectorAll('#app-menus button')].find(button => button.textContent === 'Open simple demo')).click()");
    await until("document.getElementById('status').textContent.includes('Decoded 2048')");
    const script = 'from binwave import Struct\n'
        + "file.records('samples', Struct('Sample', [('value', 'f32')]))\n";
    const loaded = await evaluate(`(async () => {
        const input = document.getElementById('decoder-file');
        Object.defineProperty(input, 'files', {configurable:true,
            value:[new File([${JSON.stringify(script)}], 'decoder.py', {type:'text/plain'})]});
        await input.onchange();
        return true;
    })()`);
    assert.equal(loaded, true);
    await until("document.getElementById('status').textContent.includes('Loaded decoder.py')");
    await evaluate("([...document.querySelectorAll('#app-menus button')].find(button => button.textContent === '▶ Run (F5)')).click()");
    await until("document.getElementById('status').textContent.includes('Decoded 2048')", 60000);
    console.log('Chrome browser Python Run decoded 512 samples through the bundled runtime.');
} finally {
    socket.close();
}
