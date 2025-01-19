use {
    crate::{
        compression::{ProcessArchive, SeekWithTempFileExt},
        config_file::HoolamikeConfig,
        modlist_json::GameName,
        progress_bars_v2::{count_progress_style, IndicatifWrapIoExt},
        utils::{scoped_temp_file, MaybeWindowsPath, PathReadWrite},
    },
    anyhow::{Context, Result},
    manifest_file::{
        asset::{FullLocation, LocationIndex, MaybeFullLocation},
        kind_guard::WithKindGuard,
        location::{Location, ReadArchiveLocation, WriteArchiveLocation},
        variable::Variable,
        Package,
    },
    normalize_path::NormalizePath,
    num::ToPrimitive,
    parking_lot::Mutex,
    rayon::iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    serde::{Deserialize, Serialize},
    std::{
        borrow::Cow,
        collections::BTreeMap,
        io::{BufReader, Read},
        path::{Path, PathBuf},
        sync::Arc,
    },
    tap::prelude::*,
    tempfile::TempPath,
    tracing::{debug, info, info_span, instrument, warn},
    tracing_indicatif::span_ext::IndicatifSpanExt,
};

pub mod manifest_file;
pub mod templating {
    /// returns (left, variable_name, right)
    pub fn find_template_marker(input: &str) -> Option<(&str, &str, &str)> {
        input.split_once('%').and_then(|(left, right)| {
            right
                .split_once('%')
                .map(|(variable_name, right)| (left, variable_name, right))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionConfig {
    path_to_ttw_mpi_file: PathBuf,
    variables: BTreeMap<String, String>,
}

#[derive(clap::Args)]
pub struct CliConfig {
    /// will only run assets containing this chunk of text, useful for debugging
    #[arg(long)]
    contains: Vec<String>,
}

const MANIFEST_PATH: &str = "_package/index.json";

type LocationsLookup = BTreeMap<LocationIndex, Location>;

#[derive(Clone)]
struct RepackingContext {
    queued_archives: Arc<Mutex<BTreeMap<PathBuf, LazyArchive>>>,
    locations: Arc<LocationsLookup>,
}

#[derive(Debug)]
struct LazyArchive {
    files: Vec<(PathBuf, TempPath)>,
    #[allow(dead_code)]
    archive_metadata: WriteArchiveLocation,
}

impl LazyArchive {
    #[instrument]
    fn new(metadata: &WriteArchiveLocation) -> Self {
        debug!("scheduling new archive");
        Self {
            files: Vec::new(),
            archive_metadata: metadata.clone(),
        }
    }

    #[instrument]
    fn insert(&mut self, archive_path: PathBuf, file: TempPath) {
        debug!("scheduling file into archive");
        self.files.push((archive_path, file))
    }
}

impl RepackingContext {
    pub fn new(locations: Arc<LocationsLookup>) -> Self {
        Self {
            queued_archives: Default::default(),
            locations,
        }
    }
}

struct VariablesContext {
    variables: BTreeMap<String, Variable>,
    ttw_config_variables: BTreeMap<String, String>,
    hoolamike_installation_config: HoolamikeConfig,
}

impl VariablesContext {
    #[instrument(skip(self))]
    fn resolve_variable(&self, maybe_with_variable: &str) -> Result<Cow<str>> {
        match self::templating::find_template_marker(maybe_with_variable) {
            Some((left, variable_name, right)) => info_span!("variable_found", %variable_name)
                .in_scope(|| match variable_name {
                    "FO3ROOT" => self
                        .hoolamike_installation_config
                        .games
                        .get(&GameName::new("Fallout3".to_string()))
                        .context("'Fallout3' is not found in hoolamike defined games")
                        .map(|p| p.root_directory.display().to_string().pipe(Cow::Owned))
                        .tap_ok(|value| info!(%variable_name, %value, "⭐⭐⭐ MAGICALLY ⭐⭐⭐ filling the variable using hoolamike derived context")),

                    "FNVROOT" => self
                        .hoolamike_installation_config
                        .games
                        .get(&GameName::new("FalloutNewVegas".to_string()))
                        .context("'FalloutNewVegas' is not found in hoolamike defined games")
                        .map(|p| p.root_directory.display().to_string().pipe(Cow::Owned))
                        .tap_ok(|value| info!(%variable_name, %value, "⭐⭐⭐ MAGICALLY ⭐⭐⭐ filling the variable using hoolamike derived context")),

                    variable_name => match self.variables.get(variable_name) {
                        Some(variable) => Err(())
                            .or_else(|_| {
                                self.ttw_config_variables
                                    .get(variable_name)
                                    .map(|v| v.as_str().pipe(Cow::Borrowed))
                                    .with_context(|| format!("no variable defined in hoolamike config: '{variable_name}'"))
                            })
                            .or_else(|reason| {
                                variable
                                    .value()
                                    .filter(|v| {
                                        !v.is_empty().tap(|is_empty| {
                                            if *is_empty {
                                                tracing::warn!("variable [{variable_name}] is empty which means it should be filled by the user");
                                            }
                                        })
                                    })
                                    .map(Cow::Borrowed)
                                    .context("variable not found in installer variable definition section")
                                    .with_context(|| format!("{reason:?}"))
                            }),
                        None => Err(anyhow::anyhow!("ttw installer does not define this variable: '{variable_name}'")),
                    },
                })
                .and_then(|updated| self.resolve_variable(&updated))
                .map(|variable| format!("{left}{variable}{right}"))
                .map(Cow::Owned)
                .inspect(|updated_value| tracing::info!(%updated_value, "updated templated value")),
            None => Ok(Cow::Owned(
                maybe_with_variable
                    .to_string()
                    .tap(|value| tracing::debug!(%value, "value does not contain variables")),
            )),
        }
        .context("HINT: you can override the variables in hoolamike config")
    }
}

impl MaybeFullLocation {
    fn lookup_from_both_source_and_target(self, source: &FullLocation) -> FullLocation {
        match self.path {
            Some(path) => FullLocation { location: self.location, path },
            None => FullLocation {
                location: self.location,
                path: source.path.clone(),
            },
        }
    }
}

impl FullLocation {
    #[instrument(level = "DEBUG", skip(from_reader, repacking_context))]
    fn insert_into(self, repacking_context: RepackingContext, from_reader: &mut impl Read) -> Result<()> {
        repacking_context
            .locations
            .get(&self.location)
            .with_context(|| format!("no location for {self:#?}"))
            .inspect(|location| tracing::debug!("{location:#?}"))
            .and_then(|location| match location {
                Location::Folder(folder) => folder
                    .inner
                    .value
                    .clone()
                    .pipe(MaybeWindowsPath)
                    .pipe(MaybeWindowsPath::into_path)
                    .pipe(|folder| folder.join(self.path.0.into_path()).normalize())
                    .open_file_write()
                    .and_then(|(target_path, mut target_file)| {
                        std::io::copy(from_reader, &mut target_file)
                            .with_context(|| format!("copying into [{target_path:#?}]"))
                            .map(|wrote| tracing::info!(?target_path, "wrote [{wrote}bytes]"))
                    }),
                Location::ReadArchive(read_archive) => anyhow::bail!("cannot insert into Location::ReadArchive({read_archive:#?})"),
                Location::WriteArchive(write_archive) => write_archive
                    .inner
                    .value
                    .clone()
                    .pipe(MaybeWindowsPath)
                    .pipe(MaybeWindowsPath::into_path)
                    .pipe(|output_archive_path| {
                        let archive_path = self.path.0.into_path().normalize();
                        scoped_temp_file()
                            .and_then(|mut buffer| {
                                std::io::copy(from_reader, &mut buffer)
                                    .context("copying into buffer")
                                    .map(|_| buffer)
                            })
                            .map(|buffer| buffer.into_temp_path())
                            .map(|buffer| {
                                repacking_context
                                    .queued_archives
                                    .lock()
                                    .entry(output_archive_path)
                                    .or_insert_with(|| LazyArchive::new(&write_archive.inner))
                                    .insert(archive_path, buffer);
                            })
                    }),
            })
    }
    fn into_reader(self, context: RepackingContext) -> Result<Box<dyn Read>> {
        context
            .locations
            .get(&self.location)
            .with_context(|| format!("no location for {self:#?}"))
            .inspect(|location| tracing::debug!("{location:#?}"))
            .and_then(|location| {
                (match location {
                    Location::Folder(folder) => folder
                        .inner
                        .value
                        .clone()
                        .pipe(MaybeWindowsPath)
                        .pipe(MaybeWindowsPath::into_path)
                        .pipe(|path| path.join(self.path.0.into_path()).normalize())
                        .pipe(|source| {
                            source
                                .open_file_read()
                                .map(|(_, file)| Box::new(file) as Box<dyn Read>)
                        }),
                    Location::ReadArchive(WithKindGuard {
                        inner: ReadArchiveLocation { name: _, value },
                        ..
                    }) => {
                        let value = MaybeWindowsPath(value.clone()).into_path().normalize();
                        crate::compression::ArchiveHandle::with_guessed(value.as_path(), value.extension(), |mut archive| {
                            archive.get_handle(&self.path.clone().0.into_path())
                        })
                        .map(|handle| Box::new(handle) as Box<dyn Read>)
                    }
                    Location::WriteArchive(write_archive) => anyhow::bail!("cannot write into this, right? => Location::WriteArchive({write_archive:#?})"),
                })
                .with_context(|| format!("when converting location into reader:\n[{location:#?}]"))
            })
    }
}

#[instrument(skip_all)]
pub fn install(CliConfig { contains }: CliConfig, hoolamike_config: HoolamikeConfig) -> Result<()> {
    let ExtensionConfig {
        path_to_ttw_mpi_file,
        variables: ttw_config_variables,
    } = hoolamike_config
        .extras
        .as_ref()
        .and_then(|extras| extras.tale_of_two_wastelands.as_ref())
        .context("no tale of two wastelands configured in hoolamike.yaml")?;

    let manifest_file::Manifest {
        package,
        variables,
        locations,
        tags: _,
        checks: _,
        file_attrs: _,
        post_commands: _,
        assets,
    } = crate::compression::bethesda_archive::BethesdaArchive::open(path_to_ttw_mpi_file)
        .and_then(|mut archive| {
            archive
                .get_handle(Path::new(MANIFEST_PATH))
                .context("extracting the manifest out of MPI file")
        })
        .map(BufReader::new)
        .and_then(|reader| {
            String::new()
                .pipe(|mut out| {
                    info_span!("extracing_manifest")
                        .wrap_read(0, reader)
                        .read_to_string(&mut out)
                        .map(|_| out)
                        .context("extracting")
                })
                .and_then(|manifest| serde_json::from_str::<manifest_file::Manifest>(&manifest).context("parsing"))
                .context("parsing extracted manifest file")
        })
        .with_context(|| format!("extracting manifest out of [{path_to_ttw_mpi_file:?}]"))?;
    info!(package=%serde_json::to_string_pretty(&package).unwrap_or_else(|e| format!("[{e:#?}]")), "got manifest file");

    let _span = info_span!(
        "installing_ttw",
        version=%package.version,
        title=%package.title,
    )
    .entered();
    let variables = variables
        .release()
        .into_iter()
        .map(|variable| (variable.name().to_string(), variable))
        .collect::<BTreeMap<_, _>>();

    let variables_context = VariablesContext {
        variables,
        ttw_config_variables: ttw_config_variables.clone(),
        hoolamike_installation_config: hoolamike_config.clone(),
    };
    let locations = locations
        .release()
        .into_iter()
        .enumerate()
        .map(|(idx, mut location)| {
            idx.to_u8()
                .context("too many assets")
                .map(LocationIndex)
                .and_then(|idx| {
                    variables_context
                        .resolve_variable(location.value_mut())
                        .map(|resolved| (idx, location.tap_mut(|location| *location.value_mut() = resolved.to_string())))
                })
        })
        .collect::<Result<BTreeMap<LocationIndex, Location>>>()
        .context("collecting locations")?;

    let contains = Arc::new(contains);
    let assets = match contains.is_empty() {
        true => assets,
        false => assets
            .into_par_iter()
            .filter(|a| {
                serde_json::to_string(a)
                    .map(|text| contains.iter().all(|phrase| text.contains(phrase)))
                    .tap_err(|error| warn!(?error, "serializiation failed?"))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>(),
    };
    let asset_count = assets.len() as u64;
    let handling_assets = info_span!("handling_assets").tap(|pb| {
        pb.pb_set_style(&count_progress_style());
        pb.pb_set_length(asset_count);
    });

    let repacking_context = RepackingContext::new(locations.pipe(Arc::new));
    let asset_context = handle_asset::AssetContext {
        path_to_ttw_mpi_file: Arc::from(path_to_ttw_mpi_file.as_path()),
        repacking_context: repacking_context.clone(),
    };

    handling_assets
        .clone()
        .in_scope(|| {
            assets
                .into_par_iter()
                .inspect(move |_| handling_assets.pb_inc(1))
                .try_for_each({
                    let asset_context = asset_context.clone();
                    move |asset| {
                        info_span!("handling_asset", kind=?manifest_file::asset::AssetRawKind::from(&asset), asset=%asset.name()).in_scope(|| {
                            asset_context
                                .clone()
                                .handle_asset(asset.clone())
                                .with_context(|| format!("handling [{asset:#?}]"))
                                .inspect(|_| info!("[OK]"))
                        })
                    }
                })
                .context("executing asset operations")
                .map(move |_| {
                    repacking_context
                        .queued_archives
                        .lock()
                        .pipe_deref_mut(std::mem::take)
                })
                .and_then(|archives| {
                    let building_archives = info_span!("building_archives").tap_mut(|pb| {
                        pb.pb_set_style(&count_progress_style());
                        pb.pb_set_length(archives.len() as _);
                    });
                    building_archives.clone().in_scope(|| {
                        archives
                            .into_iter()
                            .inspect(|_| building_archives.pb_inc(1))
                            .try_for_each(|(_, descriptor)| {
                                build_bsa::build_bsa(descriptor, |archive, options, output_path| {
                                    output_path
                                        .into_path()
                                        .normalize()
                                        .open_file_write()
                                        .and_then(|(output_path, output)| {
                                            archive
                                                .write(&mut tracing::Span::current().wrap_write(0, output), &options)
                                                .with_context(|| format!("writing built bsa file to {output_path:?}"))
                                                .tap_ok(|_| info!(?output_path, "[OK]"))
                                        })
                                })
                            })
                    })
                })
        })
        .tap_ok(|_| {
            let Package {
                title,
                version,
                author,
                home_page,
                description,
                gui: _,
            } = package;
            info!(%title);
            info!(%version);
            info!(%author);
            info!(%description);
            info!(%home_page);
            info!("☢️ :: succesfully installed [{asset_count}] assets :: ☢️");
        })
}

mod build_bsa;
mod handle_asset;
