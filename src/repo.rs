use crate::expander::BumpCache;
use crate::expander::TopRepoExpander;
use crate::git::BlobId;
use crate::git::CommitId;
use crate::git::GitModulesInfo;
use crate::git::GitPath;
use crate::git::TreeId;
use crate::git::git_command;
use crate::git::git_global_command;
use crate::git_fast_export_import::ChangedFile;
use crate::git_fast_export_import::FastImportCommit;
use crate::git_fast_export_import::ImportCommitRef;
use crate::git_fast_export_import::WithoutCommitterId;
use crate::git_fast_export_import_dedup::GitFastExportImportDedupCache;
use crate::gitmodules::SubmoduleUrlExt as _;
use crate::log::Logger;
use crate::repo_name::RepoName;
use crate::repo_name::SubRepoName;
use crate::util::CommandExtension as _;
use crate::util::EMPTY_GIX_URL;
use crate::util::RcKey;
use anyhow::Context;
use anyhow::Result;
use bstr::BStr;
use bstr::ByteSlice as _;
use gix::refs::FullName;
use gix::refs::FullNameRef;
use gix::refs::file::ReferenceExt;
use itertools::Itertools;
use serde_with::serde_as;
use std::borrow::Borrow as _;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Display;
use std::hash::Hash;
use std::ops::Deref;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Debug)]
pub struct TopRepo {
    pub directory: PathBuf,
    pub gix_repo: gix::ThreadSafeRepository,
    pub url: gix::url::Url,
}

impl TopRepo {
    pub fn create(directory: PathBuf, url: gix::url::Url) -> Result<TopRepo> {
        git_global_command()
            .arg("init")
            .arg("--quiet")
            .arg(directory.as_os_str())
            .safe_status()?
            .check_success()
            .context("Failed to initialize git repository")?;
        git_command(&directory)
            .args([
                "config",
                "remote.origin.pushUrl",
                "https://ERROR.invalid/Please use 'git toprepo push ...' instead",
            ])
            .safe_status()?
            .check_success()
            .context("Failed to set git-config remote.origin.pushUrl")?;
        git_command(&directory)
            .args(["config", "remote.origin.url", &url.to_string()])
            .safe_status()?
            .check_success()
            .context("Failed to set git-config remote.origin.url")?;
        let toprepo_ref_prefix: String = RepoName::Top.to_ref_prefix();
        git_command(&directory)
            .args([
                "config",
                "--replace-all",
                "remote.origin.fetch",
                &format!("+refs/heads/*:{toprepo_ref_prefix}refs/heads/*"),
            ])
            .safe_status()?
            .check_success()
            .context("Failed to set git-config remote.origin.fetch (heads)")?;
        git_command(&directory)
            .args([
                "config",
                "--add",
                "remote.origin.fetch",
                &format!("+refs/tags/*:{toprepo_ref_prefix}refs/tags/*"),
            ])
            .safe_status()?
            .check_success()
            .context("Failed to set git-config remote.origin.fetch (tags)")?;
        git_command(&directory)
            .args([
                "config",
                "--add",
                "remote.origin.fetch",
                &format!("+HEAD:{toprepo_ref_prefix}HEAD"),
            ])
            .safe_status()?
            .check_success()
            .context("Failed to set git-config remote.origin.fetch (HEAD)")?;
        git_command(&directory)
            .args(["config", "remote.origin.tagOpt", "--no-tags"])
            .safe_status()?
            .check_success()
            .context("Failed to set git-config remote.origin.tagOpt")?;
        git_command(&directory)
            .args([
                "config",
                "toprepo.config",
                "repo:refs/remotes/origin/HEAD:.gittoprepo.toml",
            ])
            .safe_status()?
            .check_success()
            .context("Failed to set git-config remote.origin.url")?;
        git_command(&directory)
            .args(["symbolic-ref", "HEAD", "refs/remotes/origin/HEAD"])
            .safe_status()?
            .check_success()
            .context("Failed to reset HEAD")?;
        Self::open(directory)
    }

    pub fn open(directory: PathBuf) -> Result<TopRepo> {
        let gix_repo = gix::open(&directory)?;
        let url = crate::git::get_default_remote_url(&gix_repo)?;
        Ok(TopRepo {
            directory,
            gix_repo: gix_repo.into_sync(),
            url,
        })
    }

    pub fn fetch_toprepo(&self) -> Result<()> {
        git_command(&self.directory)
            .arg("fetch")
            .arg("--recurse-submodules=false")
            .safe_status()?
            .check_success()?;
        Ok(())
    }

    pub fn fetch_toprepo_quiet(&self) -> Result<()> {
        git_command(&self.directory)
            .arg("fetch")
            .arg("--recurse-submodules=false")
            .arg("--quiet")
            .safe_status()?
            .check_success()?;
        Ok(())
    }

    pub fn refilter(
        &self,
        storage: &mut TopRepoCache,
        config: &crate::config::GitTopRepoConfig,
        logger: Logger,
        progress: indicatif::MultiProgress,
    ) -> Result<()> {
        let repo = self.gix_repo.to_thread_local();

        let old_origin_refs = repo
            .references()?
            .prefixed(b"refs/remotes/origin/".as_bstr())?
            .map_ok(|r| {
                let r = r.detach();
                (r.name.clone(), r)
            })
            .collect::<std::result::Result<HashMap<_, _>, _>>()
            .map_err(|err| {
                anyhow::anyhow!("Failed while iterating refs/remotes/origin/: {err:#}")
            })?;

        let ref_prefix = RepoName::Top.to_ref_prefix();
        let mut new_origin_ref_names = HashSet::new();
        let mut toprepo_symbolic_tips = Vec::new();
        let mut toprepo_object_tip_names = Vec::new();
        let mut toprepo_object_tip_ids = Vec::new();
        for r in repo
            .references()?
            .prefixed(BStr::new(ref_prefix.as_bytes()))?
        {
            let r = r.map_err(|err| anyhow::anyhow!("Failed while iterating refs: {err:#}"))?;
            let r_target = r.clone().follow_to_object().with_context(|| {
                format!("Failed to resolve symbolic ref {}", r.name().as_bstr())
            })?;
            match r_target.object()?.kind {
                gix::object::Kind::Commit => {}
                gix::object::Kind::Tag => {}
                gix::object::Kind::Tree => {
                    logger.warning(format!(
                        "Skipping ref {} that points to a tree",
                        r.name().as_bstr()
                    ));
                    continue;
                }
                gix::object::Kind::Blob => {
                    logger.warning(format!(
                        "Skipping ref {} that points to a blob",
                        r.name().as_bstr()
                    ));
                    continue;
                }
            }
            let r = r.detach();
            new_origin_ref_names.insert(TopRepoExpander::input_ref_to_output_ref(r.name.borrow())?);
            match r.target {
                gix::refs::Target::Symbolic(target_name) => {
                    toprepo_symbolic_tips.push((r.name, target_name));
                }
                gix::refs::Target::Object(object_id) => {
                    toprepo_object_tip_names.push(r.name);
                    toprepo_object_tip_ids.push(TopRepoCommitId(object_id));
                }
            }
        }
        let mut unknown_toprepo_tips = toprepo_object_tip_ids
            .into_iter()
            .filter(|commit_id| !storage.top_to_mono_map.contains_key(commit_id))
            .peekable();
        if unknown_toprepo_tips.peek().is_some() {
            let progress = progress.clone();
            let pb = progress.add(
                indicatif::ProgressBar::no_length()
                    .with_style(
                        indicatif::ProgressStyle::default_spinner()
                            .template("{elapsed:>4} {msg} {pos}")
                            .unwrap(),
                    )
                    .with_message("Looking for new commits to expand"),
            );
            let (stop_commits, num_commits_to_export) = crate::git::get_first_known_commits(
                &repo,
                unknown_toprepo_tips.map(|commit_id| commit_id.into_inner()),
                |commit_id| {
                    storage
                        .top_to_mono_map
                        .contains_key(&TopRepoCommitId(commit_id))
                },
                &pb,
            )?;
            drop(pb);

            println!("Found {num_commits_to_export} commits to expand");
            let fast_importer = crate::git_fast_export_import::FastImportRepo::new(
                self.gix_repo.git_dir(),
                logger.clone(),
            )?;
            let mut expander = TopRepoExpander {
                gix_repo: &repo,
                storage,
                config,
                progress,
                logger: logger.clone(),
                fast_importer,
                imported_commits: HashMap::new(),
                bumps: crate::expander::BumpCache::default(),
                inject_at_oldest_super_commit: false,
            };

            expander.expand_toprepo_commits(
                toprepo_object_tip_names,
                stop_commits,
                num_commits_to_export,
            )?;
            expander.wait()?;

            Self::update_refs(
                &repo,
                &logger,
                toprepo_symbolic_tips,
                old_origin_refs,
                new_origin_ref_names,
            )?;
        }
        Ok(())
    }

    fn update_refs(
        repo: &gix::Repository,
        logger: &Logger,
        toprepo_symbolic_tips: Vec<(FullName, FullName)>,
        old_origin_refs: HashMap<FullName, gix::refs::Reference>,
        new_origin_ref_names: HashSet<FullName>,
    ) -> Result<()> {
        let mut ref_edits = Vec::new();
        // Update symbolic refs/remotes/origin/* if needed.
        for (top_link_name, top_target_name) in &toprepo_symbolic_tips {
            let origin_link_name =
                TopRepoExpander::input_ref_to_output_ref(top_link_name.borrow())?;
            let Ok(origin_target_name) =
                TopRepoExpander::input_ref_to_output_ref(top_target_name.borrow())
            else {
                logger.warning(format!(
                    "Skipping symbolic ref {} that points outside the top repo, to {}.",
                    top_link_name.as_bstr(),
                    top_target_name.as_bstr(),
                ));
                continue;
            };
            let new_target = gix::refs::Target::Symbolic(origin_target_name);
            let old_target = old_origin_refs.get(&origin_link_name).map(|r| &r.target);
            if old_target != Some(&new_target) {
                ref_edits.push(gix::refs::transaction::RefEdit {
                    change: gix::refs::transaction::Change::Update {
                        log: gix::refs::transaction::LogChange {
                            mode: gix::refs::transaction::RefLog::AndReference,
                            force_create_reflog: false,
                            message: b"git-toprepo filter".into(),
                        },
                        expected: old_target.cloned().map_or(
                            gix::refs::transaction::PreviousValue::MustNotExist,
                            gix::refs::transaction::PreviousValue::MustExistAndMatch,
                        ),
                        new: new_target,
                    },
                    name: origin_link_name,
                    deref: false,
                });
            }
        }
        // Remove refs/remote/origin/* references that were removed in refs/namespaces/top/*.
        for old_ref in old_origin_refs.into_values() {
            if new_origin_ref_names.contains(&old_ref.name) {
                continue;
            }
            logger.warning(format!(
                "Deleting now removed ref {}",
                old_ref.name.as_bstr()
            ));
            ref_edits.push(gix::refs::transaction::RefEdit {
                change: gix::refs::transaction::Change::Delete {
                    expected: gix::refs::transaction::PreviousValue::MustExistAndMatch(
                        old_ref.target,
                    ),
                    log: gix::refs::transaction::RefLog::AndReference,
                },
                name: old_ref.name,
                deref: false,
            });
        }
        // Apply the ref changes.
        if !ref_edits.is_empty() {
            repo.edit_references(ref_edits)
                .context("Failed to update all the refs/remotes/origin/* references")?;
        }
        Ok(())
    }

    pub fn expand_toprepo_refs(
        &self,
        refs: &Vec<FullName>,
        storage: &mut TopRepoCache,
        config: &crate::config::GitTopRepoConfig,
        logger: Logger,
        progress: indicatif::MultiProgress,
    ) -> Result<()> {
        let repo = self.gix_repo.to_thread_local();

        let mut toprepo_object_tip_names = Vec::new();
        let mut toprepo_object_tip_ids = Vec::new();
        for full_ref in refs {
            let r = repo.find_reference(full_ref)?;
            let r_target = r.clone().follow_to_object().with_context(|| {
                format!("Failed to resolve symbolic ref {}", r.name().as_bstr())
            })?;
            match r_target.object()?.kind {
                gix::object::Kind::Commit => {}
                gix::object::Kind::Tag => {}
                gix::object::Kind::Tree => {
                    logger.warning(format!(
                        "Skipping ref {} that points to a tree",
                        r.name().as_bstr()
                    ));
                    continue;
                }
                gix::object::Kind::Blob => {
                    logger.warning(format!(
                        "Skipping ref {} that points to a blob",
                        r.name().as_bstr()
                    ));
                    continue;
                }
            }
            let r = r.detach();
            match r.target {
                gix::refs::Target::Symbolic(target_name) => {
                    unimplemented!(
                        "symbolic refs in expand_toprepo_refs are not supported yet: {target_name}"
                    );
                }
                gix::refs::Target::Object(object_id) => {
                    toprepo_object_tip_names.push(r.name);
                    toprepo_object_tip_ids.push(TopRepoCommitId(object_id));
                }
            }
        }
        let progress = progress.clone();
        let pb = progress.add(
            indicatif::ProgressBar::no_length()
                .with_style(
                    indicatif::ProgressStyle::default_spinner()
                        .template("{elapsed:>4} {msg} {pos}")
                        .unwrap(),
                )
                .with_message("Looking for new commits to expand"),
        );
        let toprepo_object_tip_ids_set = toprepo_object_tip_ids
            .iter()
            .map(|commit_id| **commit_id)
            .collect::<HashSet<_>>();
        let (stop_commits, num_commits_to_export) = crate::git::get_first_known_commits(
            &repo,
            toprepo_object_tip_ids
                .into_iter()
                .map(|commit_id| commit_id.into_inner()),
            |commit_id| {
                !toprepo_object_tip_ids_set.contains(&commit_id)
                    && storage
                        .top_to_mono_map
                        .contains_key(&TopRepoCommitId(commit_id))
            },
            &pb,
        )?;
        drop(pb);

        println!("Found {num_commits_to_export} commits to expand");
        let fast_importer = crate::git_fast_export_import::FastImportRepo::new(
            self.gix_repo.git_dir(),
            logger.clone(),
        )?;
        let mut expander = TopRepoExpander {
            gix_repo: &repo,
            storage,
            config,
            progress,
            logger: logger.clone(),
            fast_importer,
            imported_commits: HashMap::new(),
            bumps: crate::expander::BumpCache::default(),
            inject_at_oldest_super_commit: false,
        };

        expander.expand_toprepo_commits(
            toprepo_object_tip_names,
            stop_commits,
            num_commits_to_export,
        )?;
        expander.wait()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn expand_submodule_ref_onto_head(
        &self,
        ref_to_inject: &FullNameRef,
        sub_repo_name: &SubRepoName,
        abs_sub_path: &GitPath,
        dest_ref: &FullNameRef,
        storage: &mut TopRepoCache,
        config: &crate::config::GitTopRepoConfig,
        logger: Logger,
        progress: indicatif::MultiProgress,
    ) -> Result<()> {
        let repo = self.gix_repo.to_thread_local();

        let mut ref_to_inject = repo.refs.find(ref_to_inject)?;
        let id_to_inject = ref_to_inject.peel_to_id_in_place(&repo.refs, &repo.objects)?;
        let thin_commit_to_inject = storage
            .repos
            .get(&RepoName::SubRepo(sub_repo_name.clone()))
            .and_then(|repo_data| repo_data.thin_commits.get(&id_to_inject))
            .with_context(|| {
                format!(
                    "Failed to find {}, commit {}",
                    ref_to_inject.name,
                    id_to_inject.to_hex()
                )
            })?
            .clone(); // Clone to avoid borrowing the `storage` object.

        let pb = progress.add(
            indicatif::ProgressBar::no_length()
                .with_style(
                    indicatif::ProgressStyle::default_spinner()
                        .template("{elapsed:>4} {msg} {pos}")
                        .unwrap(),
                )
                .with_message("Looking for mono commit to expand onto"),
        );
        // Hopefully, HEAD points to a commit.
        let head_id: gix::ObjectId = repo.head_id()?.detach();
        let mut possible_mono_parents = Vec::new();
        let (_possible_mono_parent_ids, _num_skipped_unknowns) =
            crate::git::get_first_known_commits(
                &repo,
                [head_id].into_iter(),
                |commit_id| {
                    let Some(mono_parent) =
                        storage.monorepo_commits.get(&MonoRepoCommitId(commit_id))
                    else {
                        return false;
                    };
                    possible_mono_parents.push(mono_parent.clone());
                    true
                },
                &pb,
            )?;
        drop(pb);

        let fast_importer = crate::git_fast_export_import::FastImportRepo::new(
            self.gix_repo.git_dir(),
            logger.clone(),
        )?;
        let mut expander = TopRepoExpander {
            gix_repo: &repo,
            storage,
            config,
            progress,
            logger: logger.clone(),
            fast_importer,
            imported_commits: HashMap::new(),
            bumps: crate::expander::BumpCache::default(),
            inject_at_oldest_super_commit: true,
        };
        let result = (|| {
            let Some(_mono_commit) = expander.inject_submodule_commit(
                dest_ref,
                possible_mono_parents,
                abs_sub_path,
                sub_repo_name,
                &thin_commit_to_inject,
            )?
            else {
                anyhow::bail!(
                    "Failed to inject commit {}, to become {}, at {abs_sub_path}: No common history with HEAD",
                    ref_to_inject.name,
                    dest_ref.as_bstr()
                );
            };
            Ok(())
        })();
        expander.wait()?;
        result
    }

    #[allow(unused_variables)]
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &self,
        top_push_url: &gix::Url,
        local_ref: &FullName,
        remote_ref: &FullName,
        top_repo_cache: &mut TopRepoCache,
        config: &mut crate::config::GitTopRepoConfig,
        dry_run: bool,
        logger: Logger,
        progress: indicatif::MultiProgress,
    ) -> Result<()> {
        let repo = self.gix_repo.to_thread_local();

        let local_ref_arg = match local_ref.as_bstr().to_os_str() {
            Ok(arg) => Ok(arg.to_owned()),
            Err(err) => anyhow::bail!("{err:#}"),
        };
        let export_refs_args: Vec<std::ffi::OsString> = repo
            .references()?
            .prefixed(b"refs/remotes/origin/".as_bstr())?
            .map(|r| {
                let r = match r {
                    Ok(r) => r,
                    Err(err) => anyhow::bail!("{err:#}"),
                }
                .detach();
                match bstr::concat([b"^".as_bstr(), r.name.as_bstr()]).to_os_str() {
                    Ok(arg) => Ok(arg.to_owned()),
                    Err(err) => anyhow::bail!("{err:#}"),
                }
            })
            .chain([local_ref_arg])
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| "Failed while iterating refs/remotes/origin/")?;

        let pb = progress.add(
            indicatif::ProgressBar::no_length()
                .with_style(
                    indicatif::ProgressStyle::default_spinner()
                        .template("{elapsed:>4} {msg} {pos}")
                        .unwrap(),
                )
                .with_message("Splitting commits"),
        );
        let mut fast_importer = crate::git_fast_export_import_dedup::FastImportRepoDedup::new(
            crate::git_fast_export_import::FastImportRepo::new(
                self.gix_repo.git_dir(),
                logger.clone(),
            )?,
            &mut top_repo_cache.dedup,
        );
        let fast_exporter = crate::git_fast_export_import::FastExportRepo::load_from_path(
            self.gix_repo.git_dir(),
            Some(export_refs_args),
            logger.clone(),
        )?;

        let mut to_push_metadata = Vec::new();
        let mut bumps = BumpCache::default();
        let mut imported_mono_commits = HashMap::new();
        let mut imported_submod_commits = HashMap::new();
        for entry in fast_exporter {
            let entry = entry?; // TODO: error handling
            match entry {
                crate::git_fast_export_import::FastExportEntry::Commit(exported_mono_commit) => {
                    // TODO: Should we check if exported_mono_commit.original_id exists in the top_repo_cache?
                    let mono_commit_id = MonoRepoCommitId::new(exported_mono_commit.original_id);
                    let gix_mono_commit = repo.find_commit(*mono_commit_id)?;
                    let mono_parents = exported_mono_commit
                        .parents
                        .iter()
                        .map(|parent_id| {
                            let mono_parent = top_repo_cache
                                .monorepo_commits
                                .get(&MonoRepoCommitId::new(*parent_id))
                                // Fallback to the newly imported commits.
                                .or_else(|| imported_mono_commits.get(parent_id))
                                .cloned()
                                .with_context(|| {
                                    format!("Unknown mono commit parent {}", parent_id.to_hex())
                                })?;
                            Ok(mono_parent)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    // The user should make sure that the .gitmodules is
                    // correct. Note that inner submodules might be
                    // mentioned, but there should not be any submodule
                    // mentioned that is a valid path in the repository.
                    // TODO: Handle updated URLs in the .gitmodules file.
                    // TODO: How to handle added and removed submodules from the .gitmodules file?
                    let mut grouped_file_changes: BTreeMap<(GitPath, RepoName, gix::Url), Vec<_>> =
                        BTreeMap::new();
                    for fc in exported_mono_commit.file_changes {
                        let (repo_name, submod_path, rel_path, push_url) = Self::resolve_push_repo(
                            &gix_mono_commit,
                            GitPath::new(fc.path),
                            top_push_url.clone(),
                            config,
                        )?;
                        grouped_file_changes
                            .entry((submod_path, repo_name, push_url))
                            .or_default()
                            .push(ChangedFile {
                                path: (*rel_path).clone(),
                                change: fc.change,
                            });
                    }
                    let (message, topic) =
                        Self::rewrite_push_message(exported_mono_commit.message.to_str()?);
                    if grouped_file_changes.len() > 1 && topic.is_none() {
                        anyhow::bail!(
                            "Multiple submodules changed in commit {mono_commit_id}, but no topic was provided. \
                            Please amend the commit message to add a 'Topic: something-descriptive' line."
                        );
                    }
                    for ((abs_sub_path, repo_name, push_url), file_changes) in grouped_file_changes
                    {
                        let push_branch = format!("{}push", repo_name.to_ref_prefix());
                        let parents_commit_ids = mono_parents
                            .iter()
                            .filter_map(|mono_parent| match &repo_name {
                                RepoName::Top => {
                                    bumps.get_top_bump(mono_parent).map(|top_bump| *top_bump)
                                }
                                RepoName::SubRepo(sub_repo_name) => bumps
                                    .get_some_submodule(mono_parent, &abs_sub_path, sub_repo_name)
                                    .map(|parent_submod| *parent_submod.get_orig_commit_id()),
                            })
                            .unique()
                            .collect_vec();
                        let parents = parents_commit_ids
                            .iter()
                            .map(|parent_submod_id| {
                                imported_submod_commits
                                    .get(parent_submod_id)
                                    .cloned()
                                    .unwrap_or(ImportCommitRef::CommitId(*parent_submod_id))
                            })
                            .collect_vec();
                        if parents.is_empty() {
                            match repo_name {
                                RepoName::Top => anyhow::bail!(
                                    "Mono commit {mono_commit_id} has no parents with content outside of the submodules, which is impossible"
                                ),
                                RepoName::SubRepo(sub_repo_name) => anyhow::bail!(
                                    "Submodule {sub_repo_name} at {abs_sub_path} does not exist as a git-link in any parent of {mono_commit_id}"
                                ),
                            }
                        }
                        let import_ref = fast_importer.write_commit(&FastImportCommit {
                            branch: <&FullNameRef as TryFrom<_>>::try_from(&push_branch)
                                .expect("valid ref name"),
                            author_info: exported_mono_commit.author_info.clone(),
                            committer_info: exported_mono_commit.committer_info.clone(),
                            encoding: exported_mono_commit.encoding.clone(),
                            message: bstr::BString::from(message.clone()),
                            file_changes,
                            parents,
                            original_id: None,
                        })?;
                        let import_commit_id = fast_importer.get_object_id(&import_ref)?;
                        imported_submod_commits.insert(import_commit_id, import_ref);

                        let (top_bump, submodule_bumps) = match repo_name {
                            RepoName::Top => {
                                (Some(TopRepoCommitId::new(import_commit_id)), HashMap::new())
                            }
                            RepoName::SubRepo(sub_repo_name) => (
                                None,
                                HashMap::from([(
                                    abs_sub_path,
                                    ExpandedOrRemovedSubmodule::Expanded(
                                        ExpandedSubmodule::Expanded(SubmoduleContent {
                                            repo_name: sub_repo_name,
                                            orig_commit_id: import_commit_id,
                                        }),
                                    ),
                                )]),
                            ),
                        };
                        let mono_commit = MonoRepoCommit::new_rc(
                            mono_parents
                                .iter()
                                .map(|mono_parent| MonoRepoParent::Mono(mono_parent.clone()))
                                .collect(),
                            top_bump,
                            submodule_bumps,
                        );
                        imported_mono_commits
                            .insert(exported_mono_commit.original_id, mono_commit.clone());
                        to_push_metadata.push((
                            push_url,
                            topic.clone(),
                            import_commit_id,
                            parents_commit_ids,
                        ));
                    }
                    pb.inc(1);
                }
                crate::git_fast_export_import::FastExportEntry::Reset(reset) => {
                    logger.warning(format!(
                        "Resetting {} to {} is unimplemented",
                        reset.branch, reset.from
                    ));
                }
            };
        }
        fast_importer.wait()?;
        drop(pb);

        // Group the pushes together to run fewer git-push commands.
        to_push_metadata.reverse();
        let mut redundant_pushes = HashMap::new();
        to_push_metadata.retain(|(push_url, topic, commit_id, parents)| {
            let is_needed = redundant_pushes
                .remove(&(push_url.clone(), *commit_id))
                .as_ref()
                != Some(topic);
            for parent in parents {
                // Even if the entry exists, it should be replaced to show that
                // the first push after `*parent` will be with `topic`. Later
                // pushes will not affect anything anyway.
                redundant_pushes.insert((push_url.clone(), *parent), topic.clone());
            }
            is_needed
        });
        to_push_metadata.reverse();

        progress.suspend(|| {
            let info_label = if dry_run { "Would run" } else { "Running" };
            for (push_url, topic, commit_id, _parents) in &to_push_metadata {
                let topic_arg = match topic {
                    Some(topic) => format!(" -o topic={topic}"),
                    None => String::new(),
                };
                println!(
                    "{info_label}: git push {push_url}{topic_arg} {}:{remote_ref}",
                    commit_id.to_hex()
                );
            }
        });
        if !dry_run {
            let mut failed_pushes = 0;
            for (push_url, topic, commit_id, _parents) in to_push_metadata {
                let mut cmd = git_command(&self.directory);
                cmd.arg("push").arg(push_url.to_bstring().to_os_str()?);
                if let Some(topic) = topic {
                    cmd.arg("-o").arg(format!("topic={topic}"));
                }
                cmd.arg(format!("{commit_id}:{remote_ref}"));
                if let Err(err) = cmd.check_success_with_stderr() {
                    logger.error(format!(
                        "Failed to git push {push_url} {commit_id}:{remote_ref}: {err:#}"
                    ));
                    failed_pushes += 1;
                }
            }
            if failed_pushes != 0 {
                let times_string = if failed_pushes == 1 { "time" } else { "times" };
                anyhow::bail!(format!("git-push failed {failed_pushes} {times_string}"));
            }
        }
        Ok(())
    }

    fn rewrite_push_message(message: &str) -> (String, Option<String>) {
        let mut filtered_message = String::with_capacity(message.len());
        let mut topic = None;
        for line in message.lines() {
            if let Some(topic_name) = line.strip_prefix("Topic: ") {
                topic = Some(topic_name.to_owned());
            } else if line.starts_with("^-- ") {
                // Ignore '^-- path/to/submod 0123...'
            } else {
                filtered_message.push_str(line);
                filtered_message.push('\n');
            }
        }
        (filtered_message, topic)
    }

    /// Resolves which repository to push to. Note that the push URL might not be part of the git-toprepo configuration, so `url` is used when resolving the that.
    fn resolve_push_repo(
        mono_commit: &gix::Commit,
        path: GitPath,
        mut push_url: gix::Url,
        config: &mut crate::config::GitTopRepoConfig,
    ) -> Result<(RepoName, GitPath, GitPath, gix::Url)> {
        let mut repo_name = RepoName::Top;
        let mut repo_path = GitPath::new(b"".into());
        let mut rel_path = path;
        let mut generic_url = EMPTY_GIX_URL.clone();
        loop {
            let dot_gitmodules_path = repo_path.join(&GitPath::new(b".gitmodules".into()));
            let dot_gitmodules_bytes = match mono_commit
                .tree()?
                .lookup_entry_by_path(dot_gitmodules_path.to_path()?)?
            {
                Some(dot_gitmodules_entry) => {
                    let dot_gitmodules_object = dot_gitmodules_entry.object()?;
                    dot_gitmodules_object
                        .try_into_blob()
                        .with_context(|| format!("Failed to read {dot_gitmodules_path} file"))?
                        .take_data()
                }
                None => Vec::new(),
            };
            let git_modules_info = GitModulesInfo::parse_dot_gitmodules_bytes(
                &dot_gitmodules_bytes,
                dot_gitmodules_path.to_path()?.to_owned(),
            )
            .with_context(|| format!("Failed to parse {dot_gitmodules_path} file"))?;
            let Some((submod_path, sub_url)) = git_modules_info.get_containing_submodule(&rel_path)
            else {
                return Ok((repo_name, repo_path, rel_path.clone(), push_url));
            };
            // Apply one submodule level.
            rel_path = GitPath::new(
                rel_path
                    .strip_prefix(submod_path.as_bytes())
                    .expect("part of the submodule")
                    .strip_prefix(b"/")
                    .expect("part of the submodule")
                    .into(),
            );
            repo_path = repo_path.join(submod_path);
            let sub_url = match sub_url {
                Ok(sub_url) => sub_url,
                Err(err) => anyhow::bail!("{err:#}"),
            };
            generic_url = generic_url.join(sub_url);
            push_url = push_url.join(sub_url);
            // Update the return value.
            let (sub_repo_name, _) = config.get_or_insert_from_url(&generic_url)?;
            repo_name = RepoName::SubRepo(sub_repo_name);
        }
    }
}

#[serde_as]
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct TopRepoCommitId(
    #[serde_as(as = "serde_with::IfIsHumanReadable<serde_with::DisplayFromStr>")] CommitId,
);

impl TopRepoCommitId {
    pub fn new(commit_id: CommitId) -> Self {
        TopRepoCommitId(commit_id)
    }

    fn into_inner(self) -> CommitId {
        self.0
    }
}

impl Deref for TopRepoCommitId {
    type Target = CommitId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for TopRepoCommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub type RepoStates = HashMap<RepoName, RepoData>;

// TODO: Use `Rc` to all the `GitPath`s and `ObjectId`s to avoid memory duplication.
// Is it really more efficient to use `Rc`?
#[derive(Default)]
pub struct TopRepoCache {
    pub repos: RepoStates,
    pub monorepo_commits: HashMap<MonoRepoCommitId, Rc<MonoRepoCommit>>,
    pub monorepo_commit_ids: HashMap<RcKey<MonoRepoCommit>, MonoRepoCommitId>,
    /// Mapping from top repo commit to mono repo commit.
    pub top_to_mono_map: HashMap<TopRepoCommitId, Rc<MonoRepoCommit>>,
    pub dedup: GitFastExportImportDedupCache,
}

#[serde_as]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OriginalSubmodParent {
    // TODO: Unused?
    pub path: GitPath,
    #[serde_as(as = "serde_with::IfIsHumanReadable<serde_with::DisplayFromStr>")]
    pub commit_id: CommitId,
}

#[derive(Clone)]
pub enum MonoRepoParent {
    OriginalSubmod(OriginalSubmodParent),
    Mono(Rc<MonoRepoCommit>),
}

/// While importing, the commit id might not yet be known and set to a dummy id.
#[serde_as]
#[derive(Clone, Eq, Hash, PartialEq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct MonoRepoCommitId(
    #[serde_as(as = "serde_with::IfIsHumanReadable<serde_with::DisplayFromStr>")] CommitId,
);

impl MonoRepoCommitId {
    pub fn new(commit_id: CommitId) -> Self {
        MonoRepoCommitId(commit_id)
    }

    pub fn dummy() -> Self {
        Self(gix::ObjectId::null(gix::hash::Kind::Sha1))
    }
}

impl Display for MonoRepoCommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Deref for MonoRepoCommitId {
    type Target = CommitId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct MonoRepoCommit {
    pub parents: Vec<MonoRepoParent>,
    /// The depth in the mono repo, i.e. the number of commits in the longest
    /// history path.
    pub depth: usize,
    /// Potential update of the top repo content in this mono repo commit.
    pub top_bump: Option<TopRepoCommitId>,
    /// The original commits that were updated in this mono repo commit, recursively.
    pub submodule_bumps: HashMap<GitPath, ExpandedOrRemovedSubmodule>,
    /// The expanded submodule paths in this mono repo commit, recursively.
    pub submodule_paths: Rc<HashSet<GitPath>>,
}

impl MonoRepoCommit {
    pub fn new_rc(
        parents: Vec<MonoRepoParent>,
        top_bump: Option<TopRepoCommitId>,
        submodule_bumps: HashMap<GitPath, ExpandedOrRemovedSubmodule>,
    ) -> Rc<MonoRepoCommit> {
        let depth = parents
            .iter()
            .filter_map(|p| match p {
                MonoRepoParent::Mono(parent) => Some(parent.depth + 1),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        // Adding and removing more than one submodule at a time is so rare that
        // it is not worth optimizing for it. Let's copy the HashSet every time.
        let mut submodule_paths = match parents.first() {
            Some(MonoRepoParent::Mono(first_parent)) => first_parent.submodule_paths.clone(),
            Some(MonoRepoParent::OriginalSubmod(_)) | None => Rc::new(HashSet::new()),
        };
        for (path, bump) in submodule_bumps.iter() {
            match bump {
                ExpandedOrRemovedSubmodule::Expanded(_) => {
                    submodule_paths = Rc::new({
                        let mut paths = submodule_paths.as_ref().clone();
                        paths.insert(path.clone());
                        paths
                    });
                }
                ExpandedOrRemovedSubmodule::Removed => {
                    submodule_paths = Rc::new({
                        let mut paths = submodule_paths.as_ref().clone();
                        paths.remove(path);
                        paths
                    });
                }
            }
        }
        Rc::new(MonoRepoCommit {
            parents,
            depth,
            top_bump,
            submodule_bumps,
            submodule_paths,
        })
    }
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ExpandedSubmodule {
    /// Known submodule and known commit.
    Expanded(SubmoduleContent),
    /// The submodule was not expanded. The used has to run `git submodule
    /// update --init` to get its content.
    KeptAsSubmodule(
        #[serde_as(serialize_as = "serde_with::IfIsHumanReadable<serde_with::DisplayFromStr>")]
        CommitId,
    ),
    /// The commit does not exist (any more) in the referred sub repository.
    CommitMissingInSubRepo(SubmoduleContent),
    /// It is unknown which sub repo it should be loaded from.
    UnknownSubmodule(
        #[serde_as(serialize_as = "serde_with::IfIsHumanReadable<serde_with::DisplayFromStr>")]
        CommitId,
    ),
    // TODO: MovedAndBumped(MovedSubmodule),
    /// If a submodule has regressed to an earlier or unrelated commit, it
    /// should be expanded with a different set of parents submodules. The
    /// reason is that there should not be merge lines over a revert point as
    /// those merges makes no sense.
    ///
    /// Consider the following example:
    /// ```txt
    /// Submodule:
    /// * z
    /// * y
    /// * x
    ///
    /// Top repo:
    /// * C with z
    /// * B with x
    /// * A with y
    ///
    /// Mono repo (not acceptable):
    /// * C with z
    /// |\
    /// * |  B with x
    /// |/
    /// * A with y
    /// ```
    /// This mono repo version includes a merge line from A to C after the
    /// submodule was reverted in B. The merge line does no bring any new
    /// information and is simply redundant. This means that we are missing `y`
    /// in the history between `x` in B and `z` in C. Instead, the following
    /// history is wanted:
    /// ```txt
    /// Mono repo (acceptable):
    /// * C with z
    /// |\
    /// | * B with y
    /// |/
    /// * B with x
    /// |\
    /// | * Resetting to x
    /// |/
    /// * A with y
    /// ```
    // TODO: Implement this in the
    // TopRepoExpander::get_recursive_submodule_bumps() or extract the
    // information from TopRepoExpander::expand_inner_submodules().
    RegressedNotFullyImplemented(SubmoduleContent),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ExpandedOrRemovedSubmodule {
    Expanded(ExpandedSubmodule),
    Removed,
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SubmoduleContent {
    pub repo_name: SubRepoName,
    #[serde_as(as = "serde_with::IfIsHumanReadable<serde_with::DisplayFromStr>")]
    pub orig_commit_id: CommitId,
}

impl ExpandedSubmodule {
    /// Returns the submodule content if the submodule could be resolved, i.e.
    /// .gitmodules information was accurate.
    pub fn get_known_submod(&self) -> Option<&SubmoduleContent> {
        match self {
            ExpandedSubmodule::Expanded(submod) => Some(submod),
            ExpandedSubmodule::KeptAsSubmodule(_commit_id) => None,
            ExpandedSubmodule::CommitMissingInSubRepo(submod) => Some(submod),
            ExpandedSubmodule::UnknownSubmodule(_commit_id) => None,
            ExpandedSubmodule::RegressedNotFullyImplemented(submod) => Some(submod),
        }
    }

    pub fn get_orig_commit_id(&self) -> &CommitId {
        match self {
            ExpandedSubmodule::Expanded(submod) => &submod.orig_commit_id,
            ExpandedSubmodule::KeptAsSubmodule(commit_id) => commit_id,
            ExpandedSubmodule::CommitMissingInSubRepo(submod) => &submod.orig_commit_id,
            ExpandedSubmodule::UnknownSubmodule(commit_id) => commit_id,
            ExpandedSubmodule::RegressedNotFullyImplemented(submod) => &submod.orig_commit_id,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RepoData {
    pub url: gix::Url,
    pub thin_commits: HashMap<CommitId, Rc<ThinCommit>>,
    /// A map for git-fast-import commit deduplicating, where the exported
    /// commit have different committer but otherwise are exactly the same.
    /// The values represent the latest imported or exported commit id.
    pub dedup_cache: HashMap<WithoutCommitterId, CommitId>,
}

impl RepoData {
    pub fn new(url: gix::Url) -> Self {
        Self {
            url,
            thin_commits: HashMap::new(),
            dedup_cache: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThinSubmodule {
    AddedOrModified(ThinSubmoduleContent),
    Removed,
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThinSubmoduleContent {
    /// `None` is the submodule could not be resolved from the .gitmodules file.
    pub repo_name: Option<SubRepoName>,
    #[serde_as(as = "serde_with::IfIsHumanReadable<serde_with::DisplayFromStr>")]
    pub commit_id: CommitId,
}

#[derive(Debug)]
pub struct ThinCommit {
    pub commit_id: CommitId,
    pub tree_id: TreeId,
    /// Number of parents in the longest path to the root commit. This number is
    /// strictly decreasing when following the parents.
    pub depth: u32,
    pub parents: Vec<Rc<ThinCommit>>,
    pub dot_gitmodules: Option<BlobId>,
    /// Submodule updates in this commit compared to first parent. Added
    /// submodules are included. `BTreeMap` is used for deterministic ordering.
    pub submodule_bumps: BTreeMap<GitPath, ThinSubmodule>,
    /// Paths to all the submodules in the commit, not just the updated ones.
    pub submodule_paths: Rc<HashSet<GitPath>>,
}

impl ThinCommit {
    /// Creates a new `ThinCommit` which is effectively read only due to the
    /// reference counting.
    ///
    /// It is an error to try to update the contents of the `ThinCommit` after
    /// it has been created.
    pub fn new_rc(
        commit_id: CommitId,
        tree_id: TreeId,
        parents: Vec<Rc<ThinCommit>>,
        dot_gitmodules: Option<BlobId>,
        submodule_bumps: BTreeMap<GitPath, ThinSubmodule>,
    ) -> Rc<Self> {
        // Adding and removing more than one submodule at a time is so rare that
        // it is not worth optimizing for it. Let's copy the HashSet every time.
        let mut submodule_paths = match parents.first() {
            Some(first_parent) => first_parent.submodule_paths.clone(),
            None => Rc::new(HashSet::new()),
        };
        for (path, bump) in submodule_bumps.iter() {
            match bump {
                ThinSubmodule::AddedOrModified(_) => {
                    submodule_paths = Rc::new({
                        let mut paths = submodule_paths.as_ref().clone();
                        paths.insert(path.clone());
                        paths
                    });
                }
                ThinSubmodule::Removed => {
                    submodule_paths = Rc::new({
                        let mut paths = submodule_paths.as_ref().clone();
                        paths.remove(path);
                        paths
                    });
                }
            }
        }
        Rc::new(Self {
            commit_id,
            tree_id,
            depth: parents.iter().map(|p| p.depth + 1).max().unwrap_or(0),
            parents,
            dot_gitmodules,
            submodule_bumps,
            submodule_paths,
        })
    }

    pub fn is_descendant_of(&self, ancestor: &ThinCommit) -> bool {
        // Doesn't matter which order we iterate.
        let mut visited = HashSet::new();
        let mut queue = Vec::new();
        visited.insert(self.commit_id);
        queue.push(self);

        while let Some(descendant) = queue.pop() {
            if descendant.commit_id == ancestor.commit_id {
                return true;
            }
            for descendant_parent in &descendant.parents {
                if descendant_parent.depth >= ancestor.depth
                    && visited.insert(descendant_parent.commit_id)
                {
                    queue.push(descendant_parent);
                }
            }
        }
        false
    }

    /// Walks the first parent commit graph to the submodule entry.
    pub fn get_submodule<'a>(&'a self, path: &GitPath) -> Option<&'a ThinSubmodule> {
        let mut node = self;
        loop {
            if let Some(submod) = node.submodule_bumps.get(path) {
                return Some(submod);
            }
            let Some(parent) = node.parents.first() else {
                break;
            };
            node = parent;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_fetch() -> Result<()> {
        use tempfile::tempdir;

        let from_dir = tempdir().unwrap();
        let from_path = from_dir.path();

        let to_dir = tempdir().unwrap();
        let to_path = to_dir.path();
        let env = HashMap::from([
            ("GIT_AUTHOR_NAME", "A Name"),
            ("GIT_AUTHOR_EMAIL", "a@no.domain"),
            ("GIT_AUTHOR_DATE", "2023-01-02T03:04:05Z+01:00"),
            ("GIT_COMMITTER_NAME", "C Name"),
            ("GIT_COMMITTER_EMAIL", "c@no.domain"),
            ("GIT_COMMITTER_DATE", "2023-06-07T08:09:10Z+01:00"),
        ]);

        git_command(from_path)
            .args(["init", "--quiet", "--initial-branch", "main"])
            .envs(&env)
            .safe_status()?
            .check_success()?;
        git_command(from_path)
            .args(["commit", "--allow-empty", "--quiet"])
            .args(["-m", "Initial commit"])
            .envs(&env)
            .safe_status()?
            .check_success()?;
        git_command(from_path)
            .args(["tag", "mytag"])
            .envs(&env)
            .safe_status()?
            .check_success()?;

        let toprepo = TopRepo::create(
            to_path.to_path_buf(),
            gix::url::Url::try_from(from_path).unwrap(),
        )
        .unwrap();

        toprepo.fetch_toprepo_quiet().unwrap();

        let ref_pairs = vec![
            ("HEAD", "refs/namespaces/top/HEAD"),
            ("main", "refs/namespaces/top/refs/heads/main"),
            ("mytag", "refs/namespaces/top/refs/tags/mytag"),
        ];
        for (orig_ref, top_ref) in ref_pairs {
            let orig_rev = git_command(from_path)
                .args(["rev-parse", "--verify", orig_ref])
                .output_stdout_only()?
                .check_success_with_stderr()
                .with_context(|| format!("orig {orig_ref}"))?
                .stdout
                .to_owned();
            let top_rev = git_command(&toprepo.directory)
                .args(["rev-parse", "--verify", top_ref])
                .output_stdout_only()?
                .check_success_with_stderr()
                .with_context(|| format!("top {top_ref}"))?
                .stdout
                .to_owned();
            assert_eq!(
                orig_rev.to_str().unwrap(),
                top_rev.to_str().unwrap(),
                "ref {orig_ref} mismatch",
            );
        }
        Ok(())
    }
}
