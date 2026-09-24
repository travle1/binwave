import {DecoderSource} from './decoder-source.mjs';
import {runPythonDecoder} from './python-runtime.mjs';
let session;
self.onmessage = async ({data: m}) => {
    try {
        if (m.type === 'decode' || m.type === 'decode_python') {
            const reader = new FileReaderSync();
            const source = new DecoderSource(m.file, m.patches, blob => reader.readAsArrayBuffer(blob));
            self.binwaveDecoder = {read: (start, length) => source.read(start, length)};
            const bindings = await import(m.runtime.js);
            await bindings.default({module_or_path: m.runtime.wasm});
            session = new bindings.DecoderSession(String(m.file.size));
            if (m.type === 'decode_python') {
                const answer = await runPythonDecoder(m, source, bindings.python_runner());
                if (answer.error) throw Error([answer.stdout, answer.error].filter(Boolean).join('\n'));
                const {program, stdout} = answer.done;
                const result = program === null
                    ? {nodes:[], collections:[], consumed:'0', noData:true}
                    : JSON.parse(session.decode_plan_json(JSON.stringify(program)));
                result.stdout = stdout;
                self.postMessage({type:'decoded', result});
            } else {
                self.postMessage({type:'decoded', result:JSON.parse(m.interpretation ? session.decode_interpretation(m.yaml, m.interpretation.name, m.interpretation.start, m.interpretation.end) : session.decode(m.yaml))});
            }
        } else if (m.type === 'pick') {
            const result = m.anchor === undefined
                ? session.pick(m.path, m.index)
                : session.pick_range(m.path, m.anchor, m.index);
            self.postMessage({type:'pick', request:m.request, result:JSON.parse(result)});
        } else if (m.type === 'probe') {
            self.postMessage({type:'probe', request:m.request, result:JSON.parse(session.probe(m.path, m.start, m.end))});
        } else if (m.type === 'inspect') {
            self.postMessage({type:'inspect', request:m.request, result:JSON.parse(session.inspect(m.start, m.end))});
        } else if (m.type === 'encode') {
            self.postMessage({type:'encoded', request:m.request, result:JSON.parse(session.encode(m.start, m.end, m.value))});
        } else if (m.type === 'table') {
            self.postMessage({type:'table', result:JSON.parse(session.table(m.path, m.start))});
        } else if (m.type === 'plot') {
            self.postMessage({type:'plot', request:m.request, reset:m.reset, result:JSON.parse(m.paths ? session.plot_many(JSON.stringify(m.paths), m.start, m.end, m.width) : session.plot(m.path, m.start, m.end, m.width))});
        } else throw Error('Unknown decoding request');
    } catch (error) {
        self.postMessage({type:'error', message:String(error.message || error)});
    }
};
