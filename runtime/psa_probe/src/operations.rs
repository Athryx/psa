use crate::sandboxes::SandboxManager;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use psa::api::{
    Archive, MAX_SANDBOX_SEED_BYTES,
    probes::{
        CHUNK_SIZE, Command, DirectoryEntry, Event, ExitStatus, Operation, SandboxRequest,
        SandboxSpec, Stdio,
    },
};
use psa_shared::probe_transport::{Call, CallSender};
use std::{
    collections::HashMap,
    io,
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::Stdio as ProcessStdio,
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::io::SyncIoBridge;

#[cfg(test)]
#[path = "tests/operations/sync_dir_tests.rs"]
mod sync_dir_tests;

#[cfg(test)]
#[path = "tests/operations/seed_transfer_tests.rs"]
mod seed_transfer_tests;

static ARCHIVE_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
type PublicationLocks = HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>;
static DIRECTORY_PUBLICATIONS: LazyLock<tokio::sync::Mutex<PublicationLocks>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Probe-wide state shared by operations on every connection.
pub struct Context {
    /// Directory holding extracted probe tools.
    pub bin: PathBuf,
    /// Sandbox management, present when the probe advertises sandbox support.
    pub sandboxes: Option<SandboxManager>,
}

pub async fn execute(operation: Operation, mut call: Call, context: Arc<Context>) {
    let sender = call.sender.clone();
    let result = match operation {
        Operation::Spawn { command } => run_command(command, &mut call, &context.bin)
            .await
            .map(Some),
        Operation::Sandbox { request } => {
            let Some(manager) = &context.sandboxes else {
                let event = Event::Error {
                    code: "sandboxes_disabled".into(),
                    message: "this probe does not support sandboxes".into(),
                };
                let _ = sender.finish(event).await;
                return;
            };
            execute_sandbox_request(manager, request, &mut call).await
        }
        Operation::UploadDir { path } => upload_directory(path, &mut call, false)
            .await
            .map(|()| None),
        Operation::SyncDir { path } => upload_directory(path, &mut call, true).await.map(|()| None),
        Operation::DownloadDir { path } => download_directory(path, &mut call).await.map(|()| None),
        operation => tokio::select! {
            biased;
            _ = sender.closed() => Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled").into()),
            result = execute_filesystem_operation(operation, &mut call) => result.map(|()| None),
        },
    };

    let event = match result {
        Ok(status) => Event::Complete { status },
        Err(error) => Event::Error {
            code: "operation_failed".into(),
            message: error.to_string(),
        },
    };
    let _ = sender.finish(event).await;
}

async fn execute_sandbox_request(
    manager: &SandboxManager,
    request: SandboxRequest,
    call: &mut Call,
) -> Result<Option<ExitStatus>> {
    let request = match request {
        SandboxRequest::Create { mut spec } => {
            receive_seeds(&mut spec, call).await?;
            SandboxRequest::Create { spec }
        }
        request => request,
    };
    if let Some(containers) = manager.execute(request).await? {
        call.sender.event(Event::Containers { containers }).await?;
    }
    Ok(None)
}

/// Read the seed archives streamed after a create request into their mounts.
async fn receive_seeds(spec: &mut SandboxSpec, call: &mut Call) -> Result<()> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    if spec.seeds.is_empty() {
        return Ok(());
    }

    let mut total = 0u64;
    let mut targets = std::collections::HashSet::new();
    for transfer in &spec.seeds {
        let mount = spec
            .mounts
            .get(transfer.mount)
            .ok_or_else(|| invalid("seed for a mount that does not exist"))?;
        if mount.seed.is_some() || !targets.insert(transfer.mount) {
            return Err(invalid("mount has more than one seed").into());
        }
        total = total.saturating_add(transfer.size);
    }
    if total > MAX_SANDBOX_SEED_BYTES as u64 {
        return Err(invalid("mount seeds exceed the size limit").into());
    }

    let mut data = Vec::with_capacity(total as usize);
    loop {
        match call.next_event().await? {
            Event::Data { stream: 0, bytes } => {
                if (data.len() + bytes.len()) as u64 > total {
                    return Err(invalid("more seed data than announced").into());
                }
                data.extend_from_slice(&bytes);
            }
            Event::Eof { stream: 0 } => break,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "seed input cancelled or invalid",
                )
                .into());
            }
        }
    }
    if data.len() as u64 != total {
        return Err(invalid("less seed data than announced").into());
    }

    for transfer in std::mem::take(&mut spec.seeds) {
        let tar_data = data.drain(..transfer.size as usize).collect();
        spec.mounts[transfer.mount].seed = Some(Archive {
            tar_data,
            compression: transfer.compression,
        });
    }
    Ok(())
}

fn require_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.to_str().is_none() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "path must be absolute UTF-8").into(),
        );
    }
    Ok(())
}

async fn execute_filesystem_operation(operation: Operation, call: &mut Call) -> Result<()> {
    match operation {
        Operation::ReadFile { path } => {
            require_absolute(&path)?;
            let mut file = tokio::fs::File::open(path).await?;
            send_reader(&mut file, &call.sender, 1).await?;
        }
        Operation::WriteFile { path, append } => {
            require_absolute(&path)?;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(path)
                .await?;
            receive_writer(&mut file, call).await?;
            file.sync_all().await?;
        }
        Operation::CreateDir { path } => {
            require_absolute(&path)?;
            tokio::fs::create_dir_all(path).await?;
        }
        Operation::ListDir { path } => {
            require_absolute(&path)?;
            let entries = read_directory(&path).await?;
            call.sender.event(Event::Entries { entries }).await?;
        }
        Operation::DownloadDir { .. }
        | Operation::UploadDir { .. }
        | Operation::SyncDir { .. }
        | Operation::Spawn { .. }
        | Operation::Sandbox { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

async fn read_directory(path: &Path) -> Result<Vec<DirectoryEntry>> {
    let mut directory = tokio::fs::read_dir(path).await?;
    let mut entries = Vec::new();
    let mut bytes = 0;

    while let Some(entry) = directory.next_entry().await? {
        let metadata = match tokio::fs::symlink_metadata(entry.path()).await {
            Ok(metadata) => metadata,
            // A directory entry may disappear between readdir and metadata.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let name = entry.file_name().into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory contains non-UTF-8 filename",
            )
        })?;

        bytes += name.len();
        if entries.len() >= 10_000 || bytes > 512 * 1024 {
            return Err(io::Error::other("directory listing exceeds limit").into());
        }

        let kind = if metadata.is_file() {
            "file"
        } else if metadata.is_dir() {
            "directory"
        } else if metadata.is_symlink() {
            "symlink"
        } else {
            "other"
        };
        entries.push(DirectoryEntry {
            name,
            kind: kind.into(),
            size: metadata.len(),
        });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

async fn send_reader(
    reader: &mut (impl AsyncRead + Unpin),
    sender: &CallSender,
    stream: u8,
) -> Result<()> {
    let mut buffer = vec![0; CHUNK_SIZE];

    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        sender.write(stream, &buffer[..count]).await?;
    }
    sender.event(Event::Eof { stream }).await?;
    Ok(())
}

async fn receive_writer(writer: &mut (impl AsyncWrite + Unpin), call: &mut Call) -> Result<()> {
    loop {
        match call.next_event().await? {
            Event::Data { stream: 0, bytes } => writer.write_all(&bytes).await?,
            Event::Eof { stream: 0 } => {
                writer.shutdown().await?;
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "input cancelled or invalid",
                )
                .into());
            }
        }
    }
}

struct ProcessGroup {
    pid: Pid,
    armed: bool,
}
impl ProcessGroup {
    fn signal(&self, signal: Signal) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        match killpg(self.pid, signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn kill(&mut self) {
        let _ = self.signal(Signal::SIGKILL);
        self.armed = false;
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

fn build_stdio(value: Stdio) -> ProcessStdio {
    match value {
        Stdio::Piped => ProcessStdio::piped(),
        Stdio::Null => ProcessStdio::null(),
        Stdio::Inherit => ProcessStdio::inherit(),
    }
}

fn build_command(command: Command, bin: &Path) -> Result<tokio::process::Command> {
    require_absolute(&command.cwd)?;
    if command.program.is_empty() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "program must not be empty").into(),
        );
    }

    let mut process = tokio::process::Command::new(command.program);
    process
        .args(command.args)
        .current_dir(command.cwd)
        .kill_on_drop(true)
        .process_group(0)
        .stdin(build_stdio(command.stdin))
        .stdout(build_stdio(command.stdout))
        .stderr(build_stdio(command.stderr));

    if command.env_clear {
        process.env_clear();
    }
    for name in crate::image_hooks::HOOK_VARS {
        process.env_remove(name);
    }
    let mut path = if command.env_clear {
        None
    } else {
        std::env::var_os("PATH")
    };
    for (key, value) in command.env {
        if key == "PATH" {
            path = value.clone().map(Into::into);
        }
        match value {
            Some(value) => {
                process.env(key, value);
            }
            None => {
                process.env_remove(key);
            }
        }
    }

    let mut paths = vec![bin.to_owned()];
    if let Some(path) = path {
        paths.extend(std::env::split_paths(&path));
    }
    process.env("PATH", std::env::join_paths(paths)?);

    Ok(process)
}

async fn run_command(command: Command, call: &mut Call, bin: &Path) -> Result<ExitStatus> {
    let mut process = build_command(command, bin)?;
    let mut child = process.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("missing process ID"))?;
    let mut group = ProcessGroup {
        pid: Pid::from_raw(pid as i32),
        armed: true,
    };
    let sender = call.sender.clone();

    // Tasks own pipe halves, while this task remains responsive to signals and
    // cancellation even if the child never reads stdin or a consumer stalls.
    let mut output = tokio::task::JoinSet::new();
    if let Some(mut stdout) = child.stdout.take() {
        let sender = sender.clone();
        output.spawn(async move { send_reader(&mut stdout, &sender, 1).await });
    }
    if let Some(mut stderr) = child.stderr.take() {
        let sender = sender.clone();
        output.spawn(async move { send_reader(&mut stderr, &sender, 2).await });
    }

    let (_, placeholder) = tokio::sync::mpsc::channel(1);
    let mut signals = std::mem::replace(&mut call.signals, placeholder);

    // The stdin pump is polled independently from process supervision.
    let input = forward_stdin(child.stdin.take(), call);
    tokio::pin!(input);
    let mut input_done = false;
    let mut status = None;

    let result: Result<()> = async {
        sender.event(Event::Started { pid: Some(pid) }).await?;
        loop {
            if status.is_some() && output.is_empty() {
                return Ok(());
            }

            tokio::select! {
                biased;
                _ = sender.closed() => return Err(io::Error::new(io::ErrorKind::Interrupted, "command cancelled").into()),
                signal = signals.recv() => {
                    if let Some(signal) = signal {
                        group.signal(Signal::try_from(signal)?)?;
                    }
                }
                result = child.wait(), if status.is_none() => {
                    status = Some(result?);
                    group.kill();
                }
                result = &mut input, if !input_done => {
                    input_done = true;
                    if let Err(error) = result
                        && !error.downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
                    {
                        return Err(error);
                    }
                }
                result = output.join_next(), if !output.is_empty() => {
                    result.unwrap()??;
                }
            }
        }
    }.await;

    // Always clean up descendants, including children left behind by an exited
    // group leader. Reap the direct child before publishing completion.
    group.kill();
    if status.is_none() {
        let _ = child.wait().await;
    }
    output.shutdown().await;

    result?;
    let status = status.unwrap();
    Ok(ExitStatus {
        code: status.code(),
        signal: status.signal(),
    })
}

async fn forward_stdin(
    mut stdin: Option<tokio::process::ChildStdin>,
    call: &mut Call,
) -> Result<()> {
    loop {
        match call.next_event().await? {
            Event::Data { stream: 0, bytes } => {
                let pipe = stdin.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "stdin is closed or not piped")
                })?;
                pipe.write_all(&bytes).await?;
            }
            Event::Eof { stream: 0 } => {
                stdin.take();
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "command cancelled").into());
            }
        }
    }
}

// Dropping the outer future cancels archive traversal as well as its pipe I/O.
struct ArchiveCancellation(Arc<AtomicBool>);
impl Drop for ArchiveCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn download_directory(path: PathBuf, call: &mut Call) -> Result<()> {
    require_absolute(&path)?;
    let permit = acquire_archive_worker(&call.sender).await?;
    let (mut reader, writer) = tokio::io::duplex(CHUNK_SIZE);
    let cancelled = Arc::new(AtomicBool::new(false));
    let _guard = ArchiveCancellation(cancelled.clone());

    let worker_cancelled = cancelled.clone();
    let worker = tokio::task::spawn_blocking(move || -> Result<()> {
        let _permit = permit;
        let cancelled = worker_cancelled;
        write_directory_archive(&path, SyncIoBridge::new(writer), &cancelled)
    });

    let sent = tokio::select! {
        _ = call.sender.closed() => Err(io::Error::new(io::ErrorKind::Interrupted, "archive cancelled").into()),
        result = send_reader(&mut reader, &call.sender, 1) => result,
    };

    if sent.is_err() {
        cancelled.store(true, Ordering::Release);
    }
    drop(reader);
    let built = worker.await?;

    sent?;
    built?;
    Ok(())
}

async fn upload_directory(path: PathBuf, call: &mut Call, replace: bool) -> Result<()> {
    require_absolute(&path)?;
    if !replace && tokio::fs::try_exists(&path).await? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "upload destination already exists",
        )
        .into());
    }
    path.parent()
        .ok_or_else(|| io::Error::other("upload destination needs a parent directory"))?;
    let permit = acquire_archive_worker(&call.sender).await?;
    let (reader, mut writer) = tokio::io::duplex(CHUNK_SIZE);
    let cancelled = Arc::new(AtomicBool::new(false));
    let _guard = ArchiveCancellation(cancelled.clone());

    let worker_cancelled = cancelled.clone();
    let mut worker = tokio::task::spawn_blocking(move || -> Result<()> {
        let _permit = permit;
        let cancelled = worker_cancelled;
        extract_directory_archive(&path, SyncIoBridge::new(reader), &cancelled, replace)
    });

    let sender = call.sender.clone();
    let received = tokio::select! {
        _ = sender.closed() => Err(io::Error::new(io::ErrorKind::Interrupted, "archive cancelled").into()),
        result = receive_writer(&mut writer, call) => result,
        result = &mut worker => return result?,
    };

    if received.is_err() {
        cancelled.store(true, Ordering::Release);
    }
    drop(writer);
    let unpacked = worker.await?;

    received?;
    unpacked?;
    Ok(())
}

async fn acquire_archive_worker(
    sender: &CallSender,
) -> Result<tokio::sync::SemaphorePermit<'static>> {
    tokio::select! {
        _ = sender.closed() => Err(io::Error::new(io::ErrorKind::Interrupted, "archive cancelled").into()),
        permit = ARCHIVE_WORKERS.acquire() => Ok(permit?),
    }
}

// The archive codec functions run only inside the bounded blocking workers.
fn write_directory_archive(
    path: &Path,
    writer: impl io::Write,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut archive = tar::Builder::new(writer);
    if !std::fs::symlink_metadata(path)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "download source must be a directory",
        )
        .into());
    }

    archive.append_dir(".", path)?;
    let mut directories = vec![(PathBuf::from("."), std::fs::read_dir(path)?)];
    while let Some((relative, entries)) = directories.last_mut() {
        require_archive_active(cancelled)?;
        let Some(entry) = entries.next() else {
            directories.pop();
            continue;
        };
        let entry = entry?;
        let relative = relative.join(entry.file_name());
        let source = entry.path();
        let metadata = std::fs::symlink_metadata(&source)?;
        if metadata.is_dir() {
            archive.append_dir(&relative, &source)?;
            directories.push((relative, std::fs::read_dir(&source)?));
        } else if metadata.is_file() {
            archive.append_file(relative, &mut std::fs::File::open(source)?)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archives support only files and directories",
            )
            .into());
        }
    }
    archive.finish()?;
    Ok(())
}

fn extract_directory_archive(
    path: &Path,
    reader: impl io::Read,
    cancelled: &AtomicBool,
    replace: bool,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("upload destination needs a parent directory"))?;
    let staging = tempfile::Builder::new()
        .prefix(".psa-upload-")
        .tempdir_in(parent)?;
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    let mut directories = Vec::new();

    for entry in archive.entries()? {
        require_archive_active(cancelled)?;
        let mut entry = entry?;
        validate_archive_entry(&entry)?;

        if entry.header().entry_type().is_dir() {
            let relative = entry.path()?.into_owned();
            std::fs::create_dir_all(staging.path().join(&relative))?;
            directories.push((relative, entry.header().mode()?));
            continue;
        }
        if !entry.unpack_in(staging.path())? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive path escapes destination",
            )
            .into());
        }
    }

    // Wait for explicit stream EOF before publishing the staged directory.
    std::io::copy(&mut archive.into_inner(), &mut std::io::sink())?;
    require_archive_active(cancelled)?;

    let _publication = lock_directory_publication(path)?;
    require_archive_active(cancelled)?;
    let published = publish_directory(staging.path(), path, directories, replace);
    // After exchange this path holds the old tree, not the new destination.
    // Restore traversability before cleanup, including for read-only old trees.
    let cleaned = remove_staging_directory(staging);
    published?;
    cleaned
}

fn validate_archive_entry<R: io::Read>(entry: &tar::Entry<'_, R>) -> Result<()> {
    if !(entry.header().entry_type().is_file() || entry.header().entry_type().is_dir()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "links and special archive entries are unsupported",
        )
        .into());
    }
    if entry.path()?.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "archive path escapes destination",
        )
        .into());
    }
    Ok(())
}

// Called only by archive blocking workers. Parent canonicalization also serializes
// probe requests that spell the same destination through different parent aliases.
fn lock_directory_publication(destination: &Path) -> Result<tokio::sync::OwnedMutexGuard<()>> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("directory needs a parent"))?;
    let name = destination
        .file_name()
        .ok_or_else(|| io::Error::other("directory needs a name"))?;
    let key = std::fs::canonicalize(parent)?.join(name);
    let lock = {
        let mut locks = DIRECTORY_PUBLICATIONS.blocking_lock();
        locks.retain(|_, lock| lock.strong_count() != 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    Ok(lock.blocking_lock_owned())
}

fn publish_directory(
    staging: &Path,
    destination: &Path,
    mut directories: Vec<(PathBuf, u32)>,
    replace: bool,
) -> Result<()> {
    // Defer directory permissions so read-only directories can receive
    // their children before their final modes are applied.
    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    let published: Result<()> = (|| {
        for (relative, mode) in &directories {
            std::fs::set_permissions(
                staging.join(relative),
                std::fs::Permissions::from_mode(*mode),
            )?;
        }
        let flags = if replace {
            match std::fs::symlink_metadata(destination) {
                Ok(metadata) if metadata.is_dir() => rustix::fs::RenameFlags::EXCHANGE,
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "sync destination must be a directory, not a file or symlink",
                    )
                    .into());
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    rustix::fs::RenameFlags::NOREPLACE
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            rustix::fs::RenameFlags::NOREPLACE
        };
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            staging,
            rustix::fs::CWD,
            destination,
            flags,
        )?;
        Ok(())
    })();

    if published.is_err() {
        // Make staging traversable again so TempDir can remove read-only trees.
        let _ = std::fs::set_permissions(staging, std::fs::Permissions::from_mode(0o700));
        directories.reverse();
        for (relative, _) in directories {
            let _ = std::fs::set_permissions(
                staging.join(relative),
                std::fs::Permissions::from_mode(0o700),
            );
        }
    }

    published
}

fn remove_staging_directory(staging: tempfile::TempDir) -> Result<()> {
    let mut directories = vec![staging.path().to_owned()];
    while let Some(path) = directories.pop() {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() {
            continue;
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    match staging.close() {
        Ok(()) => Ok(()),
        // A non-replacing publication moved staging away entirely.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn require_archive_active(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "archive cancelled").into());
    }
    Ok(())
}
