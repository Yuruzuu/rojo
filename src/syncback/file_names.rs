//! Contains logic for generating new file names for Instances based on their
//! middleware.

use std::borrow::Cow;

use anyhow::Context;
use rbx_dom_weak::Instance;

use crate::{snapshot::InstanceWithMeta, snapshot_middleware::Middleware};

pub fn name_for_inst<'old>(
    middleware: Middleware,
    new_inst: &Instance,
    old_inst: Option<InstanceWithMeta<'old>>,
) -> anyhow::Result<Cow<'old, str>> {
    if let Some(old_inst) = old_inst {
        if let Some(source) = old_inst.metadata().relevant_paths.first() {
            source
                .file_name()
                .and_then(|s| s.to_str())
                .map(Cow::Borrowed)
                .context("sources on the file system should be valid unicode and not be stubs")
        } else {
            // This is technically not /always/ true, but we want to avoid
            // running syncback on anything that has no instigating source
            // anyway.
            anyhow::bail!(
                "members of 'old' trees should have an instigating source. Somehow, {} did not.",
                old_inst.name(),
            );
        }
    } else {
        let name = &new_inst.name;
        validate_new_file_name(name)
            .with_context(|| format!("name '{name}' is not legal to write to the file system"))?;

        Ok(match middleware {
            Middleware::Dir
            | Middleware::CsvDir
            | Middleware::ServerScriptDir
            | Middleware::ClientScriptDir
            | Middleware::PluginScriptDir
            | Middleware::ModuleScriptDir => Cow::Owned(name.clone()),
            _ => {
                let extension = extension_for_middleware(middleware);
                Cow::Owned(format!("{name}.{extension}"))
            }
        })
    }
}

/// Returns the extension a provided piece of middleware is supposed to use.
pub fn extension_for_middleware(middleware: Middleware) -> &'static str {
    match middleware {
        Middleware::Csv => "csv",
        Middleware::JsonModel => "model.json",
        Middleware::Json => "json",
        Middleware::ServerScript => "server.luau",
        Middleware::ClientScript => "client.luau",
        Middleware::ModuleScript => "luau",
        Middleware::PluginScript => "plugin.luau",
        Middleware::Project => "project.json",
        Middleware::Rbxm => "rbxm",
        Middleware::Rbxmx => "rbxmx",
        Middleware::Toml => "toml",
        Middleware::Text => "txt",
        Middleware::Yaml => "yml",

        Middleware::LegacyServerScript
        | Middleware::LegacyClientScript
        | Middleware::RunContextServerScript
        | Middleware::RunContextClientScript => {
            todo!("syncback does not work on the middleware {middleware:?} yet")
        }
        // These are manually specified and not `_` to guard against future
        // middleware additions missing this function.
        Middleware::Ignore => unimplemented!("syncback does not work on Ignore middleware"),
        Middleware::Dir
        | Middleware::CsvDir
        | Middleware::ServerScriptDir
        | Middleware::ClientScriptDir
        | Middleware::PluginScriptDir
        | Middleware::ModuleScriptDir => {
            unimplemented!("directory middleware requires special treatment")
        }
    }
}

/// A list of file names that are not valid on Windows.
const INVALID_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

const ADDITIONAL_WINDOWS_DEVICE_NAMES: [&str; 8] = [
    "COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³", "CONIN$", "CONOUT$",
];

/// A list of all characters that are outright forbidden to be included
/// in a file's name.
const FORBIDDEN_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '|', '?', '*', '\\'];

/// Validates a provided file name to ensure it's allowed on the file system. An
/// error is returned if the name isn't allowed, indicating why.
/// This takes into account rules for Windows, MacOS, and Linux.
///
/// In practice however, these broadly overlap so the only unexpected behavior
/// is Windows, where there are 22 reserved names.
pub fn validate_file_name<S: AsRef<str>>(name: S) -> anyhow::Result<()> {
    let str = name.as_ref();

    if str.ends_with(' ') {
        anyhow::bail!("file names cannot end with a space")
    }
    if str.ends_with('.') {
        anyhow::bail!("file names cannot end with '.'")
    }

    for char in str.chars() {
        if FORBIDDEN_CHARS.contains(&char) {
            anyhow::bail!("file names cannot contain <, >, :, \", /, |, ?, *, or \\")
        } else if char.is_control() {
            anyhow::bail!("file names cannot contain control characters")
        }
    }

    for forbidden in INVALID_WINDOWS_NAMES {
        if str == forbidden {
            anyhow::bail!("files cannot be named {str}")
        }
    }

    Ok(())
}

// Apply stricter portability checks only to new paths. Existing source names
// may be valid on their host filesystem despite Windows device restrictions.
fn validate_new_file_name(name: &str) -> anyhow::Result<()> {
    validate_file_name(name)?;
    if name.is_empty() {
        anyhow::bail!("file names cannot be empty")
    }

    // Windows also treats device stems followed by an extension as devices.
    let stem = name.split('.').next().unwrap().trim_end_matches(' ');
    for forbidden in INVALID_WINDOWS_NAMES
        .into_iter()
        .chain(ADDITIONAL_WINDOWS_DEVICE_NAMES)
    {
        if stem.eq_ignore_ascii_case(forbidden) {
            anyhow::bail!("files cannot be named {name}")
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    use crate::snapshot::{InstanceMetadata, InstanceSnapshot, RojoTree};

    use super::*;

    const MIDDLEWARES: [Middleware; 11] = [
        Middleware::Dir,
        Middleware::CsvDir,
        Middleware::ServerScriptDir,
        Middleware::ClientScriptDir,
        Middleware::PluginScriptDir,
        Middleware::ModuleScriptDir,
        Middleware::Csv,
        Middleware::ServerScript,
        Middleware::ClientScript,
        Middleware::PluginScript,
        Middleware::ModuleScript,
    ];

    #[test]
    fn new_file_and_directory_names_reject_invalid_components() {
        for name in [
            "",
            ".",
            "..",
            "../escaped",
            r"..\escaped",
            "/absolute",
            r"C:\absolute",
            "trailing.",
            "trailing ",
            "colon:name",
            "control\0name",
        ] {
            let tree = WeakDom::new(InstanceBuilder::new("Folder").with_name(name));
            for middleware in MIDDLEWARES {
                assert!(
                    name_for_inst(middleware, tree.root(), None).is_err(),
                    "accepted {name:?} with {middleware:?}"
                );
            }
        }
    }

    #[test]
    fn windows_device_names_are_reserved_with_any_case_or_extension() {
        for device in INVALID_WINDOWS_NAMES
            .into_iter()
            .chain(ADDITIONAL_WINDOWS_DEVICE_NAMES)
        {
            for stem in [device.to_owned(), device.to_lowercase()] {
                for name in [stem.clone(), format!("{stem}.txt"), format!("{stem} .luau")] {
                    let tree = WeakDom::new(InstanceBuilder::new("Folder").with_name(&name));
                    for middleware in MIDDLEWARES {
                        assert!(
                            name_for_inst(middleware, tree.root(), None).is_err(),
                            "accepted {name:?} with {middleware:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn valid_components_keep_their_names() {
        for name in [
            "Worker",
            "init.server",
            ".hidden",
            "conifer",
            "COM10",
            "LPT10.txt",
        ] {
            let tree = WeakDom::new(InstanceBuilder::new("Folder").with_name(name));
            for middleware in MIDDLEWARES {
                let expected = if middleware.is_dir() {
                    name.to_owned()
                } else {
                    format!("{name}.{}", extension_for_middleware(middleware))
                };
                assert_eq!(
                    name_for_inst(middleware, tree.root(), None).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn existing_sources_keep_their_names_without_revalidation() {
        let new_tree = WeakDom::new(InstanceBuilder::new("Folder").with_name("../escaped"));
        for source in ["con.txt", "Old.modulescript", "Existing Directory"] {
            let old_tree = RojoTree::new(
                InstanceSnapshot::new()
                    .name("Original")
                    .class_name("Folder")
                    .metadata(
                        InstanceMetadata::new()
                            .relevant_paths(vec![Path::new("/src").join(source)]),
                    ),
            );
            for middleware in MIDDLEWARES {
                assert_eq!(
                    name_for_inst(middleware, new_tree.root(), Some(old_tree.root())).unwrap(),
                    source
                );
            }
        }
    }
}
