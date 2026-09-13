//! Python decoders run as ordinary local Python programs, with access to installed
//! libraries and the user's files. This is not a Python sandbox. The subprocess
//! bridge bounds its own messages and reads, and always runs away from the UI.
//! Generated read plans use the same lazy parser and edited ByteView as YAML.
use crate::byte_store::{ByteView, READ_LIMIT};
use crate::structure::{ParseOutput, Program};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_SCRIPT: usize = 256 * 1024;
const MAX_MESSAGE: usize = 2 * 1024 * 1024;
const MAX_LOG: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const RUNNER: &str = include_str!("../python/runner.py");

pub struct PythonOutput {
    pub decoded: Option<(Program, ParseOutput)>,
    pub stdout: String,
}

/// Execute a Python decoder on a background worker. Cancellation stops the
/// interpreter even when its code or pipe I/O is blocked. Reads see precisely
/// this immutable working-byte revision, including unsaved byte edits.
pub fn run(
    code: &str,
    script_path: Option<&Path>,
    source: &ByteView,
    cancel: &AtomicBool,
) -> Result<PythonOutput> {
    ensure!(code.len() <= MAX_SCRIPT, "Python decoder exceeds 256 KiB");
    ensure!(!cancel.load(Ordering::Relaxed), "Python decoding cancelled");
    source.validate()?;
    let (plan, stdout) = run_python(code, script_path, source, cancel)?;
    ensure!(!cancel.load(Ordering::Relaxed), "Python decoding cancelled");
    source.validate()?;
    let decode = || -> Result<(Program, ParseOutput)> {
        let yaml =
            serde_yaml_ng::to_string(&plan).context("read Python decoder's generated plan")?;
        let program =
            Program::parse(&yaml).context("invalid read plan returned by Python decoder")?;
        let output = program
            .run(source, cancel)
            .context("decode Python read plan")?;
        Ok((program, output))
    };
    let decoded = if plan.is_null() {
        None
    } else {
        Some(decode().map_err(|error| with_output(error, &stdout))?)
    };
    Ok(PythonOutput { decoded, stdout })
}

fn run_python(
    code: &str,
    script_path: Option<&Path>,
    source: &ByteView,
    cancel: &AtomicBool,
) -> Result<(Value, String)> {
    let interpreter = std::env::var_os("PLOTTERMAX_PYTHON").unwrap_or_else(|| {
        if cfg!(windows) {
            "python".into()
        } else {
            "python3".into()
        }
    });
    let mut command = Command::new(&interpreter);
    command.args(["-u", "-c", RUNNER]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A relative filename must be resolved before changing the child's working
    // directory, so traceback filenames and local imports refer to the script.
    let script_path = script_path.map(|path| {
        path.canonicalize().unwrap_or_else(|_| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        })
    });
    if let Some(parent) = script_path.as_deref().and_then(Path::parent) {
        command.current_dir(parent);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn().with_context(|| {
        format!("Start Python interpreter {:?}. Install Python 3 or set PLOTTERMAX_PYTHON to its executable.", interpreter)
    })?;
    let mut child = ChildGuard {
        child,
        reaped: false,
    };
    let input = child.child.stdin.take().expect("piped Python stdin");
    let output = child.child.stdout.take().expect("piped Python stdout");
    let errors = child.child.stderr.take().expect("piped Python stderr");
    let completed = Arc::new(AtomicBool::new(false));
    let exit = Arc::new(Mutex::new(None));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let (sender, receiver) = mpsc::sync_channel(1);
    let output_worker = thread::spawn(move || read_messages(output, sender));
    let error_buffer = stderr.clone();
    let error_worker = thread::spawn(move || drain_stderr(errors, &error_buffer));
    let filename = script_path
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<decoder>".into());
    let result = thread::scope(|scope| {
        let cleanup = CompletionGuard(completed.clone());
        let child_completed = completed.clone();
        let child_exit = exit.clone();
        scope.spawn(move || watch_child(child, cancel, &child_completed, &child_exit));
        let mut input = BufWriter::new(input);
        let result = (|| {
            send_message(
                &mut input,
                &json!({"script": code, "filename": filename, "size": source.len()}),
            )?;
            exchange(&mut input, &receiver, source, cancel, &exit)
        })();
        drop(input);
        drop(cleanup); // Terminate/reap before joining the scoped watchdog.
        result
    });
    drop(receiver); // Release a stdout reader waiting to send a queued message.
                    // Ordinary children close their pipes when reaped. Do not wait indefinitely
                    // for a script-created daemon that escaped the interpreter's process group.
    finish_reader(output_worker);
    finish_reader(error_worker);
    if cancel.load(Ordering::Relaxed) {
        bail!("Python decoding cancelled");
    }
    let stderr = stderr.lock().unwrap_or_else(|error| error.into_inner());
    let stderr = bounded_text(&String::from_utf8_lossy(&stderr));
    result.map_err(|error| {
        if stderr.trim().is_empty() {
            error
        } else {
            error.context(format!("Python stderr:\n{stderr}"))
        }
    })
}

struct CompletionGuard(Arc<AtomicBool>);
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct ChildGuard {
    child: Child,
    reaped: bool,
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            kill_process_tree(&mut self.child);
            let _ = self.child.wait();
        }
    }
}

#[derive(Clone)]
struct ExitInfo {
    description: String,
    at: Instant,
}

fn watch_child(
    mut managed: ChildGuard,
    cancel: &AtomicBool,
    completed: &AtomicBool,
    exit: &Mutex<Option<ExitInfo>>,
) {
    let child = &mut managed.child;
    let description = loop {
        if cancel.load(Ordering::Relaxed) || completed.load(Ordering::Acquire) {
            kill_process_tree(child);
            break child
                .wait()
                .map(|status| status.to_string())
                .unwrap_or_else(|error| error.to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_process_group(child.id());
                break status.to_string();
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(error) => {
                kill_process_tree(child);
                let _ = child.wait();
                break error.to_string();
            }
        }
    };
    managed.reaped = true;
    *exit.lock().unwrap_or_else(|error| error.into_inner()) = Some(ExitInfo {
        description,
        at: Instant::now(),
    });
}

fn kill_process_tree(child: &mut Child) {
    kill_process_group(child.id());
    let _ = child.kill();
}
fn kill_process_group(id: u32) {
    #[cfg(unix)]
    if let Ok(id) = i32::try_from(id) {
        if id > 0 {
            // SAFETY: the child was launched with process_group(0), so its PID
            // names its dedicated process group. No handles or pointers cross
            // this syscall; ESRCH after normal termination is harmless.
            unsafe {
                libc::kill(-id, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = id;
}

fn finish_reader(worker: JoinHandle<()>) {
    let deadline = Instant::now() + Duration::from_millis(200);
    while !worker.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    if worker.is_finished() {
        let _ = worker.join();
    }
}

enum PipeMessage {
    Line(Vec<u8>),
    End,
    Failed(String),
}
fn read_messages(output: impl Read, sender: SyncSender<PipeMessage>) {
    let mut output = BufReader::new(output);
    loop {
        let message = match read_line(&mut output) {
            Ok(Some(line)) => PipeMessage::Line(line),
            Ok(None) => PipeMessage::End,
            Err(error) => PipeMessage::Failed(format!("{error:#}")),
        };
        let ended = !matches!(message, PipeMessage::Line(_));
        if sender.send(message).is_err() || ended {
            return;
        }
    }
}

/// `read_until` alone could allocate an arbitrarily long protocol line.
fn read_line(input: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let chunk = input.fill_buf().context("read Python decoder response")?;
        if chunk.is_empty() {
            ensure!(
                line.is_empty(),
                "Python decoder ended inside a protocol message"
            );
            return Ok(None);
        }
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let length = end.map_or(chunk.len(), |index| index + 1);
        ensure!(
            length <= MAX_MESSAGE.saturating_sub(line.len()),
            "Python decoder protocol message exceeds 2 MiB"
        );
        line.extend_from_slice(&chunk[..length]);
        input.consume(length);
        if end.is_some() {
            return Ok(Some(line));
        }
    }
}

fn drain_stderr(mut errors: impl Read, buffer: &Mutex<Vec<u8>>) {
    let mut chunk = [0_u8; 4096];
    loop {
        match errors.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                let mut buffer = buffer.lock().unwrap_or_else(|error| error.into_inner());
                let keep = count.min(MAX_LOG.saturating_sub(buffer.len()));
                buffer.extend_from_slice(&chunk[..keep]);
            }
        }
    }
}

#[derive(Serialize)]
struct DataResponse<'a> {
    data: &'a [u8],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    offset: u64,
    length: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Done {
    program: Value,
    #[serde(default)]
    stdout: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Failure {
    error: String,
    #[serde(default)]
    stdout: String,
}

fn exchange(
    input: &mut impl Write,
    receiver: &Receiver<PipeMessage>,
    source: &ByteView,
    cancel: &AtomicBool,
    exit: &Mutex<Option<ExitInfo>>,
) -> Result<(Value, String)> {
    loop {
        ensure!(!cancel.load(Ordering::Relaxed), "Python decoding cancelled");
        let line = match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(PipeMessage::Line(line)) => line,
            Ok(PipeMessage::Failed(error)) => bail!("{error}"),
            Ok(PipeMessage::End) | Err(RecvTimeoutError::Disconnected) => {
                bail!("Python decoder exited before returning a read plan")
            }
            Err(RecvTimeoutError::Timeout) => {
                let exit = exit.lock().unwrap_or_else(|error| error.into_inner());
                if let Some(exit) = exit
                    .as_ref()
                    .filter(|exit| exit.at.elapsed() > Duration::from_millis(200))
                {
                    bail!(
                        "Python decoder exited before returning a read plan ({})",
                        exit.description
                    );
                }
                continue;
            }
        };
        let message: Value =
            serde_json::from_slice(&line).context("invalid Python decoder protocol message")?;
        let object = message
            .as_object()
            .context("Python decoder protocol message must be an object")?;
        if let Some(request) = object.get("read") {
            ensure!(object.len() == 1, "invalid Python read request");
            let result = serde_json::from_value::<ReadRequest>(request.clone())
                .context("invalid Python byte-read request")
                .and_then(|request| read_source(source, request));
            match result {
                // Byte arrays contain at most 1 MiB raw bytes (up to 4 MiB on
                // the wire). Stream them instead of allocating a serialized copy.
                Ok(data) => send_message(input, &DataResponse { data: &data })?,
                Err(error) => send_message(input, &json!({"error":format!("{error:#}")}))?,
            }
        } else if let Some(done) = object.get("done") {
            ensure!(object.len() == 1, "invalid Python decoder completion");
            // No decoded data is represented by an explicit null plan. A
            // missing field remains a malformed response, not an empty run.
            ensure!(
                done.as_object()
                    .is_some_and(|object| object.contains_key("program")),
                "invalid Python decoder completion: missing program"
            );
            let done: Done = serde_json::from_value(done.clone())
                .context("invalid Python decoder completion")?;
            return Ok((done.program, bounded_text(&done.stdout)));
        } else if object.contains_key("error") {
            let error: Failure =
                serde_json::from_value(message).context("invalid Python decoder failure")?;
            return Err(with_output(
                anyhow::anyhow!(bounded_text(&error.error)),
                &bounded_text(&error.stdout),
            ));
        } else {
            bail!("unrecognized Python decoder protocol message");
        }
    }
}

fn read_source(source: &ByteView, request: ReadRequest) -> Result<Vec<u8>> {
    let offset = usize::try_from(request.offset)
        .context("Python byte offset exceeds this platform's address range")?;
    let length = usize::try_from(request.length)
        .context("Python read length exceeds this platform's address range")?;
    ensure!(
        length <= READ_LIMIT,
        "Python reads are limited to 1 MiB at a time"
    );
    let end = offset
        .checked_add(length)
        .context("Python byte range overflow")?;
    ensure!(end <= source.len(), "Python byte range is outside the file");
    source.validate()?;
    let data = source.read(offset, length)?;
    ensure!(data.len() == length, "file ended during Python byte read");
    source.validate()?;
    Ok(data)
}

fn send_message(output: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(&mut *output, value).context("write Python decoder request")?;
    output
        .write_all(b"\n")
        .context("write Python decoder request")?;
    output.flush().context("flush Python decoder request")
}

fn bounded_text(text: &str) -> String {
    if text.len() <= MAX_LOG {
        return text.to_owned();
    }
    const NOTE: &str = "\n[output truncated]";
    let mut end = MAX_LOG - NOTE.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{NOTE}", &text[..end])
}
fn with_output(error: anyhow::Error, stdout: &str) -> anyhow::Error {
    if stdout.trim().is_empty() {
        error
    } else {
        error.context(format!("Python output:\n{stdout}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::Original;
    use crate::byte_store::ByteStore;
    use crate::file_source::FileSource;
    use std::io::{Cursor, Seek, SeekFrom};

    fn bytes(data: &[u8]) -> ByteStore {
        ByteStore::from_snapshot(Original::from_bytes(data).unwrap())
    }
    fn execute(code: &str, source: &ByteView) -> PythonOutput {
        match run(code, None, source, &AtomicBool::new(false)) {
            Ok(output) => output,
            Err(error) => panic!("{error:#}"),
        }
    }
    fn fail(code: &str, source: &ByteView) -> String {
        match run(code, None, source, &AtomicBool::new(false)) {
            Ok(_) => panic!("Python decoder unexpectedly succeeded"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn ordinary_python_succeeds_without_registering_decoded_data() {
        let source = bytes(&[7, 0]).view();
        for (code, expected_stdout) in [
            ("test = 5", ""),
            (
                "import math\ndef squared(value): return value * value\nprint(squared(3), math.sqrt(16))",
                "9 4.0\n",
            ),
            ("print(file.read_at(0, 2).hex())", "0700\n"),
        ] {
            let result = execute(code, &source);
            assert!(result.decoded.is_none(), "ordinary code emitted a read plan");
            assert_eq!(result.stdout, expected_stdout);
        }
    }

    #[test]
    fn python_reads_working_edits_and_returns_the_shared_parser_plan() {
        let mut store = bytes(&[2, 10, 0, 20, 0]);
        store.replace(1, &42_u16.to_le_bytes()).unwrap();
        let result = execute(
            r#"from plottermax import Struct
Header = Struct('Header', [('count', 'u8')])
header = file.read_struct('header', Header)
assert file.read_at(1, 2) == bytes([42, 0])
Sample = Struct('Sample', [('value', 'u16')])
samples = file.records('samples', Sample, count=header.count)
print('count:', len(samples))
"#,
            &store.view(),
        );
        assert!(result.stdout.contains("count: 2"));
        let (program, output) = result.decoded.expect("registered structures must decode");
        assert_eq!(output.revision, store.view().revision);
        assert_eq!(output.consumed, 5);
        assert_eq!(
            output.nodes[1].children[0].children[0].numeric(),
            Some(42.0)
        );
        assert_eq!(
            output.nodes[1].children[1].children[0].numeric(),
            Some(20.0)
        );
        assert!(program.structures.contains_key("Sample"));
    }

    #[test]
    fn python_functions_and_imports_work_from_the_decoder_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("local_decoder_helper.py"),
            "from plottermax import Struct\ndef make_type():\n    return Struct('Value', [('number', 'u8')])\n").unwrap();
        let path = dir.path().join("decoder.py");
        let code = "import math\nfrom local_decoder_helper import make_type\ndef decode():\n    return file.read_struct('value', make_type())\nprint(math.sqrt(decode().number))\n";
        std::fs::write(&path, code).unwrap();
        let result = run(
            code,
            Some(&path),
            &bytes(&[81]).view(),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|error| panic!("{error:#}"));
        assert!(result.stdout.contains("9.0"));
        let (_, output) = result.decoded.expect("registered structures must decode");
        assert_eq!(output.nodes[0].children[0].numeric(), Some(81.0));
    }

    #[test]
    fn python_large_collections_stay_lazy_and_keep_64_bit_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large-sparse.raw");
        let length = 5_u64 * 1024 * 1024 * 1024 + 16;
        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len(length).unwrap();
        file.seek(SeekFrom::Start(length - 4)).unwrap();
        file.write_all(&12345_u32.to_le_bytes()).unwrap();
        drop(file);
        let source = ByteStore::from_file(Arc::new(FileSource::open(&path).unwrap())).view();
        let result = execute(
            "from plottermax import Struct\nassert file.unpack('<I', offset=file.size - 4) == (12345,)\nSample = Struct('Sample', [('value', 'u32')])\nsamples = file.records('samples', Sample, count=file.size // 4)\nprint(len(samples))\n",
            &source,
        );
        let (_, output) = result.decoded.expect("registered structures must decode");
        assert_eq!(output.nodes.len(), 1);
        let node = &output.nodes[0];
        let lazy = node.lazy.as_ref().expect("large records must remain lazy");
        assert_eq!(lazy.count, usize::try_from(length / 4).unwrap());
        assert!(node.children.is_empty());
        let tail = lazy
            .page(&source, &AtomicBool::new(false), lazy.count - 1, 1)
            .unwrap();
        assert_eq!(tail[0].range.start, usize::try_from(length - 4).unwrap());
        assert_eq!(tail[0].children[0].numeric(), Some(12345.0));
    }

    /// Opt in to checking the real generated artifact through Python, the pipe
    /// bridge, Program validation, and Rust's lazy sample-page reader.
    #[test]
    #[ignore = "requires generated fixture; set PLOTTERMAX_WAVEFORM_FILE to choose its path"]
    #[cfg(target_pointer_width = "64")]
    fn verify_generated_waveform_python_runtime() {
        use crate::structure::Node;
        use std::collections::BTreeSet;
        use std::f64::consts::TAU;

        fn node_count(nodes: &[Node]) -> usize {
            nodes
                .iter()
                .map(|node| 1 + node_count(&node.children))
                .sum()
        }
        fn value(node: &Node, name: &str) -> f64 {
            node.children
                .iter()
                .find(|node| node.name == name)
                .unwrap()
                .numeric()
                .unwrap()
        }
        fn expected(index: usize) -> [(&'static str, &'static str, usize, f64); 12] {
            let phase = (index % 128) as f64 / 128.0;
            let bits = (index % 16) as u32;
            [
                (
                    "sine",
                    "f64",
                    8,
                    4.0 * (TAU * (index % 64) as f64 / 64.0).sin(),
                ),
                (
                    "triangle",
                    "f64",
                    8,
                    10.0 + 4.0 * (1.0 - 4.0 * ((index % 96) as f64 / 96.0 - 0.5).abs()),
                ),
                (
                    "ramp",
                    "f64",
                    8,
                    20.0 + 8.0 * ((index % 128) as f64 / 127.0 - 0.5),
                ),
                (
                    "chirp",
                    "f64",
                    8,
                    30.0 + 4.0 * (TAU * (phase + 3.0 * phase * phase)).sin(),
                ),
                (
                    "square",
                    "f32",
                    4,
                    (40.0_f32 + if index % 64 < 32 { 3.0 } else { -3.0 }) as f64,
                ),
                (
                    "pulse",
                    "f32",
                    4,
                    (50.0_f32 + if index % 96 < 8 { 6.0 } else { 0.0 }) as f64,
                ),
                (
                    "staircase",
                    "f32",
                    4,
                    (60.0_f32 + ((index % 128) / 16) as f32) as f64,
                ),
                (
                    "damped_sine",
                    "f32",
                    4,
                    (70.0 + 4.0 * (-3.0 * phase).exp() * (TAU * 4.0 * phase).sin()) as f32 as f64,
                ),
                (
                    "zigzag",
                    "i32",
                    4,
                    (80 + ((index % 32) as i32 - 16).abs() / 2) as f64,
                ),
                (
                    "alternating",
                    "i32",
                    4,
                    (100_i32 + if (index / 8) % 2 == 0 { 3 } else { -3 }) as f64,
                ),
                ("counter_ramp", "u32", 4, (110 + bits) as f64),
                ("bit_pattern", "u32", 4, (130 + (bits ^ (bits >> 1))) as f64),
            ]
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = std::env::var_os("PLOTTERMAX_WAVEFORM_FILE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| root.join("samples/data/waveform_stress_40gb.bin"));
        assert!(
            path.is_file(),
            "generate the fixture first: {}",
            path.display()
        );
        let source = ByteStore::from_file(Arc::new(FileSource::open(&path).unwrap())).view();
        assert!(
            source.len() > u32::MAX as usize,
            "use the large generated fixture"
        );
        let script = root.join("samples/decoders/python/waveform.py");
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let result = run(
            include_str!("../samples/decoders/python/waveform.py"),
            Some(&script),
            &source,
            &cancel,
        )
        .unwrap_or_else(|error| panic!("{error:#}"));
        let python_elapsed = started.elapsed();
        assert!(result
            .stdout
            .contains("Discovered 12 fields, 64 bytes per sample"));
        assert!(result.stdout.contains("Registered"));
        let (program, output) = result.decoded.expect("registered structures must decode");
        assert_eq!(output.consumed, source.len());
        // Reuse the returned Rust plan independently of the Python process.
        let output = program.run(&source, &cancel).unwrap();
        assert_eq!(output.consumed, source.len());
        assert_eq!(output.revision, source.revision);
        assert!(
            node_count(&output.nodes) < 160,
            "metadata and lazy nodes only"
        );
        let samples = output
            .nodes
            .iter()
            .find(|node| node.name == "samples")
            .unwrap();
        let lazy = samples
            .lazy
            .as_ref()
            .expect("40 GB samples must remain lazy");
        assert!(samples.children.is_empty());
        assert_eq!(lazy.stride, 64);
        let count = lazy.count;
        let sample_offset = samples.range.start;
        assert_eq!(samples.range.end, sample_offset + count * lazy.stride);
        let header = output
            .nodes
            .iter()
            .find(|node| node.name == "header")
            .unwrap();
        let footer = output
            .nodes
            .iter()
            .find(|node| node.name == "footer")
            .unwrap();
        assert_eq!(value(header, "sample_count") as usize, count);
        assert_eq!(value(footer, "sample_count") as usize, count);
        assert_eq!(value(footer, "sample_offset") as usize, sample_offset);
        assert_eq!(value(footer, "final_index") as usize, count - 1);
        let names = expected(0)
            .map(|(name, _, _, _)| name)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let sample_type = &program.structures["Sample"];
        assert_eq!(
            sample_type
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<BTreeSet<_>>(),
            names
        );
        let after_four_gib = ((1_usize << 32) - sample_offset) / lazy.stride + 1;
        let starts = [0, after_four_gib, count / 2, count - 8];
        assert!(sample_offset + starts[1] * lazy.stride > u32::MAX as usize);
        let mut checked = 0;
        for start in starts {
            let page = lazy.page(&source, &cancel, start, 8).unwrap();
            assert_eq!(page.len(), 8);
            for (position, record) in page.iter().enumerate() {
                let index = start + position;
                assert_eq!(record.path, format!("samples[{index}]"));
                assert_eq!(
                    record.range,
                    sample_offset + index * 64..sample_offset + (index + 1) * 64
                );
                assert_eq!(record.children.len(), 12);
                let mut field_offset = record.range.start;
                for field in &record.children {
                    let (_, kind, width, expected_value) = expected(index)
                        .into_iter()
                        .find(|(name, _, _, _)| *name == field.name)
                        .unwrap();
                    assert_eq!(field.kind, kind);
                    assert_eq!(field.path, format!("samples[{index}].{}", field.name));
                    assert_eq!(field.range, field_offset..field_offset + width);
                    field_offset += width;
                    let actual = field.numeric().unwrap();
                    assert!(
                        (actual - expected_value).abs() <= 1e-9,
                        "sample {index}, {}: expected {expected_value}, found {actual}",
                        field.name
                    );
                }
                assert_eq!(field_offset, record.range.end);
                checked += 1;
            }
        }
        println!("Python runtime: {} bytes; {count} lazy samples; {} metadata nodes; Python + initial parse {:.3}s; verified {checked} records at start, beyond 4 GiB, middle, and end in {:.3}s total", source.len(), node_count(&output.nodes), python_elapsed.as_secs_f64(), started.elapsed().as_secs_f64());
    }

    #[test]
    fn python_errors_include_traceback_and_prior_print_output() {
        let source = bytes(&[0]).view();
        let syntax = fail("def broken(:\n    pass", &source);
        assert!(syntax.contains("SyntaxError"), "{syntax}");
        let runtime = fail(
            "print('before failure')\nraise ValueError('decode failed here')",
            &source,
        );
        for expected in [
            "before failure",
            "Traceback",
            "ValueError",
            "decode failed here",
        ] {
            assert!(runtime.contains(expected), "missing {expected}: {runtime}");
        }
        let bounds = fail("file.read_at(2, 1)", &source);
        assert!(
            bounds.contains("exceeds file size")
                || bounds.contains("outside")
                || bounds.contains("bounds")
                || bounds.contains("EOF"),
            "{bounds}"
        );
    }

    #[test]
    fn python_reads_a_full_chunk_without_buffering_one_json_value_per_byte() {
        let source = bytes(&vec![255; READ_LIMIT]).view();
        let result = execute(
            "from plottermax import Struct\nassert file.read_at(0, 1024 * 1024) == bytes([255]) * (1024 * 1024)\nfile.read_struct('value', Struct('Value', [('number', 'u8')]))\n",
            &source,
        );
        let (_, output) = result.decoded.expect("registered structures must decode");
        assert_eq!(output.nodes[0].children[0].numeric(), Some(255.0));
    }

    fn cancel_running_code(blocking_code: &str) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("decoder.py");
        let marker = dir.path().join("running.pid");
        let code = format!("import os\nwith open('running.pid', 'w') as marker:\n    marker.write(str(os.getpid()))\n{blocking_code}\n");
        let source = bytes(&[0]).view();
        let cancel = Arc::new(AtomicBool::new(false));
        let child_cancel = cancel.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = run(&code, Some(&script), &source, &child_cancel).map(|_| ());
            let _ = tx.send(result);
        });
        let started = Instant::now();
        let python_pid = loop {
            if let Some(pid) = std::fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.parse::<i32>().ok())
            {
                break pid;
            }
            if let Ok(result) = rx.try_recv() {
                panic!("Python stopped before blocking: {result:?}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "Python never started"
            );
            thread::sleep(Duration::from_millis(5));
        };
        let cancelled_at = Instant::now();
        cancel.store(true, Ordering::Relaxed);
        let result = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("Python cancellation blocked");
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        assert!(cancelled_at.elapsed() < Duration::from_secs(3));
        worker.join().unwrap();
        #[cfg(not(unix))]
        let _ = python_pid;
        #[cfg(unix)]
        {
            let pid = python_pid;
            // SAFETY: signal 0 only checks existence; it delivers no signal.
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "Python child was not reaped"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }

    #[test]
    fn cancellation_kills_and_reaps_an_infinite_python_loop() {
        cancel_running_code("while True:\n    pass");
    }

    #[test]
    fn cancellation_interrupts_python_blocked_reading_stdin() {
        cancel_running_code("import sys\nsys.__stdin__.read()");
    }

    #[test]
    fn malformed_and_oversized_python_protocol_is_rejected() {
        let source = bytes(&[1]).view();
        let malformed = fail(
            "import sys\nsys.__stdout__.write('not json\\n')\nsys.__stdout__.flush()",
            &source,
        );
        assert!(malformed.contains("protocol"), "{malformed}");
        let oversized = fail("import sys\nsys.__stdout__.write('x' * (2 * 1024 * 1024 + 1) + '\\n')\nsys.__stdout__.flush()", &source);
        assert!(oversized.contains("exceeds 2 MiB"), "{oversized}");
        let unknown = fail(
            "import sys\nsys.__stdout__.write('{\"unexpected\":true}\\n')\nsys.__stdout__.flush()",
            &source,
        );
        assert!(unknown.contains("unrecognized"), "{unknown}");
    }

    #[test]
    fn no_data_completion_requires_explicit_null_and_rejects_other_empty_plans() {
        let source = bytes(&[1]).view();
        let missing = fail(
            "import json, sys\nsys.__stdout__.write(json.dumps({'done': {'stdout': 'finished'}}) + '\\n')\nsys.__stdout__.flush()",
            &source,
        );
        assert!(missing.contains("missing program"), "{missing}");
        for plan in [json!({}), json!([]), json!(false), json!(1), json!("")] {
            let completion =
                json!({"done": {"program": plan, "stdout": "before invalid plan"}}).to_string();
            let literal = serde_json::to_string(&format!("{completion}\n")).unwrap();
            let error = fail(
                &format!("import sys\nsys.__stdout__.write({literal})\nsys.__stdout__.flush()"),
                &source,
            );
            assert!(error.contains("invalid read plan"), "{error}");
            assert!(error.contains("before invalid plan"), "{error}");
        }
    }

    #[test]
    fn generated_program_is_validated_before_any_decoded_result_is_returned() {
        let source = bytes(&[1]).view();
        let error = fail(
            "import json, sys\nsys.__stdout__.write(json.dumps({'done': {'program': {'actions': [{'read': {'name': 'value', 'type': 'unknown_type'}}]}, 'stdout': 'plan created'}}) + '\\n')\nsys.__stdout__.flush()",
            &source,
        );
        assert!(error.contains("plan created"), "{error}");
        assert!(error.contains("unknown_type"), "{error}");
    }

    #[test]
    fn byte_rpc_rejects_overflow_truncation_and_oversized_reads() {
        let source = bytes(&[1, 2, 3]).view();
        for request in [
            ReadRequest {
                offset: u64::MAX,
                length: 1,
            },
            ReadRequest {
                offset: 2,
                length: 2,
            },
            ReadRequest {
                offset: 0,
                length: READ_LIMIT as u64 + 1,
            },
        ] {
            assert!(read_source(&source, request).is_err());
        }
        for input in [
            r#"{"offset":-1,"length":1}"#,
            r#"{"offset":1.0,"length":1}"#,
            r#"{"offset":0,"length":1,"extra":1}"#,
        ] {
            assert!(serde_json::from_str::<ReadRequest>(input).is_err());
        }
        assert!(read_source(
            &source,
            ReadRequest {
                offset: 3,
                length: 0
            }
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn protocol_lines_and_stderr_are_bounded_before_accumulation() {
        let line = vec![b'x'; MAX_MESSAGE + 1];
        let error = read_line(&mut BufReader::new(Cursor::new(line))).unwrap_err();
        assert!(error.to_string().contains("exceeds 2 MiB"));
        let log = Mutex::new(Vec::new());
        drain_stderr(Cursor::new(vec![b'e'; MAX_LOG * 8]), &log);
        assert_eq!(log.lock().unwrap().len(), MAX_LOG);
        let text = bounded_text(&"é".repeat(MAX_LOG));
        assert!(text.ends_with("[output truncated]"));
        assert!(text.len() <= MAX_LOG);
    }
}
