import {FileBackend, PAGE_SIZE} from './file-backend.mjs';
let source = null;
let busy = false;
let cancelled = false;
const metadata = () => ({name: source.file.name, size: source.size.toString(), dirty: source.dirty,
    undo: source.undo.length > 0, redo: source.redo.length > 0, exportable: !!source.handle});
self.onmessage = async ({data: m}) => {
    if (m.type === 'cancel') { cancelled = true; return; }
    if (busy) { self.postMessage({type: 'error', message: 'A file operation is still running'}); return; }
    busy = true;
    cancelled = false;
    try {
        if (m.type === 'open') {
            const next = new FileBackend(m.file, m.handle);
            await next.validate();
            source = next;
            self.postMessage({type: 'opened', ...metadata()});
        } else {
            if (!source) throw Error('Open a file first');
            if (m.type === 'read') {
                if (m.length > PAGE_SIZE) throw Error('Preview exceeds 64 KiB');
                await source.validate();
                const bytes = await source.read(m.offset, m.length);
                self.postMessage({type: 'page', offset: m.offset, bytes: bytes.buffer}, [bytes.buffer]);
            } else if (m.type === 'replace' || m.type === 'undo' || m.type === 'redo') {
                if (m.type === 'replace') await source.replace(m.offset, m.value);
                else { await source.validate(); source.history(m.type === 'redo'); }
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
