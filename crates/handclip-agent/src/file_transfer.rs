use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use handclip_core::{ClipboardContent, ClipboardEvent, FileEntry, FileEntryKind};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const CHUNK_BYTES: usize = 256 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_ACTIVE_TRANSFERS: usize = 8;

#[derive(Debug)]
struct OutgoingEntry {
    wire: FileEntry,
    source: Option<PathBuf>,
}

#[derive(Debug)]
pub struct TransferSummary {
    pub transfer_id: Uuid,
    pub entry_count: usize,
    pub total_bytes: u64,
}

pub fn stream_paths(
    selected_paths: Vec<PathBuf>,
    origin: &str,
    mut publish: impl FnMut(ClipboardEvent) -> Result<()>,
) -> Result<TransferSummary> {
    if selected_paths.is_empty() {
        bail!("clipboard file list is empty");
    }

    let mut roots = Vec::with_capacity(selected_paths.len());
    let mut used_roots = HashSet::new();
    let mut outgoing = Vec::new();

    for selected_path in selected_paths {
        let metadata = fs::symlink_metadata(&selected_path)
            .with_context(|| format!("failed to inspect {}", selected_path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "symbolic links are not transferred: {}",
                selected_path.display()
            );
        }

        let source = selected_path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", selected_path.display()))?;
        let file_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("a selected file name is not valid UTF-8"))?;
        validate_component(file_name)?;
        let root_name = unique_root_name(file_name, &mut used_roots)?;
        roots.push(root_name.clone());
        collect_path(&source, &root_name, &mut outgoing)?;
    }

    if outgoing.len() > MAX_ENTRIES {
        bail!(
            "selection contains {} entries; the limit is {MAX_ENTRIES}",
            outgoing.len()
        );
    }
    let total_bytes = outgoing.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.wire.size)
            .ok_or_else(|| anyhow!("selection size overflowed"))
    })?;
    if total_bytes > MAX_TOTAL_BYTES {
        bail!(
            "selection is {} GiB; the limit is {} GiB",
            total_bytes / (1024 * 1024 * 1024),
            MAX_TOTAL_BYTES / (1024 * 1024 * 1024)
        );
    }

    let transfer_id = Uuid::new_v4();
    publish(ClipboardEvent::new(
        origin,
        ClipboardContent::FilesStart {
            transfer_id,
            roots,
            entries: outgoing.iter().map(|entry| entry.wire.clone()).collect(),
            total_bytes,
        },
    ))?;

    let mut hashes = Vec::with_capacity(outgoing.len());
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    for (index, entry) in outgoing.iter().enumerate() {
        let Some(source) = &entry.source else {
            hashes.push(None);
            continue;
        };

        let mut file =
            File::open(source).with_context(|| format!("failed to open {}", source.display()))?;
        let mut hasher = Sha256::new();
        let mut offset = 0_u64;
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("failed to read {}", source.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            publish(ClipboardEvent::new(
                origin,
                ClipboardContent::FileChunk {
                    transfer_id,
                    file_index: u32::try_from(index)
                        .context("file index exceeds protocol limits")?,
                    offset,
                    data_base64: BASE64.encode(&buffer[..read]),
                },
            ))?;
            offset = offset
                .checked_add(u64::try_from(read).expect("chunk size fits in u64"))
                .ok_or_else(|| anyhow!("file size overflowed"))?;
        }
        if offset != entry.wire.size {
            bail!(
                "{} changed while it was being transferred (expected {} bytes, read {offset})",
                source.display(),
                entry.wire.size
            );
        }
        hashes.push(Some(format!("{:x}", hasher.finalize())));
    }

    publish(ClipboardEvent::new(
        origin,
        ClipboardContent::FilesComplete {
            transfer_id,
            sha256: hashes,
        },
    ))?;

    Ok(TransferSummary {
        transfer_id,
        entry_count: outgoing.len(),
        total_bytes,
    })
}

fn collect_path(source: &Path, relative: &str, entries: &mut Vec<OutgoingEntry>) -> Result<()> {
    if entries.len() >= MAX_ENTRIES {
        bail!("selection exceeds the {MAX_ENTRIES}-entry limit");
    }

    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("symbolic links are not transferred: {}", source.display());
    }
    if metadata.is_file() {
        entries.push(OutgoingEntry {
            wire: FileEntry {
                relative_path: relative.to_owned(),
                kind: FileEntryKind::File,
                size: metadata.len(),
            },
            source: Some(source.to_owned()),
        });
        return Ok(());
    }
    if !metadata.is_dir() {
        bail!(
            "only regular files and directories are supported: {}",
            source.display()
        );
    }

    entries.push(OutgoingEntry {
        wire: FileEntry {
            relative_path: relative.to_owned(),
            kind: FileEntryKind::Directory,
            size: 0,
        },
        source: None,
    });

    let mut children = fs::read_dir(source)
        .with_context(|| format!("failed to read directory {}", source.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to enumerate directory {}", source.display()))?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let name = child
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("a file name under {} is not valid UTF-8", source.display()))?;
        validate_component(&name)?;
        collect_path(&child.path(), &format!("{relative}/{name}"), entries)?;
    }
    Ok(())
}

fn unique_root_name(name: &str, used: &mut HashSet<String>) -> Result<String> {
    if used.insert(name.to_owned()) {
        return Ok(name.to_owned());
    }
    for suffix in 2..=10_000 {
        let candidate = format!("{name} ({suffix})");
        validate_component(&candidate)?;
        if used.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    bail!("could not create a unique name for {name}")
}

#[derive(Debug)]
pub struct IncomingTransfers {
    base_dir: PathBuf,
    active: HashMap<(String, Uuid), IncomingTransfer>,
}

#[derive(Debug)]
struct IncomingTransfer {
    partial_dir: PathBuf,
    final_dir: PathBuf,
    roots: Vec<String>,
    entries: Vec<FileEntry>,
    received: Vec<u64>,
}

impl IncomingTransfers {
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&base_dir).with_context(|| {
            format!(
                "failed to create file-transfer directory {}",
                base_dir.display()
            )
        })?;
        Ok(Self {
            base_dir,
            active: HashMap::new(),
        })
    }

    pub fn handle(
        &mut self,
        origin: &str,
        content: ClipboardContent,
    ) -> Result<Option<Vec<PathBuf>>> {
        match content {
            ClipboardContent::FilesStart {
                transfer_id,
                roots,
                entries,
                total_bytes,
            } => {
                self.start(origin, transfer_id, roots, entries, total_bytes)?;
                Ok(None)
            }
            ClipboardContent::FileChunk {
                transfer_id,
                file_index,
                offset,
                data_base64,
            } => {
                self.chunk(origin, transfer_id, file_index, offset, &data_base64)?;
                Ok(None)
            }
            ClipboardContent::FilesComplete {
                transfer_id,
                sha256,
            } => self.complete(origin, transfer_id, sha256).map(Some),
            _ => bail!("content is not a file-transfer message"),
        }
    }

    fn start(
        &mut self,
        origin: &str,
        transfer_id: Uuid,
        roots: Vec<String>,
        entries: Vec<FileEntry>,
        total_bytes: u64,
    ) -> Result<()> {
        if self.active.len() >= MAX_ACTIVE_TRANSFERS {
            bail!("too many simultaneous incoming file transfers");
        }
        if roots.is_empty() || entries.is_empty() {
            bail!("file transfer manifest is empty");
        }
        if entries.len() > MAX_ENTRIES {
            bail!("file transfer exceeds the {MAX_ENTRIES}-entry limit");
        }
        let computed_total = entries.iter().try_fold(0_u64, |total, entry| {
            total
                .checked_add(entry.size)
                .ok_or_else(|| anyhow!("file transfer size overflowed"))
        })?;
        if computed_total != total_bytes {
            bail!("file transfer manifest has an inconsistent total size");
        }
        if total_bytes > MAX_TOTAL_BYTES {
            bail!("file transfer exceeds the configured size limit");
        }

        let mut root_set = HashSet::new();
        for root in &roots {
            validate_component(root)?;
            if !root_set.insert(root.clone()) {
                bail!("file transfer contains duplicate roots");
            }
        }

        let mut entry_set = HashSet::new();
        let mut validated_paths = Vec::with_capacity(entries.len());
        for entry in &entries {
            if entry.kind == FileEntryKind::Directory && entry.size != 0 {
                bail!("directory entries must have a size of zero");
            }
            let path = validated_relative_path(&entry.relative_path)?;
            let first = path
                .components()
                .next()
                .ok_or_else(|| anyhow!("file transfer contains an empty path"))?
                .as_os_str()
                .to_string_lossy()
                .into_owned();
            if !root_set.contains(&first) {
                bail!("file transfer entry is outside its declared roots");
            }
            if !entry_set.insert(entry.relative_path.clone()) {
                bail!("file transfer contains duplicate paths");
            }
            validated_paths.push(path);
        }
        for root in &roots {
            if !entry_set.contains(root) {
                bail!("file transfer root is missing from its entries");
            }
        }

        let key = (origin.to_owned(), transfer_id);
        if self.active.contains_key(&key) {
            bail!("file transfer has already started");
        }
        let directory_name = format!("{origin}-{transfer_id}");
        let partial_dir = self.base_dir.join(format!(".{directory_name}.partial"));
        let final_dir = self.base_dir.join(directory_name);
        if partial_dir.exists() || final_dir.exists() {
            bail!("file transfer staging directory already exists");
        }
        fs::create_dir(&partial_dir).with_context(|| {
            format!(
                "failed to create staging directory {}",
                partial_dir.display()
            )
        })?;

        for (entry, relative_path) in entries.iter().zip(&validated_paths) {
            let destination = partial_dir.join(relative_path);
            match entry.kind {
                FileEntryKind::Directory => {
                    fs::create_dir_all(&destination).with_context(|| {
                        format!("failed to create directory {}", destination.display())
                    })?
                }
                FileEntryKind::File => {
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("failed to create directory {}", parent.display())
                        })?;
                    }
                    File::create(&destination).with_context(|| {
                        format!("failed to create file {}", destination.display())
                    })?;
                }
            }
        }

        self.active.insert(
            key,
            IncomingTransfer {
                partial_dir,
                final_dir,
                roots,
                received: vec![0; entries.len()],
                entries,
            },
        );
        Ok(())
    }

    fn chunk(
        &mut self,
        origin: &str,
        transfer_id: Uuid,
        file_index: u32,
        offset: u64,
        data_base64: &str,
    ) -> Result<()> {
        let transfer = self
            .active
            .get_mut(&(origin.to_owned(), transfer_id))
            .ok_or_else(|| anyhow!("file chunk arrived before its manifest"))?;
        let index = usize::try_from(file_index).context("file index exceeds platform limits")?;
        let entry = transfer
            .entries
            .get(index)
            .ok_or_else(|| anyhow!("file chunk references an unknown file"))?;
        if entry.kind != FileEntryKind::File {
            bail!("file chunk references a directory");
        }
        if transfer.received[index] != offset {
            bail!(
                "file chunk offset is {}, expected {}",
                offset,
                transfer.received[index]
            );
        }
        let data = BASE64
            .decode(data_base64)
            .context("file chunk is not valid base64")?;
        if data.is_empty() || data.len() > CHUNK_BYTES {
            bail!("file chunk has an invalid size");
        }
        let new_size = offset
            .checked_add(u64::try_from(data.len()).expect("chunk size fits in u64"))
            .ok_or_else(|| anyhow!("file chunk size overflowed"))?;
        if new_size > entry.size {
            bail!("file chunk exceeds its declared file size");
        }

        let relative_path = validated_relative_path(&entry.relative_path)?;
        let destination = transfer.partial_dir.join(relative_path);
        let mut file = OpenOptions::new()
            .write(true)
            .open(&destination)
            .with_context(|| format!("failed to open staged file {}", destination.display()))?;
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("failed to seek in {}", destination.display()))?;
        file.write_all(&data)
            .with_context(|| format!("failed to write {}", destination.display()))?;
        transfer.received[index] = new_size;
        Ok(())
    }

    fn complete(
        &mut self,
        origin: &str,
        transfer_id: Uuid,
        hashes: Vec<Option<String>>,
    ) -> Result<Vec<PathBuf>> {
        let key = (origin.to_owned(), transfer_id);
        let transfer = self
            .active
            .remove(&key)
            .ok_or_else(|| anyhow!("file completion arrived before its manifest"))?;
        if hashes.len() != transfer.entries.len() {
            bail!("file completion has the wrong number of hashes");
        }

        for (index, ((entry, received), expected_hash)) in transfer
            .entries
            .iter()
            .zip(&transfer.received)
            .zip(&hashes)
            .enumerate()
        {
            match entry.kind {
                FileEntryKind::Directory => {
                    if expected_hash.is_some() {
                        bail!("directory entry {index} unexpectedly has a hash");
                    }
                }
                FileEntryKind::File => {
                    if *received != entry.size {
                        bail!(
                            "file {} is incomplete: received {} of {} bytes",
                            entry.relative_path,
                            received,
                            entry.size
                        );
                    }
                    let expected_hash = expected_hash
                        .as_deref()
                        .ok_or_else(|| anyhow!("file {} has no hash", entry.relative_path))?;
                    if !is_sha256(expected_hash) {
                        bail!("file {} has an invalid hash", entry.relative_path);
                    }
                    let path = transfer
                        .partial_dir
                        .join(validated_relative_path(&entry.relative_path)?);
                    let actual_hash = hash_file(&path)?;
                    if actual_hash != expected_hash {
                        bail!("file {} failed integrity verification", entry.relative_path);
                    }
                }
            }
        }

        fs::rename(&transfer.partial_dir, &transfer.final_dir).with_context(|| {
            format!(
                "failed to finalize transfer at {}",
                transfer.final_dir.display()
            )
        })?;
        Ok(transfer
            .roots
            .iter()
            .map(|root| transfer.final_dir.join(root))
            .collect())
    }
}

fn validated_relative_path(value: &str) -> Result<PathBuf> {
    if value.is_empty() || value.starts_with('/') || value.ends_with('/') {
        bail!("file transfer contains an invalid relative path");
    }
    let mut path = PathBuf::new();
    for component in value.split('/') {
        validate_component(component)?;
        path.push(component);
    }
    Ok(path)
}

fn validate_component(component: &str) -> Result<()> {
    if component.is_empty()
        || matches!(component, "." | "..")
        || component.len() > 255
        || component.ends_with([' ', '.'])
        || component.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        bail!("file name is not portable across macOS and Windows: {component:?}");
    }

    let device_name = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_uppercase();
    if matches!(device_name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device_name.len() == 4
            && (device_name.starts_with("COM") || device_name.starts_with("LPT"))
            && matches!(device_name.as_bytes()[3], b'1'..=b'9'))
    {
        bail!("file name is reserved on Windows: {component:?}");
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_and_windows_reserved_paths() {
        assert!(validated_relative_path("folder/report.pdf").is_ok());
        assert!(validated_relative_path("../secret").is_err());
        assert!(validated_relative_path("folder\\secret").is_err());
        assert!(validated_relative_path("CON.txt").is_err());
        assert!(validated_relative_path("folder/name.").is_err());
    }

    #[test]
    fn creates_unique_root_names() {
        let mut used = HashSet::new();
        assert_eq!(
            unique_root_name("report.pdf", &mut used).unwrap(),
            "report.pdf"
        );
        assert_eq!(
            unique_root_name("report.pdf", &mut used).unwrap(),
            "report.pdf (2)"
        );
    }

    #[test]
    fn streams_and_restores_a_directory_tree() {
        let test_root = std::env::temp_dir().join(format!("handclip-test-{}", Uuid::new_v4()));
        let source_root = test_root.join("source").join("bundle");
        fs::create_dir_all(source_root.join("empty")).unwrap();
        fs::write(source_root.join("hello.txt"), b"hello from Handclip").unwrap();
        fs::write(source_root.join("zero.bin"), b"").unwrap();

        let mut events = Vec::new();
        let summary = stream_paths(vec![source_root], "air", |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.total_bytes, 19);

        let mut incoming =
            IncomingTransfers::new(test_root.join("received")).expect("create receiver");
        let mut restored_roots = None;
        for event in events {
            restored_roots = incoming
                .handle("air", event.content)
                .unwrap()
                .or(restored_roots);
        }
        let restored = restored_roots.unwrap().pop().unwrap();
        assert_eq!(
            fs::read(restored.join("hello.txt")).unwrap(),
            b"hello from Handclip"
        );
        assert_eq!(fs::metadata(restored.join("zero.bin")).unwrap().len(), 0);
        assert!(restored.join("empty").is_dir());

        fs::remove_dir_all(test_root).unwrap();
    }
}
