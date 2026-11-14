use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{Datelike, Local, Timelike};
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value};
use zip::write::SimpleFileOptions;

use crate::_base::utils::abort::{AbortError, AbortSignal};
use crate::_base::utils::paths::canonical_path;
use crate::wire::record::WireAsyncError;

use super::errors::SESSION_EXPORT_OUTPUT_CONFLICT;
use super::file_source::{ZipSource, ZipSourceIdentity, open_zip_source_sync};
use super::session_export::ExportSessionManifest;

#[derive(Clone)]
pub enum ExtraZipEntry {
    Source { source: ZipSource, target: String },
    Data { data: Vec<u8>, target: String },
}

impl ExtraZipEntry {
    pub fn target(&self) -> &str {
        match self {
            Self::Source { target, .. } | Self::Data { target, .. } => target,
        }
    }
}

#[derive(Clone)]
pub enum SessionZipEntry {
    Path(String),
    Source { path: String, source: ZipSource },
}

pub struct WriteExportZipArgs {
    pub output_path: String,
    pub manifest: ExportSessionManifest,
    pub session_dir: String,
    pub session_files: Vec<SessionZipEntry>,
    pub extra_entries: Option<Vec<ExtraZipEntry>>,
    pub signal: Option<AbortSignal>,
}

pub async fn collect_files_recursive(root: &str) -> Result<Vec<String>, WireAsyncError> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::from(root)];
    while let Some(directory) = pending.pop() {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Box::new(error)),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| Box::new(error) as WireAsyncError)?
        {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .map_err(|error| Box::new(error) as WireAsyncError)?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                files.push(path.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files.sort();
    Ok(files)
}

pub async fn write_export_zip(
    args: WriteExportZipArgs,
) -> Result<Vec<String>, WireAsyncError> {
    let entries = archive_entries(&args);
    let sources = predefined_sources(&args);
    let output_path = args.output_path.clone();
    let signal = args.signal.clone();
    let mut temp_dir = None;
    let mut result = async {
        throw_if_aborted(signal.as_ref())?;
        if let Some(conflicting_source) = find_conflicting_source(&args).await? {
            return Err(Box::new(conflict_error(
                &args.output_path,
                &conflicting_source,
            )) as WireAsyncError);
        }
        let output = Path::new(&args.output_path);
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| Box::new(error) as WireAsyncError)?;
        throw_if_aborted(signal.as_ref())?;
        let directory = create_temp_directory(parent).await?;
        let temp_output = directory.join("archive.zip");
        temp_dir = Some(directory);
        let write_output = temp_output.clone();
        let write_args = args;
        tokio::task::spawn_blocking(move || write_archive_sync(write_args, &write_output))
            .await
            .map_err(|error| Box::new(error) as WireAsyncError)??;
        throw_if_aborted(signal.as_ref())?;
        tokio::fs::rename(&temp_output, output)
            .await
            .map_err(|error| Box::new(error) as WireAsyncError)?;
        Ok(())
    }
    .await;

    for source in sources {
        if let Err(error) = source.close().await
            && result.is_ok()
        {
            result = Err(error);
        }
    }
    if let Some(directory) = temp_dir
        && let Err(error) = tokio::fs::remove_dir_all(directory).await
        && error.kind() != io::ErrorKind::NotFound
        && result.is_ok()
    {
        result = Err(Box::new(error));
    }
    result?;
    Ok(entries)
}

fn write_archive_sync(
    args: WriteExportZipArgs,
    temp_output: &Path,
) -> Result<(), WireAsyncError> {
    throw_if_aborted(args.signal.as_ref())?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_output)
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    let mut zip = zip::ZipWriter::new(output);
    let manifest = serde_json::to_vec_pretty(&args.manifest)
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    zip.start_file("manifest.json", zip_options(None)?)
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    zip.write_all(&manifest)
        .map_err(|error| Box::new(error) as WireAsyncError)?;

    for entry in &args.session_files {
        throw_if_aborted(args.signal.as_ref())?;
        let source_path = session_entry_path(entry);
        let target = session_entry_target(&args.session_dir, source_path);
        match entry {
            SessionZipEntry::Path(path) => {
                let source = open_zip_source_sync(path, args.signal.as_ref())?;
                add_source(&mut zip, &target, &source, false, args.signal.as_ref())?;
            }
            SessionZipEntry::Source { source, .. } => {
                add_source(&mut zip, &target, source, true, args.signal.as_ref())?;
            }
        }
    }

    for extra in args.extra_entries.as_deref().unwrap_or(&[]) {
        throw_if_aborted(args.signal.as_ref())?;
        match extra {
            ExtraZipEntry::Data { data, target } => {
                zip.start_file(target, zip_options(None)?)
                    .map_err(|error| Box::new(error) as WireAsyncError)?;
                zip.write_all(data)
                    .map_err(|error| Box::new(error) as WireAsyncError)?;
            }
            ExtraZipEntry::Source { source, target } => {
                add_source(&mut zip, target, source, true, args.signal.as_ref())?;
            }
        }
    }
    throw_if_aborted(args.signal.as_ref())?;
    let output = zip
        .finish()
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    output
        .sync_all()
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    Ok(())
}

fn add_source(
    zip: &mut zip::ZipWriter<File>,
    target: &str,
    source: &ZipSource,
    include_metadata: bool,
    signal: Option<&AbortSignal>,
) -> Result<(), WireAsyncError> {
    let options = if include_metadata {
        zip_options(Some(source))?
    } else {
        zip_options(None)?
    };
    zip.start_file(target, options)
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    let mut stream = source.stream(signal.cloned());
    let copied = io::copy(&mut stream, zip).map_err(|error| Box::new(error) as WireAsyncError);
    drop(stream);
    copied?;
    Ok(())
}

fn zip_options(source: Option<&ZipSource>) -> Result<SimpleFileOptions, WireAsyncError> {
    let mtime = source
        .map(|source| source.mtime)
        .unwrap_or_else(SystemTime::now);
    let modified = zip_datetime(mtime)?;
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(modified);
    Ok(match source {
        Some(source) => options.unix_permissions(source.mode),
        None => options,
    })
}

fn zip_datetime(value: SystemTime) -> Result<zip::DateTime, WireAsyncError> {
    let local = chrono::DateTime::<Local>::from(value);
    let year = u16::try_from(local.year()).map_err(|error| Box::new(error) as WireAsyncError)?;
    zip::DateTime::from_date_and_time(
        year,
        local.month() as u8,
        local.day() as u8,
        local.hour() as u8,
        local.minute() as u8,
        local.second() as u8,
    )
    .map_err(|error| Box::new(error) as WireAsyncError)
}

async fn find_conflicting_source(
    args: &WriteExportZipArgs,
) -> Result<Option<String>, WireAsyncError> {
    throw_if_aborted(args.signal.as_ref())?;
    let output_path = canonical_path(&args.output_path);
    for entry in &args.session_files {
        let source_path = session_entry_path(entry);
        if canonical_path(source_path) == output_path {
            return Ok(Some(source_path.to_string()));
        }
    }
    for entry in args.extra_entries.as_deref().unwrap_or(&[]) {
        if let ExtraZipEntry::Source { source, target } = entry
            && source
                .source_path
                .as_deref()
                .is_some_and(|path| canonical_path(path) == output_path)
        {
            return Ok(Some(target.clone()));
        }
    }

    let Some(output) = stat_existing(&output_path).await? else {
        return Ok(None);
    };
    for entry in &args.session_files {
        throw_if_aborted(args.signal.as_ref())?;
        let input = match entry {
            SessionZipEntry::Path(path) => stat_existing(path).await?,
            SessionZipEntry::Source { source, .. } => Some(source.identity.clone()),
        };
        if input.is_some_and(|input| same_file(&output, &input)) {
            return Ok(Some(session_entry_path(entry).to_string()));
        }
    }
    for entry in args.extra_entries.as_deref().unwrap_or(&[]) {
        throw_if_aborted(args.signal.as_ref())?;
        if let ExtraZipEntry::Source { source, target } = entry
            && same_file(&output, &source.identity)
        {
            return Ok(Some(target.clone()));
        }
    }
    Ok(None)
}

async fn stat_existing(path: &str) -> Result<Option<ZipSourceIdentity>, WireAsyncError> {
    let path = path.to_string();
    let result = tokio::task::spawn_blocking(move || ZipSourceIdentity::from_path(&path))
        .await
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    match result {
        Ok(identity) => Ok(Some(identity)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Box::new(error)),
    }
}

fn same_file(left: &ZipSourceIdentity, right: &ZipSourceIdentity) -> bool {
    #[cfg(unix)]
    {
        return left.inode != 0
            && right.inode != 0
            && left.device == right.device
            && left.inode == right.inode;
    }
    #[cfg(not(unix))]
    {
        match (&left.handle, &right.handle) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }
}

fn predefined_sources(args: &WriteExportZipArgs) -> Vec<ZipSource> {
    let mut sources = Vec::new();
    for entry in &args.session_files {
        if let SessionZipEntry::Source { source, .. } = entry {
            sources.push(source.clone());
        }
    }
    for entry in args.extra_entries.as_deref().unwrap_or(&[]) {
        if let ExtraZipEntry::Source { source, .. } = entry {
            sources.push(source.clone());
        }
    }
    sources
}

fn archive_entries(args: &WriteExportZipArgs) -> Vec<String> {
    let mut entries = vec!["manifest.json".to_string()];
    entries.extend(args.session_files.iter().map(|entry| {
        session_entry_target(&args.session_dir, session_entry_path(entry))
    }));
    entries.extend(
        args.extra_entries
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|entry| entry.target().to_string()),
    );
    entries
}

fn session_entry_path(entry: &SessionZipEntry) -> &str {
    match entry {
        SessionZipEntry::Path(path) => path,
        SessionZipEntry::Source { path, .. } => path,
    }
}

fn session_entry_target(session_dir: &str, source_path: &str) -> String {
    if let Ok(relative) = Path::new(source_path).strip_prefix(session_dir) {
        return relative.to_string_lossy().replace('\\', "/");
    }
    let session_dir = canonical_path(session_dir);
    let source_path = canonical_path(source_path);
    match Path::new(&source_path).strip_prefix(&session_dir) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => source_path.replace('\\', "/"),
    }
}

async fn create_temp_directory(parent: &Path) -> Result<PathBuf, WireAsyncError> {
    for _ in 0..100 {
        let candidate = parent.join(format!(
            ".kimi-session-export-{}-{:016x}",
            std::process::id(),
            fastrand::u64(..)
        ));
        match tokio::fs::create_dir(&candidate).await {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Box::new(error)),
        }
    }
    Err(Box::new(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary session export directory",
    )))
}

fn conflict_error(output_path: &str, source: &str) -> Error2 {
    let mut details = Map::new();
    details.insert(
        "outputPath".to_string(),
        Value::String(output_path.to_string()),
    );
    details.insert("source".to_string(), Value::String(source.to_string()));
    Error2::with_options(
        SESSION_EXPORT_OUTPUT_CONFLICT,
        format!("Session export output conflicts with selected source \"{source}\"."),
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    )
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if let Some(signal) = signal
        && signal.aborted()
    {
        let message = signal
            .reason()
            .map(|reason| reason.to_string())
            .unwrap_or_else(|| "The operation was aborted.".to_string());
        return Err(Box::new(AbortError::new(message)));
    }
    Ok(())
}
