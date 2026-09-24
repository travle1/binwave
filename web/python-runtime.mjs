// Browser-local transport for the exact same Python runner used by native.
// The interpreter and core standard library ship with the static web build. This
// adapter reads through DecoderSource's bounded slices and does not upload bytes.
const PYODIDE_BASE = new URL('../pyodide/', import.meta.url).href;

export async function runPythonDecoder(message, source, runnerSource, runtimeBase = PYODIDE_BASE, runtimeIndex = runtimeBase) {
    const {loadPyodide} = await import(new URL('pyodide.mjs', runtimeBase).href);
    const pyodide = await loadPyodide({indexURL: runtimeIndex});
    pyodide.registerJsModule('binwave_browser', {
        read(offset, length) {
            return source.read(String(offset), Number(length));
        },
    });
    pyodide.globals.set('_bw_runner_source', runnerSource);
    pyodide.globals.set('_bw_script', message.script);
    pyodide.globals.set('_bw_filename', message.filename || '<decoder.py>');
    pyodide.globals.set('_bw_size', message.file.size);
    // The runner module's __name__ is not __main__, so its subprocess protocol
    // entrypoint is never invoked. Only run_decoder is called.
    pyodide.runPython(`
import sys, types
_bw_runner = types.ModuleType("binwave_runner")
_bw_runner.__file__ = "<binwave_runner>"
exec(compile(_bw_runner_source, _bw_runner.__file__, "exec"), _bw_runner.__dict__)
sys.modules["binwave_runner"] = _bw_runner
`);
    await pyodide.loadPackagesFromImports(message.script);
    const text = pyodide.runPython(`
import json
from binwave_browser import read as _bw_js_read
def _bw_read(offset, length):
    return _bw_js_read(offset, length).to_bytes()
json.dumps(_bw_runner.run_decoder(_bw_script, _bw_filename, _bw_size, _bw_read),
           ensure_ascii=False, allow_nan=False)
`);
    return JSON.parse(text);
}
