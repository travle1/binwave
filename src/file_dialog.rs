//! Native file dialogs that never wait on egui's event loop.

#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

/// Each file category keeps its own last successful open or save directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Data,
    Yaml,
    Python,
    CsvConfig,
    Exports,
}
impl FileKind {
    fn sample_directory(self) -> Option<&'static str> {
        match self {
            Self::Data => Some("samples/data"),
            Self::Yaml => Some("samples/decoders/yaml"),
            Self::Python => Some("samples/decoders/python"),
            Self::CsvConfig => Some("samples/configs"),
            Self::Exports => None,
        }
    }
}

const FILE_LOCATIONS_KEY: &str = "binwave.file_locations.v1";
const MAX_RECENT_PAIRS: usize = 8;
const MAX_LIBRARY_DECODERS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecentPair {
    pub data: PathBuf,
    pub decoder: Option<PathBuf>,
    #[serde(default)]
    pub opened_at_unix_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DecoderRun {
    pub decoder: PathBuf,
    pub data: PathBuf,
    pub completed_at_unix_secs: u64,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct SavedLocations {
    data: Option<PathBuf>,
    yaml: Option<PathBuf>,
    python: Option<PathBuf>,
    csv_config: Option<PathBuf>,
    exports: Option<PathBuf>,
    recent_pairs: Vec<RecentPair>,
    decoder_library: Vec<PathBuf>,
    decoder_runs: Vec<DecoderRun>,
}
impl SavedLocations {
    fn directory(&self, kind: FileKind) -> &Option<PathBuf> {
        match kind {
            FileKind::Data => &self.data,
            FileKind::Yaml => &self.yaml,
            FileKind::Python => &self.python,
            FileKind::CsvConfig => &self.csv_config,
            FileKind::Exports => &self.exports,
        }
    }
    fn directory_mut(&mut self, kind: FileKind) -> &mut Option<PathBuf> {
        match kind {
            FileKind::Data => &mut self.data,
            FileKind::Yaml => &mut self.yaml,
            FileKind::Python => &mut self.python,
            FileKind::CsvConfig => &mut self.csv_config,
            FileKind::Exports => &mut self.exports,
        }
    }
}

/// Shared across the app and its panes; only the app owns persistence.
#[derive(Clone, Default)]
pub struct FileLocations {
    state: Arc<Mutex<SavedLocations>>,
}
impl FileLocations {
    pub fn decoder_paths(&self) -> Vec<PathBuf> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut paths = state.decoder_library.clone();
        for path in state.recent_pairs.iter().filter_map(|pair| pair.decoder.as_ref()) {
            if !paths.contains(path) { paths.push(path.clone()); }
        }
        paths
    }

    pub fn forget_decoder(&self, path: &Path) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.decoder_library.retain(|saved| saved != path);
        state.decoder_runs.retain(|run| run.decoder != path);
        for pair in &mut state.recent_pairs {
            if pair.decoder.as_deref() == Some(path) { pair.decoder = None; }
        }
    }

    pub fn rename_decoder(&self, old: &Path, new: &Path) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        for saved in &mut state.decoder_library {
            if saved == old { *saved = new.to_path_buf(); }
        }
        for pair in &mut state.recent_pairs {
            if pair.decoder.as_deref() == Some(old) { pair.decoder = Some(new.to_path_buf()); }
        }
        for run in &mut state.decoder_runs {
            if run.decoder == old { run.decoder = new.to_path_buf(); }
        }
    }

    pub fn remember_decoder_run(&self, decoder: &Path, data: &Path) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.decoder_runs.retain(|run| run.decoder != decoder);
        state.decoder_runs.insert(0, DecoderRun {
            decoder: decoder.to_path_buf(),
            data: data.to_path_buf(),
            completed_at_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        });
        state.decoder_runs.truncate(MAX_LIBRARY_DECODERS);
    }

    pub fn decoder_run(&self, decoder: &Path) -> Option<DecoderRun> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
            .decoder_runs.iter().find(|run| run.decoder == decoder).cloned()
    }

    pub fn recent_pairs(&self) -> Vec<RecentPair> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .recent_pairs
            .clone()
    }

    /// Keep one current decoder per data file, without storing file contents.
    pub fn remember_pair(&self, data: &Path, decoder: Option<&Path>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let pair = RecentPair {
            data: data.to_path_buf(),
            decoder: decoder.map(Path::to_path_buf),
            opened_at_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        };
        state.recent_pairs.retain(|old| old.data != pair.data);
        if let Some(path) = &pair.decoder {
            state.decoder_library.retain(|saved| saved != path);
            state.decoder_library.insert(0, path.clone());
            state.decoder_library.truncate(MAX_LIBRARY_DECODERS);
        }
        state.recent_pairs.insert(0, pair);
        state.recent_pairs.truncate(MAX_RECENT_PAIRS);
    }
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let state = storage
            .and_then(|storage| storage.get_string(FILE_LOCATIONS_KEY))
            .filter(|value| value.len() <= 128 * 1024)
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        match serde_json::to_string(&state) {
            Ok(value) => storage.set_string(FILE_LOCATIONS_KEY, value),
            Err(error) => log::warn!("Could not store file dialog directories: {error}"),
        }
    }

    pub fn directory(&self, kind: FileKind) -> Option<PathBuf> {
        let mut roots = Vec::with_capacity(2);
        if let Ok(cwd) = std::env::current_dir() {
            roots.push(cwd);
        }
        // Portable packages keep samples beside the executable, regardless of cwd.
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                roots.push(parent.to_path_buf());
            }
        }
        roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        self.directory_from(kind, &roots)
    }

    pub fn remembered_directory(&self, kind: FileKind) -> Option<PathBuf> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .directory(kind)
            .clone()
            .filter(|path| path.is_dir())
    }

    fn directory_from(&self, kind: FileKind, roots: &[PathBuf]) -> Option<PathBuf> {
        if let Some(path) = self.remembered_directory(kind) {
            return Some(path);
        }
        let relative = kind.sample_directory()?;
        roots
            .iter()
            .map(|root| root.join(relative))
            .find(|path| path.is_dir())
    }

    /// Record a completed file operation, not a picker opening or cancellation.
    pub fn remember_file(&self, kind: FileKind, path: &Path) {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(path)
        } else {
            return;
        };
        let Some(parent) = absolute.parent().filter(|parent| parent.is_dir()) else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        *state.directory_mut(kind) = Some(parent.to_path_buf());
        if matches!(kind, FileKind::Yaml | FileKind::Python | FileKind::CsvConfig) {
            state.decoder_library.retain(|saved| saved != &absolute);
            state.decoder_library.insert(0, absolute);
            state.decoder_library.truncate(MAX_LIBRARY_DECODERS);
        }
    }

    pub fn configure(&self, kind: FileKind, dialog: rfd::AsyncFileDialog) -> rfd::AsyncFileDialog {
        // Set an explicit fallback so a native portal's global last-directory
        // setting cannot send an unrelated category back to the data folder.
        match self
            .directory(kind)
            .or_else(|| std::env::current_dir().ok())
        {
            Some(directory) => dialog.set_directory(directory),
            None => dialog,
        }
    }
}

/// A dialog and the operation it was opened for. Keeping the payload here lets
/// callers freeze a save's source, byte range, or text while the picker is open.
///
/// Call `poll` once per app frame, including when its owning pane is hidden.
/// Native futures are created on the UI thread, as required by some platforms,
/// but polled on one worker: even rfd's asynchronous Linux picker can wait while
/// exporting a parent window's Wayland handle.
pub struct NativeFileDialog<T> {
    pending: Option<Pending<T>>,
}

struct Pending<T> {
    result: mpsc::Receiver<Option<PathBuf>>,
    payload: T,
    _worker: DialogWorker,
}

struct DialogWorker {
    cancelled: Arc<AtomicBool>,
    thread: thread::Thread,
}

impl Drop for DialogWorker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        // Wake an idle worker so it can drop its future. Do not join: a native
        // backend may still be finishing a blocking platform call.
        self.thread.unpark();
    }
}

impl<T> Default for NativeFileDialog<T> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<T> NativeFileDialog<T> {
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Start an open dialog. Returns false if another request is still pending.
    /// Construct and start the builder on the UI thread for native platforms
    /// whose dialog setup needs that thread.
    pub fn begin_open(
        &mut self,
        dialog: rfd::AsyncFileDialog,
        payload: T,
        ctx: &egui::Context,
    ) -> bool {
        if self.pending() {
            return false;
        }
        let future = dialog.pick_file();
        self.begin(
            async move { future.await.map(|file| file.path().to_path_buf()) },
            payload,
            ctx,
        )
    }

    /// Start a save dialog without waiting for the user to choose a destination.
    pub fn begin_save(
        &mut self,
        dialog: rfd::AsyncFileDialog,
        payload: T,
        ctx: &egui::Context,
    ) -> bool {
        if self.pending() {
            return false;
        }
        let future = dialog.save_file();
        self.begin(
            async move { future.await.map(|file| file.path().to_path_buf()) },
            payload,
            ctx,
        )
    }

    fn begin(
        &mut self,
        future: impl Future<Output = Option<PathBuf>> + Send + 'static,
        payload: T,
        ctx: &egui::Context,
    ) -> bool {
        if self.pending() {
            return false;
        }
        let (sender, result) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let repaint = ctx.clone();
        let viewport = ctx.viewport_id();
        let worker = thread::Builder::new()
            .name("file-dialog".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_dialog(future, &worker_cancelled)
                }));
                let path = match result {
                    Ok(Some(path)) => path,
                    Ok(None) => return, // The owning app state was dropped.
                    Err(_) => {
                        log::error!("Native file dialog failed unexpectedly");
                        None
                    }
                };
                // The bounded channel has exactly one producer and one result.
                let _ = sender.send(path);
                repaint.request_repaint_of(viewport);
            });
        match worker {
            Ok(worker) => {
                self.pending = Some(Pending {
                    result,
                    payload,
                    _worker: DialogWorker {
                        cancelled,
                        thread: worker.thread().clone(),
                    },
                });
                // Dropping a JoinHandle detaches it; never join on the UI thread.
                drop(worker);
                ctx.request_repaint();
                true
            }
            Err(error) => {
                log::error!("Could not start native file dialog: {error}");
                false
            }
        }
    }

    /// Return a completed operation exactly once. `None` as the selected path
    /// means the native picker was dismissed; no file operation should follow.
    /// This method only checks a channel and never polls platform code.
    pub fn poll(&mut self, _ctx: &egui::Context) -> Option<(T, Option<PathBuf>)> {
        let pending = self.pending.as_ref()?;
        let path = match pending.result.try_recv() {
            Ok(path) => path,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                log::error!("Native file dialog stopped without returning a result");
                None
            }
        };
        let pending = self.pending.take().expect("dialog was just polled");
        Some((pending.payload, path))
    }

    #[cfg(test)]
    pub(crate) fn begin_test(
        &mut self,
        future: impl Future<Output = Option<PathBuf>> + Send + 'static,
        payload: T,
        ctx: &egui::Context,
    ) -> bool {
        self.begin(future, payload, ctx)
    }
}

struct WorkerWake(thread::Thread);

impl Wake for WorkerWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// A tiny executor for one native dialog. Parking happens only on this worker;
/// an unpark token also covers a wake racing with the transition into `park`.
/// The outer option distinguishes dropping the owner from cancelling a picker.
fn run_dialog(
    future: impl Future<Output = Option<PathBuf>>,
    cancelled: &AtomicBool,
) -> Option<Option<PathBuf>> {
    let waker = Waker::from(Arc::new(WorkerWake(thread::current())));
    let mut task = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        match future.as_mut().poll(&mut task) {
            Poll::Ready(path) => return Some(path),
            Poll::Pending => thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    #[test]
    fn recent_pairs_replace_decoder_per_file_and_bound_history() {
        let locations = FileLocations::default();
        let first = PathBuf::from("/data/first.bin");
        locations.remember_pair(&first, None);
        locations.remember_pair(&first, Some(Path::new("/decoders/first.yaml")));
        assert_eq!(locations.recent_pairs().len(), 1);
        assert_eq!(locations.recent_pairs()[0].decoder.as_deref(), Some(Path::new("/decoders/first.yaml")));
        for index in 0..MAX_RECENT_PAIRS + 2 {
            locations.remember_pair(&PathBuf::from(format!("/data/{index}.bin")), None);
        }
        assert_eq!(locations.recent_pairs().len(), MAX_RECENT_PAIRS);
        assert_eq!(locations.recent_pairs()[0].data, PathBuf::from("/data/9.bin"));
    }

    #[test]
    fn previous_recent_file_settings_without_open_time_still_load() {
        let pair: RecentPair = serde_json::from_str(r#"{"data":"/data/example.bin","decoder":null}"#).unwrap();
        assert_eq!(pair.opened_at_unix_secs, 0);
    }

    #[test]
    fn decoder_library_keeps_independent_files_and_forgetting_clears_recent_links() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("one.yaml");
        let python = dir.path().join("two.py");
        std::fs::write(&yaml, "actions: []").unwrap();
        std::fs::write(&python, "print('ready')").unwrap();
        let locations = FileLocations::default();
        locations.remember_file(FileKind::Yaml, &yaml);
        locations.remember_file(FileKind::Python, &python);
        locations.remember_pair(&dir.path().join("data.bin"), Some(&yaml));
        locations.remember_decoder_run(&yaml, &dir.path().join("data.bin"));
        assert_eq!(locations.decoder_paths(), vec![yaml.clone(), python.clone()]);
        let mut storage = LocationsStorage::default();
        locations.save(&mut storage);
        let restored = FileLocations::load(Some(&storage));
        assert_eq!(restored.decoder_paths(), locations.decoder_paths());
        assert_eq!(restored.decoder_run(&yaml).unwrap().data, dir.path().join("data.bin"));
        let renamed = dir.path().join("renamed.yaml");
        restored.rename_decoder(&yaml, &renamed);
        assert_eq!(restored.decoder_run(&renamed).unwrap().decoder, renamed);
        assert_eq!(restored.recent_pairs()[0].decoder.as_deref(), Some(renamed.as_path()));
        restored.forget_decoder(&renamed);
        assert_eq!(restored.decoder_paths(), vec![python]);
        assert!(restored.decoder_run(&renamed).is_none());
        assert_eq!(restored.recent_pairs()[0].decoder, None);
    }

    #[derive(Default)]
    struct LocationsStorage(std::collections::BTreeMap<String, String>);
    impl eframe::Storage for LocationsStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.into(), value);
        }
        fn remove_string(&mut self, key: &str) { self.0.remove(key); }
            fn flush(&mut self) {}
    }

    #[test]
    fn file_locations_are_shared_separate_and_restored_from_app_storage() {
        let temporary = tempfile::tempdir().unwrap();
        let locations = FileLocations::load(None);
        let clone = locations.clone();
        let categories = [
            (FileKind::Data, "data"),
            (FileKind::Yaml, "yaml"),
            (FileKind::Python, "python"),
            (FileKind::CsvConfig, "configs"),
            (FileKind::Exports, "exports"),
        ];
        for (kind, name) in categories {
            let directory = temporary.path().join(name);
            std::fs::create_dir(&directory).unwrap();
            clone.remember_file(kind, &directory.join("chosen-file"));
            assert_eq!(locations.directory(kind), Some(directory));
        }
        let mut storage = LocationsStorage::default();
        locations.remember_pair(&temporary.path().join("data/sample.bin"), Some(&temporary.path().join("yaml/layout.yaml")));
        locations.save(&mut storage);
        let restored = FileLocations::load(Some(&storage));
        for (kind, name) in categories {
            assert_eq!(restored.directory(kind), Some(temporary.path().join(name)));
        }
        assert_eq!(restored.recent_pairs(), locations.recent_pairs());
        assert_eq!(storage.0.len(), 1, "One separate versioned settings entry");
    }

    #[test]
    fn organized_sample_fallbacks_ignore_missing_locations_and_keep_exports_separate() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let remembered = temporary.path().join("remembered");
        std::fs::create_dir(&remembered).unwrap();
        for (kind, suffix) in [
            (FileKind::Data, "samples/data"),
            (FileKind::Yaml, "samples/decoders/yaml"),
            (FileKind::Python, "samples/decoders/python"),
            (FileKind::CsvConfig, "samples/configs"),
        ] {
            std::fs::create_dir_all(project.join(suffix)).unwrap();
            let locations = FileLocations::default();
            assert_eq!(
                locations.directory_from(kind, &[project.clone()]),
                Some(project.join(suffix))
            );
            locations.remember_file(kind, &remembered.join("chosen-file"));
            assert_eq!(
                locations.directory_from(kind, &[project.clone()]),
                Some(remembered.clone())
            );
        }
        let locations = FileLocations::default();
        locations.remember_file(FileKind::Yaml, &remembered.join("decoder.yaml"));
        std::fs::remove_dir(&remembered).unwrap();
        assert_eq!(
            locations.directory_from(FileKind::Yaml, &[project.clone()]),
            Some(project.join("samples/decoders/yaml"))
        );
        assert!(locations
            .directory_from(FileKind::Exports, &[project.clone()])
            .is_none());
        locations.remember_file(FileKind::Data, &project.join("samples/data/source.raw"));
        assert!(locations
            .directory_from(FileKind::Exports, &[project])
            .is_none());
    }

    #[test]
    fn invalid_location_storage_and_invalid_parent_leave_defaults_usable() {
        let mut storage = LocationsStorage::default();
        storage
            .0
            .insert(FILE_LOCATIONS_KEY.into(), "not valid json".into());
        let locations = FileLocations::load(Some(&storage));
        assert!(locations.remembered_directory(FileKind::Data).is_none());
        let temporary = tempfile::tempdir().unwrap();
        locations.remember_file(FileKind::Data, &temporary.path().join("source.raw"));
        locations.remember_file(FileKind::Data, &temporary.path().join("missing/other.raw"));
        assert_eq!(
            locations.remembered_directory(FileKind::Data).as_deref(),
            Some(temporary.path())
        );
    }

    #[derive(Default)]
    struct ControlledState {
        result: Option<Option<PathBuf>>,
        waker: Option<Waker>,
    }

    struct ControlledDialog(Arc<Mutex<ControlledState>>);

    impl Future for ControlledDialog {
        type Output = Option<PathBuf>;

        fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
            let mut state = self.0.lock().unwrap();
            if let Some(path) = state.result.take() {
                Poll::Ready(path)
            } else {
                state.waker = Some(task.waker().clone());
                Poll::Pending
            }
        }
    }

    fn complete(state: &Arc<Mutex<ControlledState>>, path: Option<PathBuf>) {
        let waker = {
            let mut state = state.lock().unwrap();
            state.result = Some(path);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn await_result<T>(
        dialog: &mut NativeFileDialog<T>,
        ctx: &egui::Context,
    ) -> (T, Option<PathBuf>) {
        let started = Instant::now();
        loop {
            if let Some(result) = dialog.poll(ctx) {
                return result;
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn ui_renders_while_native_future_poll_is_blocked() {
        // rfd's Linux async backend can block while exporting a parent handle.
        // Simulate that inside Future::poll, not just a well-behaved Pending.
        struct BlockingDialog {
            started: mpsc::SyncSender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Future for BlockingDialog {
            type Output = Option<PathBuf>;
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                self.started.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                Poll::Ready(Some(PathBuf::from("chosen.raw")))
            }
        }
        let ctx = egui::Context::default();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut dialog = NativeFileDialog::default();
        assert!(dialog.begin_test(
            BlockingDialog {
                started: started_tx,
                release: release_rx,
            },
            "open",
            &ctx,
        ));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for frame in 0..180 {
            let output = ctx.run(egui::RawInput::default(), |ctx, ui| {
                assert!(dialog.poll(ctx).is_none());
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.label(format!("Responsive frame {frame}"));
                });
            });
            assert!(!output.shapes.is_empty());
            assert!(dialog.pending());
        }
        release_tx.send(()).unwrap();
        assert_eq!(
            await_result(&mut dialog, &ctx),
            ("open", Some(PathBuf::from("chosen.raw")))
        );
        assert!(!dialog.pending());
        assert!(dialog.poll(&ctx).is_none());
    }

    #[test]
    fn native_completion_wakes_idle_egui() {
        let ctx = egui::Context::default();
        let repaints = Arc::new(AtomicUsize::new(0));
        let callback_count = repaints.clone();
        ctx.set_request_repaint_callback(move |_| {
            callback_count.fetch_add(1, Ordering::SeqCst);
        });
        let state = Arc::new(Mutex::new(ControlledState::default()));
        let mut dialog = NativeFileDialog::default();
        dialog.begin_test(ControlledDialog(state.clone()), (), &ctx);
        // Consume initial requests, as when the app is idle behind the picker.
        for _ in 0..4 {
            let _ = ctx.run(egui::RawInput::default(), |ctx, _ui| {
                assert!(dialog.poll(ctx).is_none());
            });
        }
        let before = repaints.load(Ordering::SeqCst);
        complete(&state, Some(PathBuf::from("saved.bin")));
        let started = Instant::now();
        while repaints.load(Ordering::SeqCst) == before {
            assert!(started.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            await_result(&mut dialog, &ctx),
            ((), Some(PathBuf::from("saved.bin")))
        );
    }

    #[test]
    fn cancel_preserves_payload_and_allows_another_request() {
        let ctx = egui::Context::default();
        let state = Arc::new(Mutex::new(ControlledState::default()));
        let mut dialog = NativeFileDialog::default();
        let mut yaml = String::from("original definition");
        dialog.begin_test(ControlledDialog(state.clone()), yaml.clone(), &ctx);
        yaml.clear();
        yaml.push_str("changed while picking a destination");
        assert!(dialog.poll(&ctx).is_none());
        complete(&state, None);
        assert_eq!(
            await_result(&mut dialog, &ctx),
            (String::from("original definition"), None)
        );
        assert!(!dialog.pending());
        assert!(dialog.begin_test(std::future::ready(None), yaml, &ctx));
        assert_eq!(
            await_result(&mut dialog, &ctx),
            (String::from("changed while picking a destination"), None)
        );
    }

    #[test]
    fn second_request_cannot_replace_pending_operation() {
        let ctx = egui::Context::default();
        let state = Arc::new(Mutex::new(ControlledState::default()));
        let mut dialog = NativeFileDialog::default();
        assert!(dialog.begin_test(ControlledDialog(state.clone()), 1, &ctx));
        assert!(!dialog.begin_test(
            std::future::ready(Some(PathBuf::from("wrong.bin"))),
            2,
            &ctx,
        ));
        complete(&state, Some(PathBuf::from("original.bin")));
        assert_eq!(
            await_result(&mut dialog, &ctx),
            (1, Some(PathBuf::from("original.bin")))
        );
    }

    #[test]
    fn dropping_owner_releases_payload_and_pending_worker() {
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct PendingUntilDropped(mpsc::SyncSender<()>);
        impl Future for PendingUntilDropped {
            type Output = Option<PathBuf>;
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Pending
            }
        }
        impl Drop for PendingUntilDropped {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let ctx = egui::Context::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let (dropped_tx, dropped_rx) = mpsc::sync_channel(1);
        let mut dialog = NativeFileDialog::default();
        dialog.begin_test(
            PendingUntilDropped(dropped_tx),
            Payload(drops.clone()),
            &ctx,
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(dialog);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}
