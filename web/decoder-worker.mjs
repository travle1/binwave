import {DecoderSource} from './decoder-source.mjs';
let session;
self.onmessage = async ({data: m}) => {
    try {
        if (m.type === 'decode') {
            const reader = new FileReaderSync();
            const source = new DecoderSource(m.file, m.patches, blob => reader.readAsArrayBuffer(blob));
            self.binwaveDecoder = {read: (start, length) => source.read(start, length)};
            const bindings = await import(m.runtime.js);
            await bindings.default({module_or_path: m.runtime.wasm});
            session = new bindings.DecoderSession(String(m.file.size));
            self.postMessage({type:'decoded', result:JSON.parse(m.interpretation ? session.decode_interpretation(m.yaml, m.interpretation.name, m.interpretation.start, m.interpretation.end) : session.decode(m.yaml))});
        } else if (m.type === 'pick') {
            self.postMessage({type:'pick', request:m.request, result:JSON.parse(session.pick(m.path, m.index))});
        } else if (m.type === 'probe') {
            self.postMessage({type:'probe', request:m.request, result:JSON.parse(session.probe(m.path, m.start, m.end))});
        } else if (m.type === 'table') {
            self.postMessage({type:'table', result:JSON.parse(session.table(m.path, m.start))});
        } else if (m.type === 'plot') {
            self.postMessage({type:'plot', request:m.request, reset:m.reset, result:JSON.parse(m.paths ? session.plot_many(JSON.stringify(m.paths), m.start, m.end, m.width) : session.plot(m.path, m.start, m.end, m.width))});
        } else throw Error('Unknown decoding request');
    } catch (error) {
        self.postMessage({type:'error', message:String(error.message || error)});
    }
};
