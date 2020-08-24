use super::DocBuilder;
use crate::db::blacklist::is_blacklisted;
use crate::db::file::add_path_into_database;
use crate::db::{
    add_build_into_database, add_doc_coverage, add_package_into_database,
    update_crate_data_in_database, Pool,
};
use crate::docbuilder::{crates::crates_from_path, Limits};
use crate::error::Result;
use crate::index::api::ReleaseData;
use crate::storage::CompressionAlgorithms;
use crate::utils::{copy_doc_dir, parse_rustc_version, CargoMetadata};
use crate::{Metrics, Storage};
use docsrs_metadata::{Metadata, DEFAULT_TARGETS, HOST_TARGET};
use failure::ResultExt;
use log::{debug, info, warn, LevelFilter};
use rustwide::cmd::{Command, SandboxBuilder};
use rustwide::logging::{self, LogStorage};
use rustwide::toolchain::ToolchainError;
use rustwide::{Build, Crate, Toolchain, Workspace, WorkspaceBuilder};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

const USER_AGENT: &str = "docs.rs builder (https://github.com/rust-lang/docs.rs)";
const DEFAULT_RUSTWIDE_WORKSPACE: &str = ".rustwide";
const ESSENTIAL_FILES_VERSIONED: &[&str] = &[
    "brush.svg",
    "wheel.svg",
    "down-arrow.svg",
    "dark.css",
    "light.css",
    "ayu.css",
    "main.js",
    "normalize.css",
    "rustdoc.css",
    "settings.css",
    "settings.js",
    "storage.js",
    "theme.js",
    "source-script.js",
    "noscript.css",
    "rust-logo.png",
];
const ESSENTIAL_FILES_UNVERSIONED: &[&str] = &[
    "FiraSans-Medium.woff",
    "FiraSans-Regular.woff",
    "SourceCodePro-Regular.woff",
    "SourceCodePro-Semibold.woff",
    "SourceSerifPro-Bold.ttf.woff",
    "SourceSerifPro-Regular.ttf.woff",
    "SourceSerifPro-It.ttf.woff",
];

const DUMMY_CRATE_NAME: &str = "empty-library";
const DUMMY_CRATE_VERSION: &str = "1.0.0";

pub struct RustwideBuilder {
    workspace: Workspace,
    toolchain: Toolchain,
    db: Pool,
    storage: Arc<Storage>,
    metrics: Arc<Metrics>,
    rustc_version: String,
    cpu_limit: Option<u32>,
}

impl RustwideBuilder {
    pub fn init(db: Pool, metrics: Arc<Metrics>, storage: Arc<Storage>) -> Result<Self> {
        use rustwide::cmd::SandboxImage;
        let env_workspace_path = ::std::env::var("CRATESFYI_RUSTWIDE_WORKSPACE");
        let workspace_path = env_workspace_path
            .as_ref()
            .map(|v| v.as_str())
            .unwrap_or(DEFAULT_RUSTWIDE_WORKSPACE);
        let is_docker = std::env::var("DOCS_RS_DOCKER")
            .map(|s| s == "true")
            .unwrap_or(false);
        let mut builder = WorkspaceBuilder::new(Path::new(workspace_path), USER_AGENT)
            .running_inside_docker(is_docker);
        if let Ok(custom_image) = std::env::var("DOCS_RS_LOCAL_DOCKER_IMAGE") {
            builder = builder.sandbox_image(SandboxImage::local(&custom_image)?);
        }

        let workspace = builder.init()?;
        workspace.purge_all_build_dirs()?;

        let toolchain_name = std::env::var("CRATESFYI_TOOLCHAIN")
            .map(Cow::Owned)
            .unwrap_or_else(|_| Cow::Borrowed("nightly"));

        let cpu_limit = std::env::var("DOCS_RS_BUILD_CPU_LIMIT").ok().map(|limit| {
            limit
                .parse::<u32>()
                .expect("invalid DOCS_RS_BUILD_CPU_LIMIT")
        });

        let toolchain = Toolchain::dist(&toolchain_name);

        Ok(RustwideBuilder {
            workspace,
            toolchain,
            db,
            storage,
            metrics,
            rustc_version: String::new(),
            cpu_limit,
        })
    }

    fn prepare_sandbox(&self, limits: &Limits) -> SandboxBuilder {
        SandboxBuilder::new()
            .cpu_limit(self.cpu_limit.map(|limit| limit as f32))
            .memory_limit(Some(limits.memory()))
            .enable_networking(limits.networking())
    }

    pub fn update_toolchain(&mut self) -> Result<()> {
        // Ignore errors if detection fails.
        let old_version = self.detect_rustc_version().ok();

        let mut targets_to_install = DEFAULT_TARGETS
            .iter()
            .map(|&t| t.to_string()) // &str has a specialized ToString impl, while &&str goes through Display
            .collect::<HashSet<_>>();

        let installed_targets = match self.toolchain.installed_targets(&self.workspace) {
            Ok(targets) => targets,
            Err(err) => {
                if let Some(&ToolchainError::NotInstalled) = err.downcast_ref::<ToolchainError>() {
                    Vec::new()
                } else {
                    return Err(err);
                }
            }
        };

        // The extra targets are intentionally removed *before* trying to update.
        //
        // If a target is installed locally and it goes missing the next update, rustup will block
        // the update to avoid leaving the system in a broken state. This is not a behavior we want
        // though when we also remove the target from the list managed by docs.rs: we want that
        // target gone, and we don't care if it's missing in the next update.
        //
        // Removing it beforehand works fine, and prevents rustup from blocking the update later in
        // the method.
        //
        // Note that this means that non tier-one targets will be uninstalled on every update,
        // and will not be reinstalled until explicitly requested by a crate.
        for target in installed_targets {
            if !targets_to_install.remove(&target) {
                self.toolchain.remove_target(&self.workspace, &target)?;
            }
        }

        self.toolchain.install(&self.workspace)?;

        for target in &targets_to_install {
            self.toolchain.add_target(&self.workspace, target)?;
        }
        // NOTE: rustup will automatically refuse to update the toolchain
        // if `rustfmt` is not available in the newer version
        // NOTE: this ignores the error so that you can still run a build without rustfmt.
        // This should only happen if you run a build for the first time when rustfmt isn't available.
        if let Err(err) = self.toolchain.add_component(&self.workspace, "rustfmt") {
            log::warn!("failed to install rustfmt: {}", err);
            log::info!("continuing anyway, since this must be the first build");
        }

        self.rustc_version = self.detect_rustc_version()?;
        if old_version.as_deref() != Some(&self.rustc_version) {
            self.add_essential_files()?;
        }

        Ok(())
    }

    fn detect_rustc_version(&self) -> Result<String> {
        info!("detecting rustc's version...");
        let res = Command::new(&self.workspace, self.toolchain.rustc())
            .args(&["--version"])
            .log_output(false)
            .run_capture()?;
        let mut iter = res.stdout_lines().iter();
        if let (Some(line), None) = (iter.next(), iter.next()) {
            info!("found rustc {}", line);
            Ok(line.clone())
        } else {
            Err(::failure::err_msg(
                "invalid output returned by `rustc --version`",
            ))
        }
    }

    pub fn add_essential_files(&mut self) -> Result<()> {
        self.rustc_version = self.detect_rustc_version()?;
        let rustc_version = parse_rustc_version(&self.rustc_version)?;

        info!("building a dummy crate to get essential files");

        let mut conn = self.db.get()?;
        let limits = Limits::for_crate(&mut conn, DUMMY_CRATE_NAME)?;

        let mut build_dir = self
            .workspace
            .build_dir(&format!("essential-files-{}", rustc_version));
        build_dir.purge()?;

        // This is an empty library crate that is supposed to always build.
        let krate = Crate::crates_io(DUMMY_CRATE_NAME, DUMMY_CRATE_VERSION);
        krate.fetch(&self.workspace)?;

        build_dir
            .build(&self.toolchain, &krate, self.prepare_sandbox(&limits))
            .run(|build| {
                let metadata = Metadata::from_crate_root(&build.host_source_dir())?;

                let res = self.execute_build(HOST_TARGET, true, build, &limits, &metadata)?;
                if !res.result.successful {
                    failure::bail!("failed to build dummy crate for {}", self.rustc_version);
                }

                info!("copying essential files for {}", self.rustc_version);
                let source = build.host_target_dir().join("doc");
                let dest = tempfile::Builder::new()
                    .prefix("essential-files")
                    .tempdir()?;

                let files = ESSENTIAL_FILES_VERSIONED
                    .iter()
                    .map(|f| (f, true))
                    .chain(ESSENTIAL_FILES_UNVERSIONED.iter().map(|f| (f, false)));
                for (&file, versioned) in files {
                    let segments = file.rsplitn(2, '.').collect::<Vec<_>>();
                    let file_name = if versioned {
                        format!("{}-{}.{}", segments[1], rustc_version, segments[0])
                    } else {
                        file.to_string()
                    };
                    let source_path = source.join(&file_name);
                    let dest_path = dest.path().join(&file_name);
                    ::std::fs::copy(&source_path, &dest_path).with_context(|_| {
                        format!(
                            "couldn't copy '{}' to '{}'",
                            source_path.display(),
                            dest_path.display()
                        )
                    })?;
                }

                add_path_into_database(&self.storage, "", &dest)?;
                conn.query(
                    "INSERT INTO config (name, value) VALUES ('rustc_version', $1) \
                     ON CONFLICT (name) DO UPDATE SET value = $1;",
                    &[&Value::String(self.rustc_version.clone())],
                )?;

                Ok(())
            })?;

        build_dir.purge()?;
        krate.purge_from_cache(&self.workspace)?;
        Ok(())
    }

    pub fn build_world(&mut self, doc_builder: &mut DocBuilder) -> Result<()> {
        let mut count = 0;
        crates_from_path(
            &doc_builder.options().registry_index_path.clone(),
            &mut |name, version| {
                match self.build_package(doc_builder, name, version, None) {
                    Ok(status) => {
                        count += 1;
                        if status && count % 10 == 0 {
                            let _ = doc_builder.save_cache();
                        }
                    }
                    Err(err) => warn!("failed to build package {} {}: {}", name, version, err),
                }
                doc_builder.add_to_cache(name, version);
            },
        )
    }

    pub fn build_local_package(
        &mut self,
        doc_builder: &mut DocBuilder,
        path: &Path,
    ) -> Result<bool> {
        self.update_toolchain()?;
        let metadata =
            CargoMetadata::load(&self.workspace, &self.toolchain, path).map_err(|err| {
                err.context(format!("failed to load local package {}", path.display()))
            })?;
        let package = metadata.root();
        self.build_package(doc_builder, &package.name, &package.version, Some(path))
    }

    pub fn build_package(
        &mut self,
        doc_builder: &mut DocBuilder,
        name: &str,
        version: &str,
        local: Option<&Path>,
    ) -> Result<bool> {
        if !doc_builder.should_build(name, version) {
            return Ok(false);
        }

        self.update_toolchain()?;

        info!("building package {} {}", name, version);

        let mut conn = self.db.get()?;

        if is_blacklisted(&mut conn, name)? {
            info!("skipping build of {}, crate has been blacklisted", name);
            return Ok(false);
        }

        let limits = Limits::for_crate(&mut conn, name)?;

        let mut build_dir = self.workspace.build_dir(&format!("{}-{}", name, version));
        build_dir.purge()?;

        let krate = if let Some(path) = local {
            Crate::local(path)
        } else {
            Crate::crates_io(name, version)
        };
        krate.fetch(&self.workspace)?;

        let local_storage = tempfile::Builder::new().prefix("docsrs-docs").tempdir()?;

        let res = build_dir
            .build(&self.toolchain, &krate, self.prepare_sandbox(&limits))
            .run(|build| {
                use docsrs_metadata::BuildTargets;

                let mut has_docs = false;
                let mut successful_targets = Vec::new();
                let metadata = Metadata::from_crate_root(&build.host_source_dir())?;
                let BuildTargets {
                    default_target,
                    other_targets,
                } = metadata.targets();

                // Perform an initial build
                let res = self.execute_build(default_target, true, &build, &limits, &metadata)?;
                if res.result.successful {
                    if let Some(name) = res.cargo_metadata.root().library_name() {
                        let host_target = build.host_target_dir();
                        has_docs = host_target.join("doc").join(name).is_dir();
                    }
                }

                let mut algs = HashSet::new();
                if has_docs {
                    debug!("adding documentation for the default target to the database");
                    self.copy_docs(&build.host_target_dir(), local_storage.path(), "", true)?;

                    successful_targets.push(res.target.clone());

                    // Then build the documentation for all the targets
                    // Limit the number of targets so that no one can try to build all 200000 possible targets
                    for target in other_targets.into_iter().take(limits.targets()) {
                        debug!("building package {} {} for {}", name, version, target);
                        self.build_target(
                            target,
                            &build,
                            &limits,
                            &local_storage.path(),
                            &mut successful_targets,
                            &metadata,
                        )?;
                    }
                    let new_algs = self.upload_docs(name, version, local_storage.path())?;
                    algs.extend(new_algs);
                };

                // Store the sources even if the build fails
                debug!("adding sources into database");
                let prefix = format!("sources/{}/{}", name, version);
                let (files_list, new_algs) =
                    add_path_into_database(&self.storage, &prefix, build.host_source_dir())?;
                algs.extend(new_algs);

                let has_examples = build.host_source_dir().join("examples").is_dir();
                if res.result.successful {
                    self.metrics.successful_builds.inc();
                } else if res.cargo_metadata.root().is_library() {
                    self.metrics.failed_builds.inc();
                } else {
                    self.metrics.non_library_builds.inc();
                }

                let release_data = match doc_builder.index.api().get_release_data(name, version) {
                    Ok(data) => data,
                    Err(err) => {
                        warn!("{:#?}", err);
                        ReleaseData::default()
                    }
                };

                let release_id = add_package_into_database(
                    &mut conn,
                    res.cargo_metadata.root(),
                    &build.host_source_dir(),
                    &res.result,
                    &res.target,
                    files_list,
                    successful_targets,
                    &release_data,
                    has_docs,
                    has_examples,
                    algs,
                )?;

                if let Some(doc_coverage) = res.result.doc_coverage {
                    add_doc_coverage(&mut conn, release_id, doc_coverage)?;
                }

                add_build_into_database(&mut conn, release_id, &res.result)?;

                // Some crates.io crate data is mutable, so we proactively update it during a release
                match doc_builder.index.api().get_crate_data(name) {
                    Ok(crate_data) => update_crate_data_in_database(&mut conn, name, &crate_data)?,
                    Err(err) => warn!("{:#?}", err),
                }

                doc_builder.add_to_cache(name, version);
                Ok(res)
            })?;

        build_dir.purge()?;
        krate.purge_from_cache(&self.workspace)?;
        local_storage.close()?;
        Ok(res.result.successful)
    }

    fn build_target(
        &self,
        target: &str,
        build: &Build,
        limits: &Limits,
        local_storage: &Path,
        successful_targets: &mut Vec<String>,
        metadata: &Metadata,
    ) -> Result<()> {
        let target_res = self.execute_build(target, false, build, limits, metadata)?;
        if target_res.result.successful {
            // Cargo is not giving any error and not generating documentation of some crates
            // when we use a target compile options. Check documentation exists before
            // adding target to successfully_targets.
            if build.host_target_dir().join(target).join("doc").is_dir() {
                debug!("adding documentation for target {} to the database", target,);
                self.copy_docs(&build.host_target_dir(), local_storage, target, false)?;
                successful_targets.push(target.to_string());
            }
        }
        Ok(())
    }

    fn get_coverage(
        &self,
        target: &str,
        build: &Build,
        metadata: &Metadata,
        limits: &Limits,
    ) -> Result<Option<DocCoverage>> {
        let rustdoc_flags = vec![
            "--output-format".to_string(),
            "json".to_string(),
            "--show-coverage".to_string(),
        ];

        #[derive(serde::Deserialize)]
        struct FileCoverage {
            total: i32,
            with_docs: i32,
        }

        let mut coverage = DocCoverage {
            total_items: 0,
            documented_items: 0,
        };

        self.prepare_command(build, target, metadata, limits, rustdoc_flags)?
            .process_lines(&mut |line, _| {
                if line.starts_with('{') && line.ends_with('}') {
                    let parsed = match serde_json::from_str::<HashMap<String, FileCoverage>>(line) {
                        Ok(parsed) => parsed,
                        Err(_) => return,
                    };
                    for file in parsed.values() {
                        coverage.total_items += file.total;
                        coverage.documented_items += file.with_docs;
                    }
                }
            })
            .log_output(false)
            .run()?;

        Ok(
            if coverage.total_items == 0 && coverage.documented_items == 0 {
                None
            } else {
                Some(coverage)
            },
        )
    }

    fn execute_build(
        &self,
        target: &str,
        is_default_target: bool,
        build: &Build,
        limits: &Limits,
        metadata: &Metadata,
    ) -> Result<FullBuildResult> {
        let cargo_metadata =
            CargoMetadata::load(&self.workspace, &self.toolchain, &build.host_source_dir())?;

        let mut rustdoc_flags = Vec::new();

        for dep in &cargo_metadata.root_dependencies() {
            rustdoc_flags.push("--extern-html-root-url".to_string());
            rustdoc_flags.push(format!(
                "{}=https://docs.rs/{}/{}",
                dep.name.replace("-", "_"),
                dep.name,
                dep.version
            ));
        }

        rustdoc_flags.extend(vec![
            "--resource-suffix".to_string(),
            format!("-{}", parse_rustc_version(&self.rustc_version)?),
        ]);

        let mut storage = LogStorage::new(LevelFilter::Info);
        storage.set_max_size(limits.max_log_size());

        let successful = logging::capture(&storage, || {
            self.prepare_command(build, target, metadata, limits, rustdoc_flags)
                .and_then(|command| command.run().map_err(failure::Error::from))
                .is_ok()
        });
        let doc_coverage = if successful {
            self.get_coverage(target, build, metadata, limits)?
        } else {
            None
        };
        // If we're passed a default_target which requires a cross-compile,
        // cargo will put the output in `target/<target>/doc`.
        // However, if this is the default build, we don't want it there,
        // we want it in `target/doc`.
        if target != HOST_TARGET && is_default_target {
            // mv target/$target/doc target/doc
            let target_dir = build.host_target_dir();
            let old_dir = target_dir.join(target).join("doc");
            let new_dir = target_dir.join("doc");
            debug!("rename {} to {}", old_dir.display(), new_dir.display());
            std::fs::rename(old_dir, new_dir)?;
        }

        Ok(FullBuildResult {
            result: BuildResult {
                build_log: storage.to_string(),
                rustc_version: self.rustc_version.clone(),
                docsrs_version: format!("docsrs {}", crate::BUILD_VERSION),
                successful,
                doc_coverage,
            },
            cargo_metadata,
            target: target.to_string(),
        })
    }

    fn prepare_command<'ws, 'pl>(
        &self,
        build: &'ws Build,
        target: &str,
        metadata: &Metadata,
        limits: &Limits,
        rustdoc_flags_extras: Vec<String>,
    ) -> Result<Command<'ws, 'pl>> {
        // If the explicit target is not a tier one target, we need to install it.
        if !docsrs_metadata::DEFAULT_TARGETS.contains(&target) {
            // This is a no-op if the target is already installed.
            self.toolchain.add_target(&self.workspace, target)?;
        }

        let mut cargo_args = metadata.cargo_args();

        // Add docs.rs specific arguments
        if let Some(cpu_limit) = self.cpu_limit {
            cargo_args.push(format!("-j{}", cpu_limit));
        }
        if target != HOST_TARGET {
            cargo_args.push("--target".into());
            cargo_args.push(target.into());
        };

        let mut env_vars = metadata.environment_variables();
        let rustdoc_flags = env_vars.entry("RUSTDOCFLAGS").or_default();
        rustdoc_flags.push_str(" -Z unstable-options --static-root-path / --cap-lints warn ");
        rustdoc_flags.push_str(&rustdoc_flags_extras.join(" "));

        let mut command = build
            .cargo()
            .timeout(Some(limits.timeout()))
            .no_output_timeout(None);
        for (key, val) in env_vars {
            command = command.env(key, val);
        }

        Ok(command.args(&cargo_args))
    }

    fn copy_docs(
        &self,
        target_dir: &Path,
        local_storage: &Path,
        target: &str,
        is_default_target: bool,
    ) -> Result<()> {
        let source = target_dir.join(target).join("doc");

        let mut dest = local_storage.to_path_buf();
        // only add target name to destination directory when we are copying a non-default target.
        // this is allowing us to host documents in the root of the crate documentation directory.
        // for example winapi will be available in docs.rs/winapi/$version/winapi/ for it's
        // default target: x86_64-pc-windows-msvc. But since it will be built under
        // cratesfyi/x86_64-pc-windows-msvc we still need target in this function.
        if !is_default_target {
            dest = dest.join(target);
        }

        info!("{} {}", source.display(), dest.display());
        copy_doc_dir(source, dest)
    }

    fn upload_docs(
        &self,
        name: &str,
        version: &str,
        local_storage: &Path,
    ) -> Result<CompressionAlgorithms> {
        debug!("Adding documentation into database");
        add_path_into_database(
            &self.storage,
            &format!("rustdoc/{}/{}", name, version),
            local_storage,
        )
        .map(|t| t.1)
    }
}

struct FullBuildResult {
    result: BuildResult,
    target: String,
    cargo_metadata: CargoMetadata,
}

#[derive(Clone, Copy)]
pub(crate) struct DocCoverage {
    /// The total items that could be documented in the current crate, used to calculate
    /// documentation coverage.
    pub(crate) total_items: i32,
    /// The items of the crate that are documented, used to calculate documentation coverage.
    pub(crate) documented_items: i32,
}

pub(crate) struct BuildResult {
    pub(crate) rustc_version: String,
    pub(crate) docsrs_version: String,
    pub(crate) build_log: String,
    pub(crate) successful: bool,
    pub(crate) doc_coverage: Option<DocCoverage>,
}
