import {FileBackend, PAGE_SIZE} from './file-backend.mjs';
import * as edits from './byte-pieces.mjs';
import './cache-settings.js';
let cacheBytes = globalThis.binwaveCacheSettings.DEFAULT_BYTES;
let source = null;
let busy = false;
let cancelled = false;
let decoder = null, pendingDecoder = null, rejectDecoder = null;
function resetDecoder() {
    decoder?.terminate(); decoder = null;
    pendingDecoder?.terminate(); pendingDecoder = null;
    if (rejectDecoder) { rejectDecoder(Error('Decoding cancelled')); rejectDecoder = null; }
}
async function decoding(m) {
    await source.validate();
    const candidate = m.type === 'decode' || m.type === 'decode_python';
    const worker = candidate ? new Worker(new URL('./decoder-worker.mjs', import.meta.url), {type:'module'}) : decoder;
    if (!worker) throw Error('Apply the decoder again before requesting results');
    pendingDecoder = worker;
    try {
        const result = await new Promise((resolve, reject) => {
            rejectDecoder = reject;
            worker.onmessage = ({data}) => data.type === 'error' ? reject(Error(data.message)) : resolve(data);
            worker.onerror = e => reject(Error(e.message || 'Decoder worker failed'));
            worker.postMessage(candidate ? {...m, cacheBytes, file:source.file, pieces:source.pieces} : m);
        });
        await source.validate();
        if (candidate) { decoder?.terminate(); decoder = worker; }
        self.postMessage(result);
    } catch (error) {
        if (candidate) worker.terminate();
        throw error;
    } finally {
        rejectDecoder = null; pendingDecoder = null;
    }
}
const metadata = () => ({name: source.file.name, size: source.size.toString(),
    savedSize: source.originalSize.toString(), memory: edits.memory(source.pieces), dirty: source.dirty,
    undo: source.undo.length > 0, redo: source.redo.length > 0, exportable: !!source.handle});
const EDITS = {
    replace: m => source.replace(m.offset, m.value),
    replace_range: m => source.replaceRange(m.offset, m.bytes),
    insert: m => source.insert(m.offset, m.bytes),
    insert_pattern: m => source.insertPattern(m.offset, m.bytes, m.count),
    fill: m => source.fill(m.start, m.end, m.bytes),
    insert_file: m => source.insertFile(m.offset, m.file),
    delete: m => source.remove(m.start, m.end),
};
// Continue on the file that now holds the saved bytes, with fresh history.
async function reopen(handle) {
    resetDecoder();
    source = new FileBackend(await handle.getFile(), handle);
    self.postMessage({type: 'edited', ...metadata(), saved: true, handle});
}
function reporter() {
    let lastUpdate = 0;
    return (done, total) => {
        if (performance.now() - lastUpdate > 100) {
            lastUpdate = performance.now();
            self.postMessage({type: 'progress', done, total});
        }
    };
}
self.onmessage = async ({data: m}) => {
    if (m.type === 'cache_settings') {
        if (!Number.isSafeInteger(m.cacheBytes) || m.cacheBytes < 0) return;
        cacheBytes = m.cacheBytes;
        decoder?.postMessage(m);
        if (pendingDecoder && pendingDecoder !== decoder) pendingDecoder.postMessage(m);
        return;
    }
    if (m.type === 'cancel') {
        cancelled = true;
        if (rejectDecoder) {
            pendingDecoder?.terminate();
            if (decoder === pendingDecoder) decoder = null;
            rejectDecoder(Error('Decoding cancelled')); rejectDecoder = null;
        }
        return;
    }
    if (busy) { self.postMessage({type: 'error', message: 'A file operation is still running'}); return; }
    busy = true;
    cancelled = false;
    try {
        if (m.type === 'open') {
            const next = new FileBackend(m.file, m.handle);
            await next.validate();
            resetDecoder();
            source = next;
            self.postMessage({type: 'opened', ...metadata(), demo:m.demo === true});
        } else {
            if (!source) throw Error('Open a file first');
            if (['decode', 'decode_python', 'table', 'tree_page', 'plot', 'probe', 'pick', 'inspect', 'encode'].includes(m.type)) {
                await decoding(m);
            } else if (m.type === 'read') {
                if (m.length > PAGE_SIZE) throw Error('Preview exceeds 64 KiB');
                await source.validate();
                const bytes = await source.read(m.offset, m.length);
                self.postMessage({type: 'page', offset: m.offset, bytes: bytes.buffer}, [bytes.buffer]);
            } else if (EDITS[m.type] || m.type === 'undo' || m.type === 'redo') {
                if (EDITS[m.type]) await EDITS[m.type](m);
                else { await source.validate(); source.history(m.type === 'redo'); }
                resetDecoder();
                self.postMessage({type: 'edited', ...metadata(), operation: m.type});
            } else if (m.type === 'export') {
                const whole = m.start === '0' && m.end === source.size.toString();
                await source.export(m.handle, m.start, m.end, () => cancelled, reporter());
                if (whole && m.reopen) await reopen(m.handle);
                else self.postMessage({type: 'exported', ...metadata()});
            } else if (m.type === 'save_in_place') {
                await source.replaceSource(() => cancelled, reporter());
                await reopen(source.handle);
            } else throw Error('Unknown file operation');
        }
    } catch (error) {
        self.postMessage({type: 'error', message: String(error.message || error)});
    } finally {
        busy = false;
        self.postMessage({type: 'ready'});
    }
};
