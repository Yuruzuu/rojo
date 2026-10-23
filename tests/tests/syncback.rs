use std::ffi::OsStr;

use insta::assert_snapshot;

use crate::rojo_test::syncback_util::{run_syncback_test, snapshot_rbxm};

macro_rules! syncback_tests {
    ($($test_name:ident => $list:expr$(,)?),*) => {$(
        #[test]
        fn $test_name() {
            run_syncback_test(stringify!($test_name), |path| {
                for name in $list {
                    let snapshot_name = format!(concat!(stringify!($test_name), "-{}"), name);
                    let new = path.join::<&str>(name);
                    if !new.exists() {
                        panic!("the path stub '{}' does not exist after syncback runs. consider double checking for typos.", name);
                    }
                    if let Some("rbxm") = new.extension().and_then(OsStr::to_str) {
                        let content = fs_err::read(new).unwrap();
                        snapshot_rbxm(&snapshot_name, content, name);
                    } else {
                        let content = fs_err::read_to_string(new).unwrap();
                        assert_snapshot!(snapshot_name, content, name);
                    }
                }
            });
        }
    )*};
}

syncback_tests! {
    // Ensures that there's only one copy written to disk if navigating a
    // project file might yield two copies
    child_but_not => ["OnlyOneCopy/child_of_one.luau", "ReplicatedStorage/child_replicated_storage.luau"],
    // Ensures that syncback works with CSVs
    csv => ["src/csv_init/init.csv", "src/csv.csv"],
    // Ensures that if a RojoId is duplicated somewhere in the project, it's
    // rewritten rather than synced back as a conflict
    duplicate_rojo_id => ["container.model.json"],
    // Ensures that the `ignorePaths` setting works for additions
    ignore_paths_adding => ["src/int_value.model.json", "src/subfolder/string_value.txt"],
    // Ensures that the `ignorePaths` setting works for `init` files
    ignore_paths_init => ["src/non-init.luau", "src/init-file/init.luau"],
    // Ensures that the `ignorePaths` setting works for removals
    ignore_paths_removing => ["src/Message.rbxm"],
    // Ensures that `ignoreTrees` works for additions
    ignore_trees_adding => [],
    // Ensures that `ignoreTrees` works for removals
    ignore_trees_removing => [],
    // Ensures that all of the JSON middlewares are handled as expected
    json_middlewares => ["src/dir_with_meta/init.meta.json", "src/model_json.model.json", "src/project_json.project.json"],
    // Ensures projects that refer to other projects work as expected
    nested_projects => ["nested.project.json", "string_value.txt"],
    // Ensures files that are ignored by nested projects are picked up if
    // they're included in second project. Unusual but perfectly workable
    // pattern that syncback has to support.
    nested_projects_weird => ["src/modules/ClientModule.luau", "src/modules/ServerModule.luau"],
    // Ensures that projects respect `init` files when they're directly referenced from a node
    project_init => ["src/init.luau"],
    // Ensures that projects can be reserialized by syncback and that
    // default.project.json doesn't change unexpectedly.
    project_reserialize => ["attribute_mismatch.luau", "property_mismatch.project.json"],
    // Confirms that Instances that cannot serialize as directories serialize as rbxms
    rbxm_fallback => ["src/ChildWithDuplicates.rbxm"],
    // Ensures that ref properties are linked properly on the file system
    ref_properties => ["src/pointer.model.json", "src/target.model.json"],
    // Ensures that ref properties are linked when no attributes are manually
    // set in the DataModel
    ref_properties_blank => ["src/pointer.model.json", "src/target.meta.json", "src/target.txt"],
    // Ensures that if there is a conflict in RojoRefs, one of them is rewritten.
    ref_properties_conflict => ["src/Pointer_2.model.json", "src/Target_2.model.json"],
    // Ensures that having multiple pointers that are aimed at the same target doesn't trigger ref rewrites.
    ref_properties_duplicate => [],
    // Ensures that ref properties that point to nothing after the prune both
    // do not leave any trace of themselves
    ref_properties_pruned => ["src/Pointer1.model.json", "src/Pointer2.model.json", "src/Pointer3.model.json"],
    // Ensures that the old middleware is respected during syncback
    respect_old_middleware => ["default.project.json", "src/model_json.model.json", "src/rbxm.rbxm", "src/rbxmx.rbxmx"],
    // Ensures that the `$schema` field roundtrips with syncback
    schema_roundtrip => ["default.project.json", "src/model.model.json", "src/init/init.meta.json", "src/adjacent.meta.json"],
    // Ensures that StringValues inside project files are written to the
    // project file, but only if they don't have `$path` set
    string_value_project => ["default.project.json"],
    // Ensures that sync rules are respected. This is really just a test to
    // ensure it uses the old path when possible, but we want the coverage.
    sync_rules => ["src/module.modulescript", "src/text.text"],
    // Ensures that the `syncUnscriptable` setting works
    unscriptable_properties => ["default.project.json"],
    // Ensures that syncback correctly removes default values from projects rather
    // than leaving an incorrect value.
    project_default_properties_remove => ["default.project.json"],
}

#[test]
fn small_numeric_attribute_edits_are_written() {
    use std::process::Command;

    use rbx_dom_weak::{types::Attributes, InstanceBuilder, WeakDom};
    use tempfile::tempdir;

    use crate::rojo_test::io_util::ROJO_PATH;

    let directory = tempdir().unwrap();
    let project = directory.path().join("default.project.json");
    let source = directory.path().join("src");
    let settings = source.join("Settings");
    let metadata = settings.join("init.meta.json");
    let input = directory.path().join("input.rbxmx");
    fs_err::create_dir_all(&settings).unwrap();
    fs_err::write(&project, r#"{"name":"NumericEdit","tree":{"$path":"src"}}"#).unwrap();
    fs_err::write(&metadata, r#"{"attributes":{"Speed":0.11}}"#).unwrap();

    let mut attributes = Attributes::new();
    attributes.insert("Speed".into(), 0.14_f64.into());
    let dom = WeakDom::new(
        InstanceBuilder::new("Folder")
            .with_name("NumericEdit")
            .with_child(
                InstanceBuilder::new("Folder")
                    .with_name("Settings")
                    .with_property("Attributes", attributes),
            ),
    );
    rbx_xml::to_writer_default(
        fs_err::File::create(&input).unwrap(),
        &dom,
        &[dom.root_ref()],
    )
    .unwrap();

    let run = || {
        Command::new(ROJO_PATH)
            .arg("syncback")
            .arg(&project)
            .arg("--input")
            .arg(&input)
            .args(["--non-interactive", "--list"])
            .output()
            .unwrap()
    };
    let output = run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let result: serde_json::Value =
        serde_json::from_slice(&fs_err::read(&metadata).unwrap()).unwrap();
    assert_eq!(result["attributes"]["Speed"], 0.14);

    let output = run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "syncback should converge after the edit"
    );
}

#[test]
fn new_children_under_relative_root_are_not_pruned() {
    use std::process::Command;

    use rbx_dom_weak::{InstanceBuilder, WeakDom};
    use tempfile::tempdir;

    use crate::rojo_test::io_util::ROJO_PATH;

    let directory = tempdir().unwrap();
    let project = directory.path().join("default.project.json");
    let source = directory.path().join("src");
    let input = directory.path().join("input.rbxmx");
    fs_err::create_dir(&source).unwrap();
    fs_err::write(&project, r#"{"name":"Root","tree":{"$path":"src"}}"#).unwrap();
    let dom = WeakDom::new(
        InstanceBuilder::new("Folder").with_name("Root").with_child(
            InstanceBuilder::new("ModuleScript")
                .with_name("Added")
                .with_property("Source", "return 42"),
        ),
    );
    rbx_xml::to_writer_default(
        fs_err::File::create(&input).unwrap(),
        &dom,
        &[dom.root_ref()],
    )
    .unwrap();

    let output = Command::new(ROJO_PATH)
        .arg("syncback")
        .arg(&project)
        .arg("--input")
        .arg(&input)
        .arg("--non-interactive")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs_err::read_to_string(source.join("Added.luau")).unwrap(),
        "return 42"
    );
}
