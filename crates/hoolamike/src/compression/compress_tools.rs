use {
    super::{ProcessArchive, *},
    ::compress_tools::*,
    anyhow::{Context, Result},
    itertools::Itertools,
    num::ToPrimitive,
    std::{
        collections::HashSet,
        io::{BufWriter, Seek},
        path::PathBuf,
    },
    tempfile::SpooledTempFile,
    tracing::{instrument, trace, trace_span},
    tracing_indicatif::span_ext::IndicatifSpanExt,
};

pub type CompressToolsFile = tempfile::SpooledTempFile;

#[derive(Debug)]
pub struct ArchiveHandle(std::fs::File);

impl ArchiveHandle {
    #[tracing::instrument(level = "TRACE")]
    pub fn new(mut file: std::fs::File) -> Result<Self> {
        list_archive_files_with_encoding(&mut file, |_| Ok(String::new()))
            .context("listing files")
            .and_then(|_| file.rewind().context("rewinding the stream"))
            .context("could not read with compress-tools (libarchive)")
            .map(|_| Self(file))
    }

    #[tracing::instrument(skip(self))]
    pub fn get_handle(&mut self, for_path: &Path) -> Result<CompressToolsFile> {
        self.0.rewind().context("rewinding file")?;
        let lookup = for_path.display().to_string();
        list_archive_files(&mut self.0)
            .context("listing archive")
            .map(|files| files.into_iter().collect::<std::collections::HashSet<_>>())
            .and_then(|files| {
                files
                    .contains(&lookup)
                    .then_some(&lookup)
                    .with_context(|| format!("no [{lookup}] in {files:?}"))
                    .tap_ok(|lookup| trace!("[{lookup}] found in [{files:?}]"))
            })
            .and_then(|lookup| {
                self.0.rewind().context("rewinding file")?;
                tempfile::SpooledTempFile::new(16 * 1024).pipe(|mut temp_file| {
                    {
                        let mut writer = BufWriter::new(&mut temp_file);
                        trace_span!("uncompress_archive_file")
                            .in_scope(|| uncompress_archive_file(&mut tracing::Span::current().wrap_read(0, &mut self.0), &mut writer, lookup))
                    }
                    .context("extracting archive")
                    .tap_ok(|bytes| trace!(%bytes, "extracted from CompressTools archive"))
                    .and_then(|_| {
                        temp_file
                            .flush()
                            .and_then(|_| temp_file.rewind())
                            .context("rewinding to beginning of file")
                            .map(|_| temp_file)
                    })
                })
            })
    }
}

impl ProcessArchive for ArchiveHandle {
    #[instrument(skip(self), level = "TRACE")]
    fn list_paths(&mut self) -> Result<Vec<PathBuf>> {
        ::compress_tools::list_archive_files(&mut self.0)
            .context("listing archive files")
            .map(|e| e.into_iter().map(PathBuf::from).collect())
            .and_then(|out| self.0.rewind().context("rewinding file").map(|_| out))
    }

    fn get_many_handles(&mut self, paths: &[&Path]) -> Result<Vec<(PathBuf, super::ArchiveFileHandle)>> {
        self.list_paths().and_then(|listed| {
            listed
                .into_iter()
                .collect::<HashSet<_>>()
                .pipe(|mut listed| {
                    paths
                        .iter()
                        .map(|expected| {
                            listed
                                .remove(*expected)
                                .then(|| expected.to_owned().pipe(|v| v.to_owned()))
                                .with_context(|| format!("path {expected:?} not found in {listed:?}"))
                        })
                        .collect::<Result<HashSet<PathBuf>>>()
                        .context("some paths were not found")
                        .and_then(|mut validated_paths| {
                            let extracting_mutliple_files = info_span!("extracting_mutliple_files", file_count=%validated_paths.len());
                            compress_tools::ArchiveIteratorBuilder::new(&mut self.0)
                                .filter({
                                    cloned![validated_paths];
                                    move |e, _| validated_paths.contains(Path::new(e))
                                })
                                .build()
                                .context("building archive iterator")
                                .and_then(|mut iterator| {
                                    iterator
                                        .try_fold(vec![], move |mut acc, entry| match entry {
                                            ArchiveContents::StartOfEntry(entry_path, stat) => entry_path.pipe(PathBuf::from).pipe(|entry_path| {
                                                extracting_mutliple_files.pb_set_message(&entry_path.to_string_lossy());
                                                extracting_mutliple_files.pb_inc_length(stat.st_size.to_u64().context("negative size")?);
                                                validated_paths
                                                    .remove(entry_path.as_path())
                                                    .then_some(entry_path.clone())
                                                    .with_context(|| format!("unrequested entry: {entry_path:?}"))
                                                    .map(|path| acc.tap_mut(|acc| acc.push((path, stat.st_size, SpooledTempFile::new(16 * 1024)))))
                                            }),
                                            ArchiveContents::DataChunk(chunk) => acc
                                                .last_mut()
                                                .context("no write in progress")
                                                .and_then({
                                                    cloned![extracting_mutliple_files];
                                                    move |(_, size, acc)| {
                                                        std::io::copy(
                                                            &mut extracting_mutliple_files
                                                                .wrap_read(size.to_u64().context("negative size")?, std::io::Cursor::new(chunk)),
                                                            acc,
                                                        )
                                                        .context("writing to temp file failed")
                                                    }
                                                })
                                                .map(|_| acc),
                                            ArchiveContents::EndOfEntry => Ok(acc),
                                            ArchiveContents::Err(error) => Err(error).with_context(|| {
                                                format!(
                                                    "when reading: {}",
                                                    acc.last_mut()
                                                        .map(|(path, size, _)| format!("{path:?} size={size}"))
                                                        .unwrap_or_else(|| "before reading started".to_string()),
                                                )
                                            }),
                                        })
                                        .context("reading multiple paths from archive")
                                })
                                .map(|paths| {
                                    paths
                                        .into_iter()
                                        .map(|(path, _size, file)| (path, self::ArchiveFileHandle::CompressTools(file)))
                                        .collect_vec()
                                })
                        })
                })
        })
    }

    #[instrument(skip(self), level = "TRACE")]
    fn get_handle<'this>(&mut self, path: &Path) -> Result<super::ArchiveFileHandle> {
        self.get_handle(path)
            .map(super::ArchiveFileHandle::CompressTools)
    }
}

impl super::ProcessArchiveFile for CompressToolsFile {}
