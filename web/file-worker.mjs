import {FileBackend, PAGE_SIZE} from './file-backend.mjs';
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
            worker.postMessage(candidate ? {...m, file:source.file, patches:Array.from(source.patches)} : m);
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
const metadata = () => ({name: source.file.name, size: source.size.toString(), dirty: source.dirty,
    undo: source.undo.length > 0, redo: source.redo.length > 0, exportable: !!source.handle});
self.onmessage = async ({data: m}) => {
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
            if (['decode', 'decode_python', 'table', 'plot', 'probe', 'pick', 'inspect', 'encode'].includes(m.type)) {
                await decoding(m);
            } else if (m.type === 'read') {
                if (m.length > PAGE_SIZE) throw Error('Preview exceeds 64 KiB');
                await source.validate();
                const bytes = await source.read(m.offset, m.length);
                self.postMessage({type: 'page', offset: m.offset, bytes: bytes.buffer}, [bytes.buffer]);
            } else if (['replace', 'replace_range', 'undo', 'redo'].includes(m.type)) {
                if (m.type === 'replace') await source.replace(m.offset, m.value);
                else if (m.type === 'replace_range') await source.replaceRange(m.offset, m.bytes);
                else { await source.validate(); source.history(m.type === 'redo'); }
                resetDecoder();
                self.postMessage({type: 'edited', ...metadata()});
            } else if (m.type === 'export') {
                let lastUpdate = 0;
                await source.export(m.handle, m.start, m.end, () => cancelled, (done, total) => {
                    if (performance.now() - lastUpdate > 100) {
                        lastUpdate = performance.now();
                        self.postMessage({type: 'progress', done, total});
                    }
                });
                self.postMessage({type: 'exported', ...metadata()});
            } else throw Error('Unknown file operation');
        }
    } catch (error) {
        self.postMessage({type: 'error', message: String(error.message || error)});
    } finally {
        busy = false;
        self.postMessage({type: 'ready'});
    }
};
