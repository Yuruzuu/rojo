mod file_names;
mod fs_snapshot;
mod hash;
mod property_filter;
mod ref_properties;
mod snapshot;

use anyhow::Context;
use indexmap::IndexMap;
use memofs::Vfs;
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, Instance, Ustr, UstrSet, WeakDom,
};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    env,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

use crate::{
    glob::IgnorableGlob,
    snapshot::{InstanceWithMeta, RojoTree},
    snapshot_middleware::Middleware,
    syncback::ref_properties::{collect_referents, link_referents},
    Project,
};

pub use file_names::{extension_for_middleware, name_for_inst, validate_file_name};
pub use fs_snapshot::FsSnapshot;
pub use hash::*;
pub use property_filter::{filter_properties, filter_properties_preallocated};
pub use snapshot::{SyncbackData, SyncbackSnapshot};

/// The name of an enviroment variable to use to override the behavior of
/// syncback on model files.
/// By default, syncback will use `Rbxm` for model files.
/// If this is set to `1`, it will instead use `Rbxmx`. If it is set to `2`,
/// it will use `JsonModel`.
///
/// This will **not** override existing `Rbxm` middleware. It will only impact
/// new files.
const DEBUG_MODEL_FORMAT_VAR: &str = "ROJO_SYNCBACK_DEBUG";

pub fn syncback_loop(
    vfs: &Vfs,
    old_tree: &mut RojoTree,
    mut new_tree: WeakDom,
    project: &Project,
) -> anyhow::Result<FsSnapshot> {
    let ignore_patterns = project
        .syncback_rules
        .as_ref()
        .map(|rules| rules.compile_globs())
        .transpose()?;

    // TODO: Add a better way to tell if the root of a project is a directory
    let skip_pruning = if let Some(path) = &project.tree.path {
        let path = project.folder_location().join(path.path());
        let middleware = Middleware::middleware_for_path(vfs, &project.sync_rules, &path)?;
        if let Some(middleware) = middleware {
            middleware.is_dir()
        } else {
            false
        }
    } else {
        false
    };
    if !skip_pruning {
        // Strip out any objects from the new tree that aren't in the old tree. This
        // is necessary so that hashing the roots of each tree won't always result
        // in different hashes. Shout out to Roblox for serializing a bunch of
        // Services nobody cares about.
        log::debug!("Pruning new tree");
        strip_unknown_root_children(&mut new_tree, old_tree);
    }

    log::debug!("Collecting referents for new DOM...");
    let deferred_referents = collect_referents(&new_tree, old_tree);

    // Remove any properties that are manually blocked from syncback via the
    // project file.
    log::debug!("Pre-filtering properties on DOMs");
    for referent in descendants(&new_tree, new_tree.root_ref()) {
        let new_inst = new_tree.get_by_ref_mut(referent).unwrap();
        if let Some(filter) = get_property_filter(project, new_inst) {
            for prop in filter {
                new_inst.properties.remove(&prop);
            }
        }
    }
    for referent in descendants(old_tree.inner(), old_tree.get_root_id()) {
        let mut old_inst_rojo = old_tree.get_instance_mut(referent).unwrap();
        let old_inst = old_inst_rojo.inner_mut();
        if let Some(filter) = get_property_filter(project, old_inst) {
            for prop in filter {
                old_inst.properties.remove(&prop);
            }
        }
    }

    // Handle removing the current camera.
    if let Some(syncback_rules) = &project.syncback_rules {
        if !syncback_rules.sync_current_camera.unwrap_or_default() {
            log::debug!("Removing CurrentCamera from new DOM");
            let mut camera_ref = None;
            for child_ref in new_tree.root().children() {
                let inst = new_tree.get_by_ref(*child_ref).unwrap();
                if inst.class == "Workspace" {
                    camera_ref = inst.properties.get(&ustr("CurrentCamera"));
                    break;
                }
            }
            if let Some(Variant::Ref(camera_ref)) = camera_ref {
                if new_tree.get_by_ref(*camera_ref).is_some() {
                    new_tree.destroy(*camera_ref);
                }
            }
        }
    }

    let ignore_referents = project
        .syncback_rules
        .as_ref()
        .and_then(|s| s.ignore_referents)
        .unwrap_or_default();
    if !ignore_referents {
        log::debug!("Linking referents for new DOM");
        link_referents(deferred_referents, &mut new_tree)?;
    } else {
        log::debug!("Skipping referent linking as per project syncback rules");
    }

    // As with pruning the children of the new root, we need to ensure the roots
    // for both DOMs have the same name otherwise their hashes will always be
    // different.
    new_tree.root_mut().name = old_tree.root().name().to_string();

    log::debug!("Hashing project DOM");
    let old_hashes = hash_tree(project, old_tree.inner(), old_tree.get_root_id());
    log::debug!("Hashing file DOM");
    let new_hashes = hash_tree(project, &new_tree, new_tree.root_ref());

    let project_path = project.folder_location();
    let mut ignored_tree_paths = Vec::new();
    if let Some(rules) = project
        .syncback_rules
        .as_ref()
        .filter(|rules| !rules.ignore_trees.is_empty())
    {
        for inst in old_tree.inner().descendants() {
            let inst_path = old_tree.inner().full_path_of(inst.referent(), "/");
            if rules
                .ignore_trees
                .iter()
                .any(|ignored| tree_is_ignored(&inst_path, ignored))
            {
                ignored_tree_paths.extend(
                    old_tree
                        .get_instance(inst.referent())
                        .unwrap()
                        .metadata()
                        .relevant_paths
                        .iter()
                        .cloned(),
                );
            }
        }
    }
    let path_can_be_removed = |path: &Path| {
        let path = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => vfs
                .canonicalize(parent)
                .map(|parent| Cow::Owned(parent.join(name)))
                .unwrap_or_else(|_| normalize_syncback_path(path)),
            _ => normalize_syncback_path(path),
        };
        is_valid_path(&ignore_patterns, project_path, path.as_ref())
            && !ignored_tree_paths
                .iter()
                .any(|ignored| path.starts_with(ignored))
    };

    let syncback_data = SyncbackData {
        vfs,
        old_tree,
        new_tree: &new_tree,
        project,
    };

    let mut snapshots = vec![SyncbackSnapshot {
        data: syncback_data,
        old: Some(old_tree.get_root_id()),
        new: new_tree.root_ref(),
        path: project.file_location.clone(),
        middleware: Some(Middleware::Project),
    }];

    let mut fs_snapshot = FsSnapshot::new();

    'syncback: while let Some(snapshot) = snapshots.pop() {
        let inst_path = snapshot.get_new_inst_path(snapshot.new);
        // We can quickly check that two subtrees are identical and if they are,
        // skip reconciling them.
        if let Some(old_ref) = snapshot.old {
            match (old_hashes.get(&old_ref), new_hashes.get(&snapshot.new)) {
                (Some(old), Some(new)) => {
                    if old == new {
                        log::trace!(
                            "Skipping {inst_path} due to it being identically hashed as {old:?}"
                        );
                        continue;
                    }
                }
                _ => unreachable!("All Instances in both DOMs should have hashes"),
            }
        }

        if !is_valid_path(&ignore_patterns, project_path, &snapshot.path) {
            log::debug!("Skipping {inst_path} because its path matches ignore pattern");
            continue;
        }
        if let Some(syncback_rules) = &project.syncback_rules {
            // Ignore trees;
            for ignored in &syncback_rules.ignore_trees {
                if tree_is_ignored(&inst_path, ignored) {
                    log::debug!("Tree {inst_path} is blocked by project");
                    continue 'syncback;
                }
            }
        }

        let middleware = get_best_middleware(&snapshot);

        log::trace!(
            "Middleware for {inst_path} is {:?} (path is {})",
            middleware,
            snapshot.path.display()
        );

        if matches!(middleware, Middleware::Json | Middleware::Toml) {
            log::warn!("Cannot syncback {middleware:?} at {inst_path}, skipping");
            continue;
        }

        let syncback = match middleware.syncback(&snapshot) {
            Ok(syncback) => syncback,
            Err(err) if middleware == Middleware::Dir => {
                let new_middleware = match env::var(DEBUG_MODEL_FORMAT_VAR) {
                    Ok(value) if value == "1" => Middleware::Rbxmx,
                    Ok(value) if value == "2" => Middleware::JsonModel,
                    _ => Middleware::Rbxm,
                };
                let file_name = snapshot
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("Directory middleware should have a name in its path")?;
                let mut path = snapshot.path.clone();
                path.set_file_name(format!(
                    "{file_name}.{}",
                    extension_for_middleware(new_middleware)
                ));
                let new_snapshot = snapshot.with_new_path(path, snapshot.new, snapshot.old);
                log::warn!(
                    "Could not syncback {inst_path} as a Directory because: {err}.\n\
                    It will instead be synced back as a {new_middleware:?}."
                );
                let new_syncback_result = new_middleware
                    .syncback(&new_snapshot)
                    .with_context(|| format!("Failed to syncback {inst_path}"));
                if new_syncback_result.is_ok() && snapshot.old_inst().is_some() {
                    // We need to remove the old FS representation if we're
                    // reserializing it as an rbxm.
                    let mut removals = FsSnapshot::new();
                    if !removals.remove_path_with_filter(
                        vfs,
                        &snapshot.path,
                        &path_can_be_removed,
                    )? {
                        anyhow::bail!(
                            "Cannot replace {} with a model file because it contains ignored paths",
                            snapshot.path.display()
                        );
                    }
                    fs_snapshot.merge(removals);
                }
                new_syncback_result?
            }
            Err(err) => anyhow::bail!("Failed to syncback {inst_path} because {err}"),
        };

        if !syncback.removed_children.is_empty() {
            log::debug!(
                "removed children for {inst_path}: {}",
                syncback.removed_children.len()
            );
            'remove: for inst in &syncback.removed_children {
                let path = inst.metadata().instigating_source.as_ref().unwrap().path();
                let inst_path = snapshot.get_old_inst_path(inst.id());
                if !is_valid_path(&ignore_patterns, project_path, path) {
                    log::debug!(
                        "Skipping removing {} because its matches an ignore pattern",
                        path.display()
                    );
                    continue;
                }
                if let Some(syncback_rules) = &project.syncback_rules {
                    for ignored in &syncback_rules.ignore_trees {
                        if tree_is_ignored(&inst_path, ignored) {
                            log::debug!("Skipping removing {inst_path} because its path is blocked by project");
                            continue 'remove;
                        }
                    }
                }
                fs_snapshot.remove_path_with_filter(vfs, path, &path_can_be_removed)?;
            }
        }

        // TODO provide replacement snapshots for e.g. two way sync

        fs_snapshot.merge_with_filter(syncback.fs_snapshot, |path| {
            is_valid_path(&ignore_patterns, project_path, path)
        });

        snapshots.extend(syncback.children);
    }

    Ok(fs_snapshot)
}

pub struct SyncbackReturn<'sync> {
    pub fs_snapshot: FsSnapshot,
    pub children: Vec<SyncbackSnapshot<'sync>>,
    pub removed_children: Vec<InstanceWithMeta<'sync>>,
}

pub fn get_best_middleware(snapshot: &SyncbackSnapshot) -> Middleware {
    // At some point, we're better off using an O(1) method for checking
    // equality for classes like this.
    static JSON_MODEL_CLASSES: OnceLock<HashSet<&str>> = OnceLock::new();
    let json_model_classes = JSON_MODEL_CLASSES.get_or_init(|| {
        [
            "Actor",
            "Sound",
            "SoundGroup",
            "Sky",
            "Atmosphere",
            "BloomEffect",
            "BlurEffect",
            "ColorCorrectionEffect",
            "DepthOfFieldEffect",
            "SunRaysEffect",
            "ParticleEmitter",
            "TextChannel",
            "TextChatCommand",
            // TODO: Implement a way to use inheritance for this
            "ChatWindowConfiguration",
            "ChatInputBarConfiguration",
            "BubbleChatConfiguration",
            "ChannelTabsConfiguration",
            "RemoteEvent",
            "UnreliableRemoteEvent",
            "RemoteFunction",
            "BindableEvent",
            "BindableFunction",
        ]
        .into()
    });

    let old_middleware = snapshot
        .old_inst()
        .and_then(|inst| inst.metadata().middleware);
    let inst = snapshot.new_inst();

    let mut middleware;

    if let Some(override_middleware) = snapshot.middleware {
        return override_middleware;
    } else if let Some(old_middleware) = old_middleware {
        return old_middleware;
    } else if json_model_classes.contains(inst.class.as_str()) {
        middleware = Middleware::JsonModel;
    } else {
        middleware = match inst.class.as_str() {
            "Folder" | "Configuration" | "Tool" => Middleware::Dir,
            "StringValue" => Middleware::Text,
            "Script" => Middleware::ServerScript,
            "LocalScript" => Middleware::ClientScript,
            "ModuleScript" => Middleware::ModuleScript,
            "LocalizationTable" => Middleware::Csv,
            // This isn't the ideal way to handle this but it works.
            name if name.ends_with("Value") => Middleware::JsonModel,
            _ => Middleware::Rbxm,
        }
    }

    // Init files describe their containing directory instead of a child, so
    // new children with those file names need a directory of their own.
    // Case variants alias those files on case-insensitive file systems.
    let reserved_init_name = snapshot.old.is_none()
        && matches!(
            middleware,
            Middleware::ServerScript
                | Middleware::ClientScript
                | Middleware::PluginScript
                | Middleware::ModuleScript
                | Middleware::Csv
        )
        && matches!(
            format!("{}.{}", inst.name, extension_for_middleware(middleware))
                .to_ascii_lowercase()
                .as_str(),
            "init.lua"
                | "init.luau"
                | "init.server.lua"
                | "init.server.luau"
                | "init.client.lua"
                | "init.client.luau"
                | "init.plugin.lua"
                | "init.plugin.luau"
                | "init.csv"
        );

    if !inst.children().is_empty() || reserved_init_name {
        middleware = match middleware {
            Middleware::ServerScript => Middleware::ServerScriptDir,
            Middleware::ClientScript => Middleware::ClientScriptDir,
            Middleware::PluginScript => Middleware::PluginScriptDir,
            Middleware::ModuleScript => Middleware::ModuleScriptDir,
            Middleware::Csv => Middleware::CsvDir,
            Middleware::JsonModel | Middleware::Text => Middleware::Dir,
            _ => middleware,
        }
    }

    if middleware == Middleware::Rbxm {
        middleware = match env::var(DEBUG_MODEL_FORMAT_VAR) {
            Ok(value) if value == "1" => Middleware::Rbxmx,
            Ok(value) if value == "2" => Middleware::JsonModel,
            _ => Middleware::Rbxm,
        }
    }

    middleware
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SyncbackRules {
    /// A list of subtrees in a file that will be ignored by Syncback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignore_trees: Vec<String>,
    /// A list of patterns to check against the path an Instance would serialize
    /// to. If a path matches one of these, the Instance won't be syncbacked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignore_paths: Vec<String>,
    /// A map of classes to properties to ignore for that class when doing
    /// syncback.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    ignore_properties: IndexMap<Ustr, Vec<Ustr>>,
    /// Whether or not the `CurrentCamera` of `Workspace` is included in the
    /// syncback or not. Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_current_camera: Option<bool>,
    /// Whether or not to sync properties that cannot be modified via scripts.
    /// Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_unscriptable: Option<bool>,
    /// Whether to skip serializing referent properties like `Model.PrimaryPart`
    /// during syncback. Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_referents: Option<bool>,
    /// Whether the globs specified in `ignore_paths` should be modified to also
    /// match directories. Defaults to `true`.
    ///
    /// If this is `true`, it'll take ignore globs that end in `/**` and convert
    /// them to also handle the directory they're referring to. This is
    /// generally a better UX.
    #[serde(skip_serializing_if = "Option::is_none")]
    create_ignore_dir_paths: Option<bool>,
}

impl SyncbackRules {
    pub fn compile_globs(&self) -> anyhow::Result<Vec<IgnorableGlob>> {
        let mut globs = Vec::with_capacity(self.ignore_paths.len());
        let dir_ignore_paths = self.create_ignore_dir_paths.unwrap_or(true);

        for pattern in &self.ignore_paths {
            let glob = IgnorableGlob::new(pattern)
                .with_context(|| format!("the pattern '{pattern}' is not a valid glob"))?;
            globs.push(glob);

            if dir_ignore_paths {
                if let Some(dir_pattern) = pattern.strip_suffix("/**") {
                    if let Ok(glob) = IgnorableGlob::new(dir_pattern) {
                        globs.push(glob)
                    }
                }
            }
        }

        Ok(globs)
    }
}

pub(crate) fn is_valid_path(
    globs: &Option<Vec<IgnorableGlob>>,
    base_path: &Path,
    path: &Path,
) -> bool {
    let path = normalize_syncback_path(path);
    let base_path = normalize_syncback_path(base_path);
    let test_path = match path.strip_prefix(base_path.as_ref()) {
        Ok(suffix) => suffix,
        Err(_) => path.as_ref(),
    };
    if test_path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
    }) {
        return false;
    }
    if let Some(ref ignore_paths) = globs {
        // Gitignore-style "last match wins"
        let mut ignored = false;
        for glob in ignore_paths {
            if glob.is_match(test_path) {
                ignored = !glob.is_negation();
            }
        }
        if ignored {
            return false;
        }
    }
    true
}

fn normalize_syncback_path(path: &Path) -> Cow<'_, Path> {
    if !path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Cow::Borrowed(path);
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir if normalized.file_name().is_some_and(|name| name != "..") => {
                normalized.pop();
            }
            Component::ParentDir if !normalized.has_root() => normalized.push(".."),
            Component::ParentDir | Component::CurDir => {}
            _ => normalized.push(component.as_os_str()),
        }
    }

    Cow::Owned(normalized)
}

fn tree_is_ignored(path: &str, ignored: &str) -> bool {
    let ignored = ignored.trim_end_matches('/');
    ignored.is_empty()
        || path == ignored
        || path
            .strip_prefix(ignored)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Returns a set of properties that should not be written with syncback if
/// one exists. This list is read directly from the Project and takes
/// inheritance into effect.
///
/// It **does not** handle properties that should not serialize for other
/// reasons, such as being defaults or being marked as not serializing in the
/// ReflectionDatabase.
fn get_property_filter(project: &Project, new_inst: &Instance) -> Option<UstrSet> {
    let filter = &project.syncback_rules.as_ref()?.ignore_properties;
    let mut set = UstrSet::default();

    let database = rbx_reflection_database::get().unwrap();
    let mut current_class_name = new_inst.class.as_str();

    loop {
        if let Some(list) = filter.get(&ustr(current_class_name)) {
            set.extend(list)
        }

        let class = database.classes.get(current_class_name)?;
        if let Some(super_class) = class.superclass {
            current_class_name = super_class;
        } else {
            break;
        }
    }

    Some(set)
}

/// Produces a list of descendants in the WeakDom such that all children come
/// before their parents.
fn descendants(dom: &WeakDom, root_ref: Ref) -> Vec<Ref> {
    let mut queue = VecDeque::new();
    let mut ordered = Vec::new();
    queue.push_front(root_ref);

    while let Some(referent) = queue.pop_front() {
        let inst = dom
            .get_by_ref(referent)
            .expect("Invariant: WeakDom had a Ref that wasn't inside it");
        ordered.push(referent);
        for child in inst.children() {
            queue.push_back(*child)
        }
    }

    ordered
}

/// Removes the children of `new`'s root that are not also children of `old`'s
/// root.
///
/// This does not care about duplicates, and only filters based on names and
/// class names.
fn strip_unknown_root_children(new: &mut WeakDom, old: &RojoTree) {
    let old_root = old.root();
    let old_root_children: HashMap<&str, InstanceWithMeta> = old_root
        .children()
        .iter()
        .map(|referent| {
            let inst = old
                .get_instance(*referent)
                .expect("all children of a DOM's root should exist");
            (inst.name(), inst)
        })
        .collect();

    let root_children = new.root().children().to_vec();

    for child_ref in root_children {
        let child = new
            .get_by_ref(child_ref)
            .expect("all children of the root should exist in the DOM");
        if let Some(old) = old_root_children.get(child.name.as_str()) {
            if old.class_name() == child.class {
                continue;
            }
        }
        log::trace!("Pruning root child {} of class {}", child.name, child.class);
        new.destroy(child_ref);
    }
}

#[cfg(test)]
mod test {
    use rbx_dom_weak::InstanceBuilder;

    use crate::{
        snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot},
        snapshot_middleware::snapshot_from_vfs,
    };

    use super::*;

    #[test]
    fn tree_ignores_match_whole_path_components() {
        assert!(tree_is_ignored("Parent/Keep", "Parent/Keep"));
        assert!(tree_is_ignored("Parent/Keep/Child", "Parent/Keep/"));
        assert!(!tree_is_ignored("Parent/KeepOther", "Parent/Keep"));
    }

    #[test]
    fn path_ignores_match_parent_relative_mount_spellings() {
        let globs = Some(
            rules(&["src/Gone/Keep.luau"], Some(false))
                .compile_globs()
                .unwrap(),
        );
        assert!(!is_valid_path(
            &globs,
            Path::new("/project"),
            Path::new("/project/src/../src/Gone/Keep.luau"),
        ));
        assert!(is_valid_path(
            &globs,
            Path::new("/project"),
            Path::new("/project/src/../src/Gone/Drop.luau"),
        ));
        assert_eq!(
            normalize_syncback_path(Path::new("../shared/../kept")),
            Path::new("../kept")
        );
    }

    #[test]
    fn git_directories_are_ignored_at_any_depth() {
        let base = Path::new("/project");
        for path in [
            ".git",
            ".git/config",
            "src/Nested/.git/config",
            "src/.GIT/objects",
        ] {
            assert!(!is_valid_path(&None, base, &base.join(path)), "{path}");
        }
        assert!(is_valid_path(&None, base, &base.join("src/.gitkeep")));
    }

    #[test]
    fn ignored_files_prevent_destructive_model_fallback() {
        let (_directory, vfs, project, _) = syncback_fixture(None);
        let root = project.folder_location();
        let worker = root.join("src/Worker");
        vfs.create_dir(&worker).unwrap();
        vfs.write(worker.join("Keep.luau"), "return 'protected'")
            .unwrap();
        vfs.write(
            &project.file_location,
            r#"{"name":"Root","tree":{"$path":"src"},"syncbackRules":{"ignorePaths":["src/Worker/Keep.luau"]}}"#,
        )
        .unwrap();
        let project = Project::load_exact(&vfs, &project.file_location, None).unwrap();
        let mut old_tree = rebuild_project(&vfs, &project);
        let new_tree = WeakDom::new(
            InstanceBuilder::new("Folder").with_name("Root").with_child(
                InstanceBuilder::new("Folder")
                    .with_name("Worker")
                    .with_children(["return 1", "return 2"].map(|source| {
                        InstanceBuilder::new("ModuleScript")
                            .with_name("Duplicate")
                            .with_property("Source", source)
                    })),
            ),
        );

        let result = syncback_loop(&vfs, &mut old_tree, new_tree, &project);

        assert!(
            result.is_err(),
            "fallback must preserve ignored source files"
        );
        assert_eq!(
            vfs.read_to_string(worker.join("Keep.luau"))
                .unwrap()
                .as_str(),
            "return 'protected'"
        );
        assert!(!vfs.exists(root.join("src/Worker.rbxm")).unwrap());
    }

    fn syncback_fixture(
        existing_init: Option<&str>,
    ) -> (tempfile::TempDir, Vfs, Project, RojoTree) {
        let directory = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(directory.path()).unwrap();
        fs_err::create_dir(root.join("src")).unwrap();
        fs_err::write(
            root.join("default.project.json"),
            r#"{"name":"Root","emitLegacyScripts":true,"tree":{"$path":"src"}}"#,
        )
        .unwrap();
        if let Some(init) = existing_init {
            fs_err::create_dir(root.join("src/Worker")).unwrap();
            fs_err::write(root.join("src/Worker").join(init), "return 'old source'\n").unwrap();
        }
        let vfs = Vfs::new_default().unwrap();
        vfs.set_watch_enabled(false);
        let project = Project::load_exact(&vfs, &root.join("default.project.json"), None).unwrap();
        let tree = rebuild_project(&vfs, &project);

        (directory, vfs, project, tree)
    }

    fn rebuild_project(vfs: &Vfs, project: &Project) -> RojoTree {
        RojoTree::new(
            snapshot_from_vfs(&InstanceContext::default(), vfs, &project.file_location)
                .unwrap()
                .unwrap(),
        )
    }

    #[test]
    fn syncback_rejects_directory_traversal_before_planning_writes() {
        let (_directory, vfs, project, mut old_tree) = syncback_fixture(None);
        let escaped = project.folder_location().join("escaped");
        vfs.create_dir_all(&escaped).unwrap();
        let victim = escaped.join("victim.txt");
        vfs.write(&victim, b"original").unwrap();
        let new_tree = WeakDom::new(
            InstanceBuilder::new("Folder").with_name("Root").with_child(
                InstanceBuilder::new("Folder")
                    .with_name("../escaped")
                    .with_child(
                        InstanceBuilder::new("StringValue")
                            .with_name("victim")
                            .with_property("Value", "overwritten"),
                    ),
            ),
        );

        let error = syncback_loop(&vfs, &mut old_tree, new_tree, &project)
            .err()
            .expect("a new directory cannot escape its parent");
        assert!(format!("{error:#}").contains("name '../escaped' is not legal"));
        assert_eq!(vfs.read_to_string(&victim).unwrap().as_str(), "original");
    }

    #[test]
    fn reserved_init_children_preserve_sources_and_structure_after_syncback_and_build() {
        let parent_source = "return 'parent source'\n";
        let child_source = "return 'child source'\n";
        let table_contents =
            r#"[{"key":"Child","source":"Child source","values":{"en":"Child translation"}}]"#;
        for (class, name, property, contents, init_name) in [
            ("ModuleScript", "init", "Source", child_source, "init.luau"),
            ("ModuleScript", "Init", "Source", child_source, "init.luau"),
            (
                "ModuleScript",
                "init.server",
                "Source",
                child_source,
                "init.luau",
            ),
            (
                "ModuleScript",
                "init.client",
                "Source",
                child_source,
                "init.luau",
            ),
            (
                "ModuleScript",
                "INIT.Server",
                "Source",
                child_source,
                "init.luau",
            ),
            (
                "ModuleScript",
                "init.plugin",
                "Source",
                child_source,
                "init.luau",
            ),
            ("Script", "init", "Source", child_source, "init.server.luau"),
            ("Script", "INIT", "Source", child_source, "init.server.luau"),
            (
                "LocalScript",
                "init",
                "Source",
                child_source,
                "init.client.luau",
            ),
            (
                "LocalizationTable",
                "init",
                "Contents",
                table_contents,
                "init.csv",
            ),
            (
                "LocalizationTable",
                "Init",
                "Contents",
                table_contents,
                "init.csv",
            ),
        ] {
            for (parent_class, existing_init) in [
                ("Folder", None),
                ("ModuleScript", None),
                ("ModuleScript", Some("init.lua")),
                ("ModuleScript", Some("init.luau")),
            ] {
                let (_directory, vfs, project, mut old_tree) = syncback_fixture(existing_init);
                let mut parent = InstanceBuilder::new(parent_class)
                    .with_name("Worker")
                    .with_child(
                        InstanceBuilder::new(class)
                            .with_name(name)
                            .with_property(property, contents),
                    );
                if parent_class == "ModuleScript" {
                    parent = parent.with_property("Source", parent_source);
                }
                let new_tree = WeakDom::new(
                    InstanceBuilder::new("Folder")
                        .with_name("Root")
                        .with_child(parent),
                );

                let fs_snapshot = syncback_loop(&vfs, &mut old_tree, new_tree, &project)
                    .unwrap_or_else(|error| {
                        panic!("{parent_class}/{class} {name} with {existing_init:?}: {error:#}")
                    });
                fs_snapshot
                    .write_to_vfs(project.folder_location(), &vfs)
                    .unwrap();
                let worker_path = project.folder_location().join("src/Worker");
                assert!(vfs.exists(worker_path.join(name).join(init_name)).unwrap());
                if parent_class == "ModuleScript" {
                    let path = worker_path.join(existing_init.unwrap_or("init.luau"));
                    assert_eq!(vfs.read_to_string(path).unwrap().as_str(), parent_source);
                }

                let rebuilt = rebuild_project(&vfs, &project);
                assert_eq!(rebuilt.root().children().len(), 1);
                let parent = rebuilt.get_instance(rebuilt.root().children()[0]).unwrap();
                assert_eq!(parent.name(), "Worker");
                assert_eq!(parent.class_name().as_str(), parent_class);
                if parent_class == "ModuleScript" {
                    assert_eq!(
                        parent.properties().get(&ustr("Source")),
                        Some(&Variant::String(parent_source.to_owned()))
                    );
                }
                assert_eq!(parent.children().len(), 1);
                let child = rebuilt.get_instance(parent.children()[0]).unwrap();
                assert_eq!(child.name(), name);
                assert_eq!(child.class_name().as_str(), class);
                assert!(child.children().is_empty());
                if property == "Contents" {
                    let Some(Variant::String(rebuilt_contents)) =
                        child.properties().get(&ustr(property))
                    else {
                        panic!("rebuilt LocalizationTable must have Contents");
                    };
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(rebuilt_contents).unwrap(),
                        serde_json::from_str::<serde_json::Value>(contents).unwrap()
                    );
                } else {
                    assert_eq!(
                        child.properties().get(&ustr(property)),
                        Some(&Variant::String(contents.to_owned()))
                    );
                }
            }
        }
    }

    #[test]
    fn existing_and_explicit_middleware_are_preserved_for_reserved_names() {
        let (_directory, vfs, project, _) = syncback_fixture(None);
        for middleware in [
            Middleware::ModuleScript,
            Middleware::ServerScript,
            Middleware::ClientScript,
            Middleware::PluginScript,
            Middleware::Csv,
        ] {
            let old_tree = RojoTree::new(
                InstanceSnapshot::new()
                    .name("init")
                    .class_name("ModuleScript")
                    .metadata(InstanceMetadata::new().middleware(middleware)),
            );
            let new_tree = WeakDom::new(InstanceBuilder::new("ModuleScript").with_name("init"));
            let mut snapshot = SyncbackSnapshot {
                data: SyncbackData {
                    vfs: &vfs,
                    old_tree: &old_tree,
                    new_tree: &new_tree,
                    project: &project,
                },
                old: Some(old_tree.get_root_id()),
                new: new_tree.root_ref(),
                path: project.folder_location().join("src/unchanged"),
                middleware: None,
            };
            assert_eq!(get_best_middleware(&snapshot), middleware);

            snapshot.old = None;
            snapshot.middleware = Some(middleware);
            assert_eq!(get_best_middleware(&snapshot), middleware);
        }
    }

    #[test]
    fn ordinary_new_leaf_names_keep_file_middleware() {
        let (_directory, vfs, project, old_tree) = syncback_fixture(None);
        for (class, name, expected) in [
            ("ModuleScript", "Worker", Middleware::ModuleScript),
            ("ModuleScript", "initialize", Middleware::ModuleScript),
            ("ModuleScript", "init.csv", Middleware::ModuleScript),
            ("Script", "init.server", Middleware::ServerScript),
            ("LocalScript", "init.client", Middleware::ClientScript),
            ("LocalizationTable", "init.csv", Middleware::Csv),
        ] {
            let new_tree = WeakDom::new(InstanceBuilder::new(class).with_name(name));
            let snapshot = SyncbackSnapshot {
                data: SyncbackData {
                    vfs: &vfs,
                    old_tree: &old_tree,
                    new_tree: &new_tree,
                    project: &project,
                },
                old: None,
                new: new_tree.root_ref(),
                path: project.folder_location().join("src/unused"),
                middleware: None,
            };
            assert_eq!(get_best_middleware(&snapshot), expected, "{class} {name}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn existing_windows_device_name_roundtrips_on_other_platforms() {
        let (_directory, vfs, project, _) = syncback_fixture(None);
        let path = project.folder_location().join("src/con.txt");
        vfs.write(&path, "original").unwrap();
        let mut old_tree = rebuild_project(&vfs, &project);
        let new_tree = WeakDom::new(
            InstanceBuilder::new("Folder").with_name("Root").with_child(
                InstanceBuilder::new("StringValue")
                    .with_name("con")
                    .with_property("Value", "updated"),
            ),
        );

        let fs_snapshot = syncback_loop(&vfs, &mut old_tree, new_tree, &project).unwrap();
        fs_snapshot
            .write_to_vfs(project.folder_location(), &vfs)
            .unwrap();
        assert_eq!(vfs.read_to_string(&path).unwrap().as_str(), "updated");
        let rebuilt = rebuild_project(&vfs, &project);
        assert_eq!(rebuilt.root().children().len(), 1);
        let child = rebuilt.get_instance(rebuilt.root().children()[0]).unwrap();
        assert_eq!(child.name(), "con");
        assert_eq!(child.class_name().as_str(), "StringValue");
        assert_eq!(
            child.properties().get(&ustr("Value")),
            Some(&Variant::String("updated".to_owned()))
        );
    }

    fn rules(ignore_paths: &[&str], create_ignore_dir_paths: Option<bool>) -> SyncbackRules {
        SyncbackRules {
            ignore_trees: Vec::new(),
            ignore_paths: ignore_paths.iter().map(|s| s.to_string()).collect(),
            ignore_properties: IndexMap::new(),
            sync_current_camera: None,
            sync_unscriptable: None,
            ignore_referents: None,
            create_ignore_dir_paths,
        }
    }

    #[test]
    fn ignore_paths_negation() {
        let globs = Some(
            rules(&["**/*.lua", "!keep.lua"], Some(false))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        // A later negation re-includes a path matched by an earlier pattern.
        assert!(!is_valid_path(&globs, base, Path::new("/test/foo.lua")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep.lua")));
        // Paths matched by no rule are valid.
        assert!(is_valid_path(&globs, base, Path::new("/test/plain.txt")));
    }

    #[test]
    fn ignore_paths_negation_with_dir_expansion() {
        // With `create_ignore_dir_paths`, a negated `foo/**` pattern should also
        // re-include the `foo` directory itself, mirroring the file rule.
        let globs = Some(
            rules(&["**/*", "!keep/**"], Some(true))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        assert!(!is_valid_path(&globs, base, Path::new("/test/drop/a.lua")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep")));
        assert!(is_valid_path(&globs, base, Path::new("/test/keep/a.lua")));
    }

    #[test]
    fn ignore_paths_escaped_bang_is_literal() {
        // `\!literal.lua` should ignore a file literally named `!literal.lua`
        // rather than being parsed as a negation.
        let globs = Some(
            rules(&[r"\!literal.lua"], Some(false))
                .compile_globs()
                .unwrap(),
        );
        let base = Path::new("/test");

        assert!(!globs.as_ref().unwrap()[0].is_negation());
        assert!(!is_valid_path(
            &globs,
            base,
            Path::new("/test/!literal.lua")
        ));
    }
}
