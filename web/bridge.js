// DOM picker buttons retain browser user activation; egui runs on animation frames.
(() => {
    const worker = new Worker(new URL('file-worker.mjs', document.currentScript.src), {type: 'module'});
    const events = [];
    let failed = false, graphicsReady = false;
    let busy = false, dirty = false, opened = false, exportable = false;
    let runtime = null, loadingDecoder = false;
    let decoderSource = '';
    let size = '0', selectedStart = '0', selectedEnd = '0';
    const status = document.getElementById('status');
    const open = document.getElementById('open');
    const save = document.getElementById('save');
    const extract = document.getElementById('extract');
    const cancel = document.getElementById('cancel');
    const input = document.getElementById('file');
    const openDecoder = document.getElementById('open-decoder');
    const decoderInput = document.getElementById('decoder-file');
    function controls() {
        openDecoder.disabled = busy || loadingDecoder || failed || !graphicsReady;
        open.disabled = busy || failed || !graphicsReady;
        save.disabled = busy || failed || !graphicsReady || !opened || !exportable;
        extract.disabled = save.disabled || selectedStart === selectedEnd;
        cancel.disabled = !busy;
    }
    function submit(m) {
        if (busy || failed) return false;
        busy = true;
        controls();
        if (m.type === 'decode') { m.runtime = runtime; status.textContent = 'Decoding…'; }
        worker.postMessage(m);
        return true;
    }
    worker.onmessage = ({data}) => {
        if (data.type === 'ready') { busy = false; controls(); return; }
        if (data.type === 'progress') { status.textContent = `Exported ${data.done} / ${data.total} bytes`; return; }
        if (['opened', 'edited', 'exported'].includes(data.type)) {
            dirty = data.dirty; size = data.size; opened = true;
            exportable = data.exportable && typeof window.showSaveFilePicker === 'function';
            if (data.type === 'opened') selectedStart = selectedEnd = '0';
        }
        if (data.type === 'error') status.textContent = data.message;
        if (data.type === 'decoded') {
            status.textContent = `Decoded ${data.result.consumed} of ${size} bytes`;
            if (!data.result.collections.length) status.textContent += ' · inspect Decoded structure';
        }
        if (data.type === 'exported') status.textContent = 'Export complete';
        if (data.type === 'opened') status.textContent = exportable ? 'Local file opened' : 'Inspection available; safe streaming export needs a supported file-system picker';
        // The UI consumes at 30 Hz and only one worker request can be outstanding.
        // Coalesce unread pages and cap the mailbox even if WASM startup fails.
        if (data.type === 'page') {
            const old = events.findIndex(e => e.type === 'page');
            if (old >= 0) events.splice(old, 1);
        }
        if (events.length >= 16) events.shift();
        events.push(data);
        controls();
    };
    worker.onerror = event => {
        failed = true;
        busy = false;
        const message = `File worker failed: ${event.message || 'reload the page'}`;
        events.push({type: 'error', message});
        status.textContent = message;
        controls();
    };
    open.onclick = async () => {
        if (dirty && !confirm('Discard unsaved byte edits and open another file?')) return;
        if (!window.showOpenFilePicker) { input.click(); return; }
        busy = true; controls();
        try {
            const [handle] = await window.showOpenFilePicker({multiple: false});
            const file = await handle.getFile();
            busy = false;
            submit({type: 'open', file, handle});
        } catch (e) { busy = false; controls(); if (e.name !== 'AbortError') status.textContent = e.message; }
    };
    input.onchange = () => {
        if (input.files[0]) submit({type: 'open', file: input.files[0]});
        input.value = '';
    };
    // A plain file input works on LAN HTTP and mobile Safari, without the
    // secure-context File System Access API. Keep decoder text independently
    // bounded; it must never become the data-file backend's open source.
    openDecoder.onclick = () => decoderInput.click();
    decoderInput.onchange = async () => {
        const file = decoderInput.files[0];
        decoderInput.value = '';
        if (!file) return;
        loadingDecoder = true; controls();
        try {
            if (/\.py$/i.test(file.name)) throw Error('Python decoders require the native app. Choose a YAML decoder.');
            if (file.size > 256 * 1024) throw Error('Decoder source is limited to 256 KiB. Choose a YAML decoder, not the data file.');
            const source = await file.text();
            if (source.includes('\0')) throw Error('This looks like binary data. Choose a YAML decoder.');
            const old = events.findIndex(e => e.type === 'decoder-source');
            if (old >= 0) events.splice(old, 1);
            if (events.length >= 16) events.shift();
            events.push({type:'decoder-source', name:file.name, source});
            status.textContent = `Decoder loaded: ${file.name} · tap Apply`;
        } catch (error) {
            status.textContent = String(error.message || error);
        } finally { loadingDecoder = false; controls(); }
    };
    async function exportFile(selection) {
        const start = selection ? selectedStart : '0';
        const end = selection ? selectedEnd : size;
        busy = true; controls();
        try {
            const handle = await window.showSaveFilePicker({suggestedName: selection ? 'selection.bin' : 'edited-copy.bin'});
            busy = false;
            submit({type: 'export', handle, start, end});
        } catch (e) { busy = false; controls(); if (e.name !== 'AbortError') status.textContent = e.message; }
    }
    save.onclick = () => exportFile(false);
    extract.onclick = () => exportFile(true);
    cancel.onclick = () => worker.postMessage({type: 'cancel'});
    window.addEventListener('beforeunload', event => {
        if (dirty || busy) { event.preventDefault(); event.returnValue = ''; }
    });
    window.addEventListener('TrunkApplicationStarted', () => {
        const js = document.querySelector('link[rel="modulepreload"]');
        const wasm = document.querySelector('link[type="application/wasm"]');
        if (js && wasm) runtime = {js:js.href, wasm:wasm.href};
    });
    // A DOM dialog keeps native clipboard menus and touch paste available even
    // on LAN HTTP, where navigator.clipboard is unavailable.
    function clipboardDialog(text, paste) {
        return new Promise(resolve => {
            const dialog = document.createElement('dialog');
            dialog.style.cssText = 'background:#242424;color:#eee;border:1px solid #666;border-radius:8px;padding:20px;font:16px system-ui;max-width:90vw';
            const label = document.createElement('p');
            label.textContent = paste ? 'Paste here, then Insert into decoder.' : 'Copy this selected text.';
            const area = document.createElement('textarea');
            area.value = text;
            area.setAttribute('aria-label', paste ? 'Text to paste' : 'Text to copy');
            area.style.cssText = 'width: min(75vw, 600px); height: 180px; font: 16px monospace';
            area.readOnly = !paste;
            const apply = document.createElement('button');
            apply.textContent = paste ? 'Insert into decoder' : 'Copy';
            const close = document.createElement('button'); close.textContent = 'Close';
            function finish(value) { dialog.close(); dialog.remove(); resolve(value); }
            apply.onclick = async () => {
                if (paste) { if (area.value.length > 256 * 1024) { label.textContent = 'Paste is limited to 256 KiB.'; return; } finish(area.value); }
                else {
                    area.select();
                    try { await navigator.clipboard.writeText(area.value); finish(null); }
                    catch { if (document.execCommand('copy')) finish(null); else label.textContent = 'Use your browser’s Copy command on the selected text.'; }
                }
            };
            close.onclick = () => finish(null);
            dialog.oncancel = event => { event.preventDefault(); finish(null); };
            dialog.append(label, area, apply, close); document.body.append(dialog);
            dialog.showModal(); area.focus(); if (!paste) area.select();
        });
    }
    window.addEventListener('keydown', event => {
        if (graphicsReady && (event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 's') {
            event.preventDefault();
            window.binwave.saveDecoder(decoderSource);
        }
    });
    window.binwave = {
        decoderDraft(text) { decoderSource = text; },
        pasteText() { return clipboardDialog('', true); },
        copyText(text) {
            if (!text) return;
            if (typeof navigator !== 'undefined' && navigator.clipboard) {
                navigator.clipboard.writeText(text).catch(() => clipboardDialog(text, false));
            } else clipboardDialog(text, false);
        },
        saveDecoder(text) {
            const blob = new Blob([text], {type:'text/yaml;charset=utf-8'});
            const url = URL.createObjectURL(blob);
            const link = document.createElement('a');
            link.href = url; link.download = 'decoder.yaml'; document.body.append(link); link.click(); link.remove();
            setTimeout(() => URL.revokeObjectURL(url), 60000);
            status.textContent = 'Decoder downloaded as decoder.yaml';
        },
        openDemo(bytes, replaceDecoder = false) {
            if (busy || loadingDecoder || failed || !graphicsReady) return false;
            if ((dirty || replaceDecoder) && !confirm('Replace the current data and decoder with the simple demo? Unsaved edits will be discarded.')) return false;
            if (bytes.length !== 2048) throw Error('Invalid bundled demo size');
            const file = new File([bytes], 'simple_wave.bin', {type:'application/octet-stream'});
            return submit({type:'open', file, demo:true});
        },
        poll() {
            const event = events.shift();
            if (!event) return '';
            if (event.bytes) event.bytes = Array.from(new Uint8Array(event.bytes));
            return JSON.stringify(event);
        },
        send(json) {
            const command = JSON.parse(json);
            if (command.type === 'cancel') { worker.postMessage(command); return true; }
            if (command.type === 'decode' && !runtime) return false;
            return submit(command);
        },
        selection(start, end) { selectedStart = start; selectedEnd = end; controls(); },
        error(message) {
            graphicsReady = false;
            status.textContent = 'Graphics unavailable in this browser';
            document.getElementById('graphics-error').hidden = false;
            document.getElementById('graphics-detail').textContent = message;
            document.getElementById('graphics-url').textContent = window.location.href;
            document.getElementById('binwave').hidden = true;
            controls();
        },
        freshCanvas() {
            graphicsReady = false;
            controls();
            const old = document.getElementById('binwave');
            const canvas = old.cloneNode(false);
            canvas.hidden = false;
            old.replaceWith(canvas);
            document.getElementById('graphics-error').hidden = true;
            status.textContent = 'Trying WebGL2 graphics…';
        },
        ready() {
            graphicsReady = true;
            if (!failed) status.textContent = 'Ready · files stay on your computer';
            controls();
        },
    };
    controls();
})();
