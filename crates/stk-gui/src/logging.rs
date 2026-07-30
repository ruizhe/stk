use std::{
    collections::VecDeque,
    fmt::Debug,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{
    EnvFilter, Layer,
    fmt::{self, MakeWriter},
    layer::SubscriberExt as _,
    registry::LookupSpan,
    util::SubscriberInitExt as _,
};

const MEMORY_LOG_LIMIT: usize = 2_000;
const MAX_LOG_FILE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiLogEntry {
    pub sequence: u64,
    pub timestamp_unix_ms: u64,
    pub level: GuiLogLevel,
    pub target: String,
    pub message: String,
    pub fields: String,
}

struct GuiLogCollector {
    entries: Mutex<VecDeque<GuiLogEntry>>,
    next_sequence: Mutex<u64>,
    log_path: PathBuf,
    file: Option<Arc<Mutex<File>>>,
}

impl GuiLogCollector {
    fn new(log_path: PathBuf, file: Option<Arc<Mutex<File>>>) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            next_sequence: Mutex::new(1),
            log_path,
            file,
        }
    }

    fn push(&self, mut entry: GuiLogEntry) {
        let mut next_sequence = lock_or_recover(&self.next_sequence);
        entry.sequence = *next_sequence;
        *next_sequence = next_sequence.saturating_add(1);
        drop(next_sequence);

        let mut entries = lock_or_recover(&self.entries);
        entries.push_front(entry);
        while entries.len() > MEMORY_LOG_LIMIT {
            entries.pop_back();
        }
    }

    fn snapshot(&self) -> Vec<GuiLogEntry> {
        lock_or_recover(&self.entries).iter().cloned().collect()
    }

    fn clear(&self) {
        lock_or_recover(&self.entries).clear();
        if let Some(file) = &self.file {
            let file = lock_or_recover(file);
            let _ = file.set_len(0);
        }
    }
}

#[derive(Clone)]
struct GuiLogLayer {
    collector: Arc<GuiLogCollector>,
}

impl<S> Layer<S> for GuiLogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &Attributes<'_>,
        id: &Id,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut visitor = EventVisitor::default();
        attributes.record(&mut visitor);
        span.extensions_mut().insert(SpanFields(visitor.fields));
    }

    fn on_record(
        &self,
        id: &Id,
        values: &Record<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut visitor = EventVisitor::default();
        values.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        if let Some(fields) = extensions.get_mut::<SpanFields>() {
            fields.0.extend(visitor.fields);
        } else {
            extensions.insert(SpanFields(visitor.fields));
        }
    }

    fn on_event(&self, event: &Event<'_>, context: tracing_subscriber::layer::Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let mut fields = Vec::new();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                let extensions = span.extensions();
                if let Some(span_fields) = extensions.get::<SpanFields>() {
                    if span_fields.0.is_empty() {
                        fields.push(span.metadata().name().to_string());
                    } else {
                        fields.push(format!(
                            "{}{{{}}}",
                            span.metadata().name(),
                            span_fields.0.join("  ")
                        ));
                    }
                }
            }
        }
        fields.extend(visitor.fields);
        self.collector.push(GuiLogEntry {
            sequence: 0,
            timestamp_unix_ms: current_unix_ms(),
            level: GuiLogLevel::from(metadata.level()),
            target: metadata.target().to_string(),
            message: visitor
                .message
                .unwrap_or_else(|| metadata.name().to_string()),
            fields: fields.join("  "),
        });
    }
}

struct SpanFields(Vec<String>);

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<String>,
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_value(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.record_value(field, format!("{value:?}"));
    }
}

impl EventVisitor {
    fn record_value(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value.trim_matches('"').to_string());
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }
}

impl From<&Level> for GuiLogLevel {
    fn from(level: &Level) -> Self {
        match *level {
            Level::TRACE => Self::Trace,
            Level::DEBUG => Self::Debug,
            Level::INFO => Self::Info,
            Level::WARN => Self::Warn,
            Level::ERROR => Self::Error,
        }
    }
}

#[derive(Clone)]
struct GuiLogMakeWriter {
    file: Option<Arc<Mutex<File>>>,
}

struct GuiLogWriter {
    file: Option<Arc<Mutex<File>>>,
}

impl<'a> MakeWriter<'a> for GuiLogMakeWriter {
    type Writer = GuiLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        GuiLogWriter {
            file: self.file.clone(),
        }
    }
}

impl Write for GuiLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Some(file) = &self.file {
            lock_or_recover(file).write(buffer)
        } else {
            io::stderr().write(buffer)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &self.file {
            lock_or_recover(file).flush()
        } else {
            io::stderr().flush()
        }
    }
}

static LOG_COLLECTOR: OnceLock<Arc<GuiLogCollector>> = OnceLock::new();

pub fn init(log_path: PathBuf) {
    rotate_log_if_needed(&log_path);
    let file = open_log_file(&log_path)
        .ok()
        .map(|file| Arc::new(Mutex::new(file)));
    let collector = Arc::new(GuiLogCollector::new(log_path, file.clone()));
    let _ = LOG_COLLECTOR.set(Arc::clone(&collector));
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let writer = GuiLogMakeWriter { file };
    let result = tracing_subscriber::registry()
        .with(filter)
        .with(GuiLogLayer { collector })
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_thread_names(true)
                .with_writer(writer),
        )
        .try_init();
    if let Err(error) = result {
        eprintln!("failed to initialize GUI logging: {error}");
    }
    install_panic_logging();
}

pub fn snapshot() -> Vec<GuiLogEntry> {
    LOG_COLLECTOR
        .get()
        .map_or_else(Vec::new, |collector| collector.snapshot())
}

pub fn clear() {
    if let Some(collector) = LOG_COLLECTOR.get() {
        collector.clear();
    }
}

pub fn log_path() -> Option<PathBuf> {
    LOG_COLLECTOR
        .get()
        .map(|collector| collector.log_path.clone())
}

fn open_log_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        options.mode(0o600);
        let file = options.open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    options.open(path)
}

fn rotate_log_if_needed(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() < MAX_LOG_FILE_BYTES {
        return;
    }
    let rotated = path.with_file_name(format!(
        "{}.1",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("stk.log")
    ));
    let _ = fs::remove_file(&rotated);
    let _ = fs::rename(path, rotated);
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn install_panic_logging() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let message = info
            .payload()
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                info.payload()
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_string())
            })
            .unwrap_or_else(|| "unknown panic payload".to_string());
        tracing::error!(%location, %message, "GUI thread panicked");
        previous(info);
    }));
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_keeps_newest_entries_first_and_clears_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stk.log");
        let file = Arc::new(Mutex::new(open_log_file(&path).unwrap()));
        let collector = GuiLogCollector::new(path.clone(), Some(Arc::clone(&file)));
        for message in ["first", "second"] {
            collector.push(GuiLogEntry {
                sequence: 0,
                timestamp_unix_ms: 1,
                level: GuiLogLevel::Info,
                target: "test".to_string(),
                message: message.to_string(),
                fields: String::new(),
            });
        }
        file.lock().unwrap().write_all(b"persisted\n").unwrap();

        let entries = collector.snapshot();
        assert_eq!(entries[0].message, "second");
        assert_eq!(entries[0].sequence, 2);
        assert_eq!(entries[1].message, "first");

        collector.clear();
        assert!(collector.snapshot().is_empty());
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
    }

    #[test]
    fn collector_includes_connection_span_fields() {
        let collector = Arc::new(GuiLogCollector::new(PathBuf::new(), None));
        let subscriber = tracing_subscriber::registry().with(GuiLogLayer {
            collector: Arc::clone(&collector),
        });

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("proxy_connection", connection_id = 42_u64);
            let _entered = span.enter();
            tracing::warn!(remote_target = "example.com:443", "channel open timed out");
        });

        let entries = collector.snapshot();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].fields.contains("connection_id=42"));
        assert!(entries[0].fields.contains("remote_target=example.com:443"));
    }
}
