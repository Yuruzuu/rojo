use std::{path::Path, str};

use anyhow::Context as _;
use memofs::Vfs;
use rbx_dom_weak::{
    types::{Enum, Variant},
    ustr, HashMapExt as _, UstrMap,
};

use crate::{
    snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot},
    syncback::{FsSnapshot, SyncbackReturn, SyncbackSnapshot},
};

use super::{
    dir::{snapshot_dir_no_meta, syncback_dir_no_meta},
    meta_file::{AdjacentMetadata, DirectoryMetadata},
    Middleware, PathExt as _,
};

#[derive(Debug)]
pub enum ScriptType {
    Server,
    Client,
    Module,
    Plugin,
    LegacyServer,
    LegacyClient,
    RunContextServer,
    RunContextClient,
}

/// Core routine for turning Lua files into snapshots.
pub fn snapshot_lua(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
    script_type: ScriptType,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let run_context_enums = &rbx_reflection_database::get()
        .unwrap()
        .enums
        .get("RunContext")
        .expect("Unable to get RunContext enums!")
        .items;

    let (class_name, run_context) = match script_type {
        ScriptType::Server => {
            if context.emit_legacy_scripts {
                ("Script", run_context_enums.get("Legacy"))
            } else {
                ("Script", run_context_enums.get("Server"))
            }
        }
        ScriptType::Client => {
            if context.emit_legacy_scripts {
                ("LocalScript", None)
            } else {
                ("Script", run_context_enums.get("Client"))
            }
        }
        ScriptType::Module => ("ModuleScript", None),
        ScriptType::Plugin => ("Script", run_context_enums.get("Plugin")),
        ScriptType::LegacyServer => ("Script", run_context_enums.get("Legacy")),
        ScriptType::LegacyClient => ("LocalScript", None),
        ScriptType::RunContextServer => ("Script", run_context_enums.get("Server")),
        ScriptType::RunContextClient => ("Script", run_context_enums.get("Client")),
    };

    let contents = vfs.read_to_string_lf_normalized(path)?;
    let contents_str = contents.as_str();

    let mut properties = UstrMap::with_capacity(2);
    properties.insert(ustr("Source"), contents_str.into());

    if let Some(run_context) = run_context {
        properties.insert(
            ustr("RunContext"),
            Enum::from_u32(run_context.to_owned()).into(),
        );
    }

    let mut snapshot = InstanceSnapshot::new()
        .name(name)
        .class_name(class_name)
        .properties(properties)
        .metadata(
            InstanceMetadata::new()
                .instigating_source(path)
                .relevant_paths(vec![vfs.canonicalize(path)?])
                .context(context),
        );

    AdjacentMetadata::read_and_apply_all(vfs, path, name, &mut snapshot)?;

    Ok(Some(snapshot))
}

/// Attempts to snapshot an 'init' Lua script contained inside of a folder with
/// the given name.
///
/// Scripts named `init.lua`, `init.server.lua`, or `init.client.lua` usurp
/// their parents, which acts similarly to `__init__.py` from the Python world.
pub fn snapshot_lua_init(
    context: &InstanceContext,
    vfs: &Vfs,
    init_path: &Path,
    name: &str,
    script_type: ScriptType,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let folder_path = init_path.parent().unwrap();
    let dir_snapshot = snapshot_dir_no_meta(context, vfs, folder_path, name)?.unwrap();

    if dir_snapshot.class_name != "Folder" {
        anyhow::bail!(
            "init.lua, init.server.lua, and init.client.lua can \
             only be used if the instance produced by the containing \
             directory would be a Folder.\n\
             \n\
             The directory {} turned into an instance of class {}.",
            folder_path.display(),
            dir_snapshot.class_name
        );
    }

    let mut init_snapshot =
        snapshot_lua(context, vfs, init_path, &dir_snapshot.name, script_type)?.unwrap();

    init_snapshot.children = dir_snapshot.children;
    init_snapshot.metadata = dir_snapshot.metadata;
    // The directory snapshot middleware includes all possible init paths
    // so we don't need to add it here.

    DirectoryMetadata::read_and_apply_all(vfs, folder_path, &mut init_snapshot)?;

    Ok(Some(init_snapshot))
}

pub fn syncback_lua<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let new_inst = snapshot.new_inst();

    let contents = if let Some(Variant::String(source)) = new_inst.properties.get(&ustr("Source")) {
        source.as_bytes().to_vec()
    } else {
        anyhow::bail!("Scripts must have a `Source` property that is a String")
    };
    let mut fs_snapshot = FsSnapshot::new();
    fs_snapshot.add_file(&snapshot.path, contents);

    let meta = AdjacentMetadata::from_syncback_snapshot(snapshot, snapshot.path.clone())?;
    if let Some(mut meta) = meta {
        // Scripts have relatively few properties that we care about, so shifting
        // is fine.
        meta.properties.shift_remove(&ustr("Source"));

        if !meta.is_empty() {
            let parent_location = snapshot.path.parent_err()?;
            fs_snapshot.add_file(
                parent_location.join(format!("{}.meta.json", new_inst.name)),
                serde_json::to_vec_pretty(&meta).context("cannot serialize metadata")?,
            );
        }
    }

    Ok(SyncbackReturn {
        fs_snapshot,
        // Scripts don't have a child!
        children: Vec::new(),
        removed_children: Vec::new(),
    })
}

pub fn syncback_lua_init<'sync>(
    script_type: ScriptType,
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let new_inst = snapshot.new_inst();
    let (middleware, default_name) = match script_type {
        ScriptType::Server => (Middleware::ServerScriptDir, "init.server.luau"),
        ScriptType::Client => (Middleware::ClientScriptDir, "init.client.luau"),
        ScriptType::Module => (Middleware::ModuleScriptDir, "init.luau"),
        ScriptType::Plugin => (Middleware::PluginScriptDir, "init.plugin.luau"),
        _ => anyhow::bail!("syncback is not yet implemented for {script_type:?}"),
    };
    let (existing_middleware, _, init_path) =
        super::get_dir_middleware(snapshot.vfs(), &snapshot.path)?;
    let path = if existing_middleware == middleware {
        init_path
    } else {
        snapshot.path.join(default_name)
    };

    let contents = if let Some(Variant::String(source)) = new_inst.properties.get(&ustr("Source")) {
        source.as_bytes().to_vec()
    } else {
        anyhow::bail!("Scripts must have a `Source` property that is a String")
    };

    let mut dir_syncback = syncback_dir_no_meta(snapshot)?;
    dir_syncback.fs_snapshot.add_file(&path, contents);

    let meta = DirectoryMetadata::from_syncback_snapshot(snapshot, snapshot.path.clone())?;
    if let Some(mut meta) = meta {
        // Scripts have relatively few properties that we care about, so shifting
        // is fine.
        meta.properties.shift_remove(&ustr("Source"));

        if !meta.is_empty() {
            dir_syncback.fs_snapshot.add_file(
                snapshot.path.join("init.meta.json"),
                serde_json::to_vec_pretty(&meta)
                    .context("could not serialize new init.meta.json")?,
            );
        }
    }

    Ok(dir_syncback)
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};
    use rbx_dom_weak::{types::Attributes, InstanceBuilder, WeakDom};

    use crate::{snapshot::RojoTree, syncback::syncback_loop, Project};

    fn instance_builder(snapshot: InstanceSnapshot) -> InstanceBuilder {
        InstanceBuilder::new(snapshot.class_name)
            .with_name(snapshot.name.into_owned())
            .with_properties(snapshot.properties)
            .with_children(snapshot.children.into_iter().map(instance_builder))
    }

    fn syncback_init_fixture(files: VfsSnapshot, edit: impl FnOnce(&mut InstanceSnapshot)) -> Vfs {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project",
            VfsSnapshot::dir([
                (
                    "default.project.json",
                    VfsSnapshot::file(r#"{"emitLegacyScripts":true,"tree":{"$path":"src"}}"#),
                ),
                ("src", files),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);
        let project_path = Path::new("/project/default.project.json");
        let project = Project::load_exact(&vfs, project_path, None).unwrap();
        let snapshot =
            super::super::snapshot_from_vfs(&InstanceContext::default(), &vfs, project_path)
                .unwrap()
                .unwrap();
        let mut new_snapshot = snapshot.clone();
        edit(&mut new_snapshot);

        let mut old_tree = RojoTree::new(snapshot);
        let new_tree = WeakDom::new(instance_builder(new_snapshot));
        let fs_snapshot = syncback_loop(&vfs, &mut old_tree, new_tree, &project).unwrap();
        fs_snapshot
            .write_to_vfs(project.folder_location(), &vfs)
            .unwrap();

        vfs
    }

    #[test]
    fn syncback_init_preserves_existing_lua_paths() {
        for init_name in [
            "init.lua",
            "init.server.lua",
            "init.client.lua",
            "init.plugin.lua",
        ] {
            let vfs = syncback_init_fixture(
                VfsSnapshot::dir([(
                    "Script",
                    VfsSnapshot::dir([(init_name, VfsSnapshot::file("return 'before'"))]),
                )]),
                |snapshot| {
                    snapshot.children[0]
                        .properties
                        .insert(ustr("Source"), "return 'after'".into());
                },
            );
            let init_path = Path::new("/project/src/Script").join(init_name);

            assert_eq!(
                vfs.read_to_string(&init_path).unwrap().as_str(),
                "return 'after'",
                "{init_name} should be updated in place"
            );
            assert!(
                !vfs.exists(init_path.with_extension("luau")).unwrap(),
                "{init_name} should not create a duplicate .luau file"
            );
        }
    }

    #[test]
    fn syncback_init_preserves_extension_priority() {
        for (selected_name, other_name) in [
            ("init.luau", "init.lua"),
            ("init.server.luau", "init.server.lua"),
            ("init.client.luau", "init.client.lua"),
            ("init.plugin.lua", "init.plugin.luau"),
        ] {
            let vfs = syncback_init_fixture(
                VfsSnapshot::dir([(
                    "Script",
                    VfsSnapshot::dir([
                        (selected_name, VfsSnapshot::file("return 'selected'")),
                        (other_name, VfsSnapshot::file("return 'other'")),
                    ]),
                )]),
                |snapshot| {
                    snapshot.children[0]
                        .properties
                        .insert(ustr("Source"), "return 'after'".into());
                },
            );
            let directory = Path::new("/project/src/Script");

            assert_eq!(
                vfs.read_to_string(directory.join(selected_name))
                    .unwrap()
                    .as_str(),
                "return 'after'",
                "{selected_name} should retain priority"
            );
            assert_eq!(
                vfs.read_to_string(directory.join(other_name))
                    .unwrap()
                    .as_str(),
                "return 'other'"
            );
        }
    }

    #[test]
    fn syncback_init_updates_existing_directory_metadata() {
        for init_name in [
            "init.lua",
            "init.server.lua",
            "init.client.lua",
            "init.plugin.lua",
        ] {
            let vfs = syncback_init_fixture(
                VfsSnapshot::dir([(
                    "Script",
                    VfsSnapshot::dir([
                        (init_name, VfsSnapshot::file("return 'unchanged'")),
                        (
                            "init.meta.json",
                            VfsSnapshot::file(
                                r#"{
                                    "ignoreUnknownInstances": true,
                                    "attributes": {"State": "before", "Removed": true}
                                }"#,
                            ),
                        ),
                    ]),
                )]),
                |snapshot| {
                    let script = &mut snapshot.children[0];
                    let mut attributes = Attributes::new();
                    attributes.insert("State".into(), "after".into());
                    attributes.insert("Added".into(), true.into());
                    script
                        .properties
                        .insert(ustr("Attributes"), attributes.into());
                    script.properties.insert(ustr("Archivable"), false.into());
                },
            );
            let directory = Path::new("/project/src/Script");
            let metadata: serde_json::Value = serde_json::from_str(
                vfs.read_to_string(directory.join("init.meta.json"))
                    .unwrap()
                    .as_str(),
            )
            .unwrap();

            assert_eq!(
                metadata["attributes"],
                serde_json::json!({"State": "after", "Added": true}),
                "{init_name} should sync attribute edits and removals"
            );
            assert_eq!(metadata["properties"]["Archivable"], false);
            assert!(metadata["properties"].get("Source").is_none());
            assert_eq!(metadata["ignoreUnknownInstances"], true);
            assert_eq!(
                vfs.read_to_string(directory.join(init_name))
                    .unwrap()
                    .as_str(),
                "return 'unchanged'"
            );
        }
    }

    #[test]
    fn syncback_init_uses_luau_defaults_for_new_directories() {
        for (class_name, init_name) in [
            ("ModuleScript", "init.luau"),
            ("Script", "init.server.luau"),
            ("LocalScript", "init.client.luau"),
        ] {
            let vfs = syncback_init_fixture(VfsSnapshot::empty_dir(), |snapshot| {
                snapshot.children.push(
                    InstanceSnapshot::new()
                        .name("Script")
                        .class_name(class_name)
                        .property("Source", "return 'new'")
                        .children(vec![InstanceSnapshot::new()
                            .name("Child")
                            .class_name("Folder")]),
                );
            });
            let directory = Path::new("/project/src/Script");
            let init_path = directory.join(init_name);

            assert_eq!(
                vfs.read_to_string(&init_path).unwrap().as_str(),
                "return 'new'"
            );
            assert!(!vfs.exists(init_path.with_extension("lua")).unwrap());
            assert!(vfs.exists(directory.join("Child/.gitkeep")).unwrap());
        }
    }

    #[test]
    fn class_module_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.lua"),
            "foo",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_module_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.lua"),
            "foo",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn plugin_module_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.plugin.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.plugin.lua"),
            "foo",
            ScriptType::Plugin,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_server_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.server.lua"),
            "foo",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_server_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.server.lua"),
            "foo",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_client_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.client.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.client.lua"),
            "foo",
            ScriptType::Client,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_client_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.client.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.client.lua"),
            "foo",
            ScriptType::Client,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn init_module_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([("init.lua", VfsSnapshot::file("Hello!"))]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua_init(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/root/init.lua"),
            "root",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn init_module_from_vfs_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/root",
            VfsSnapshot::dir([
                ("init.lua", VfsSnapshot::file("Hello!")),
                (
                    "init.meta.json",
                    VfsSnapshot::file(r#"{"id": "manually specified"}"#),
                ),
            ]),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua_init(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/root/init.lua"),
            "root",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_module_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/foo.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "ignoreUnknownInstances": true
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.lua"),
            "foo",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_module_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/foo.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "ignoreUnknownInstances": true
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.lua"),
            "foo",
            ScriptType::Module,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_script_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/foo.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "ignoreUnknownInstances": true
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/foo.server.lua"),
            "foo",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_script_with_meta() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/foo.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "ignoreUnknownInstances": true
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/foo.server.lua"),
            "foo",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn class_script_disabled() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/bar.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/bar.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "properties": {
                            "Disabled": true
                        }
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(true)),
            &vfs,
            Path::new("/bar.server.lua"),
            "bar",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }

    #[test]
    fn runcontext_script_disabled() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/bar.server.lua", VfsSnapshot::file("Hello there!"))
            .unwrap();
        imfs.load_snapshot(
            "/bar.meta.json",
            VfsSnapshot::file(
                r#"
                    {
                        "properties": {
                            "Disabled": true
                        }
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_lua(
            &InstanceContext::with_emit_legacy_scripts(Some(false)),
            &vfs,
            Path::new("/bar.server.lua"),
            "bar",
            ScriptType::Server,
        )
        .unwrap()
        .unwrap();

        insta::with_settings!({ sort_maps => true }, {
            insta::assert_yaml_snapshot!(instance_snapshot);
        });
    }
}
