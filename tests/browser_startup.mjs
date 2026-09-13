import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';

function bridge() {
    const elements = new Map();
    const document = {
        currentScript: {src: 'http://localhost:8080/web/bridge.js'},
        getElementById(id) {
            if (!elements.has(id)) elements.set(id, {disabled: false, hidden: true, textContent: ''});
            return elements.get(id);
        },
    };
    const window = {addEventListener() {}, location: {href: 'http://localhost:8080/'}};
    class Worker { postMessage() {} }
    vm.runInNewContext(readFileSync(new URL('../web/bridge.js', import.meta.url), 'utf8'), {
        document, window, Worker, URL,
    });
    return {app: window.binwave, get: id => document.getElementById(id)};
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
