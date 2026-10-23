//! Implements iterating through an entire WeakDom and linking all Ref
//! properties using attributes.

use std::collections::{HashMap, HashSet, VecDeque};

use rbx_dom_weak::{
    types::{Attributes, Ref, UniqueId, Variant},
    ustr, Instance, Ustr, WeakDom,
};

use crate::syncback::snapshot::inst_path;
use crate::{
    multimap::MultiMap, snapshot::RojoTree, REF_ID_ATTRIBUTE_NAME, REF_POINTER_ATTRIBUTE_PREFIX,
};

pub struct RefLinks {
    /// A map of referents to each of their Ref properties.
    prop_links: MultiMap<Ref, RefLink>,
    /// A set of referents that need their ID rewritten. This includes
    /// Instances that have no existing ID.
    need_rewrite: HashSet<Ref>,
    specified_ids: HashMap<Ref, String>,
}

#[derive(PartialEq, Eq)]
struct RefLink {
    /// The name of a property
    name: Ustr,
    /// The value of the property.
    value: Ref,
}

/// Iterates through a WeakDom and collects referent properties.
///
/// They can be linked to a dom later using `link_referents`.
pub fn collect_referents(dom: &WeakDom, old_tree: &RojoTree) -> RefLinks {
    let specified_ids = collect_specified_ids(dom, old_tree);
    let mut ids: HashMap<_, _> = specified_ids
        .iter()
        .map(|(referent, id)| (id.as_str(), *referent))
        .collect();
    let mut need_rewrite = specified_ids
        .iter()
        .filter_map(|(referent, id)| {
            let existing = get_existing_id(dom.get_by_ref(*referent).unwrap())?;
            (existing != id).then_some(*referent)
        })
        .collect::<HashSet<_>>();
    let mut links = MultiMap::new();

    // Note that this is back-in, front-out. This is important because
    // VecDeque::extend is the equivalent to using push_back.
    let mut queue = VecDeque::new();
    queue.push_back(dom.root_ref());
    while let Some(inst_ref) = queue.pop_front() {
        let pointer = dom.get_by_ref(inst_ref).unwrap();
        queue.extend(pointer.children().iter().copied());

        for (prop_name, prop_value) in &pointer.properties {
            let Variant::Ref(prop_value) = prop_value else {
                continue;
            };
            if prop_value.is_none() {
                continue;
            }

            let target = match dom.get_by_ref(*prop_value) {
                Some(inst) => inst,
                None => {
                    // Properties that are Some but point to nothing may as
                    // well be `nil`. Roblox and us never produce these values
                    // but syncback prunes trees without adjusting ref
                    // properties for performance reasons.
                    log::warn!(
                        "Property {}.{} will be `nil` on the disk because the actual value is not being included in syncback",
                        inst_path(dom, inst_ref),
                        prop_name
                    );
                    continue;
                }
            };

            links.insert(
                inst_ref,
                RefLink {
                    name: *prop_name,
                    value: *prop_value,
                },
            );

            // 1. Check if target has an ID
            let specified_id = specified_ids.get(prop_value).map(String::as_str);
            if let Some(id) = specified_id.or_else(|| get_existing_id(target)) {
                if specified_id.is_some() && get_existing_id(target) != Some(id) {
                    need_rewrite.insert(*prop_value);
                }
                // If it does, we need to check whether that ID is a duplicate
                if let Some(id_ref) = ids.get(id) {
                    // If the same ID points to a new Instance, rewrite it.
                    if id_ref != prop_value {
                        if log::log_enabled!(log::Level::Trace) {
                            log::trace!(
                                "{} needs an id rewritten because it has the same id as {}",
                                target.name,
                                dom.get_by_ref(*id_ref).unwrap().name
                            );
                        }
                        need_rewrite.insert(*prop_value);
                        continue;
                    }
                }
                ids.insert(id, *prop_value);
            } else {
                log::trace!("{} needs an id rewritten because it has no id but is referred to by {}.{prop_name}", target.name, pointer.name);
                // If it does not, it needs one.
                need_rewrite.insert(*prop_value);
            }
        }
    }

    RefLinks {
        need_rewrite,
        prop_links: links,
        specified_ids,
    }
}

fn collect_specified_ids(dom: &WeakDom, old_tree: &RojoTree) -> HashMap<Ref, String> {
    let mut ids = HashMap::new();
    let mut queue = VecDeque::from([(dom.root_ref(), old_tree.get_root_id())]);

    // Match by sibling name, as directory syncback does. Paths rendered as
    // strings are ambiguous when an instance name contains a slash.
    while let Some((new_ref, old_ref)) = queue.pop_front() {
        let new_inst = dom.get_by_ref(new_ref).unwrap();
        let old_inst = old_tree.get_instance(old_ref).unwrap();
        if let Some(id) = &old_inst.metadata().specified_id {
            if old_tree.get_specified_id(id) == Some(old_ref) {
                ids.insert(new_ref, id.to_string());
            }
        }

        let old_children = unique_children(old_tree.inner(), old_inst.children());
        for (name, new_child) in unique_children(dom, new_inst.children()) {
            if let (Some(new_child), Some(Some(old_child))) = (new_child, old_children.get(name)) {
                queue.push_back((new_child, *old_child));
            }
        }
    }

    ids
}

fn unique_children<'a>(dom: &'a WeakDom, children: &[Ref]) -> HashMap<&'a str, Option<Ref>> {
    let mut names = HashMap::new();
    for child in children {
        let inst = dom.get_by_ref(*child).unwrap();
        names
            .entry(inst.name.as_str())
            .and_modify(|referent| *referent = None)
            .or_insert(Some(*child));
    }

    names
}

pub fn link_referents(links: RefLinks, dom: &mut WeakDom) -> anyhow::Result<()> {
    write_id_attributes(&links, dom)?;

    let mut prop_list = Vec::new();

    for (inst_id, properties) in links.prop_links {
        for ref_link in properties {
            let prop_inst = match dom.get_by_ref(ref_link.value) {
                Some(inst) => inst,
                None => continue,
            };
            let id = get_existing_id(prop_inst)
                .expect("all Instances that are pointed to should have an ID");
            prop_list.push((ref_link.name, Variant::String(id.to_owned())));
        }
        let inst = match dom.get_by_ref_mut(inst_id) {
            Some(inst) => inst,
            None => continue,
        };

        let mut attributes: Attributes = match inst.properties.remove(&ustr("Attributes")) {
            Some(Variant::Attributes(attrs)) => attrs,
            None => Attributes::new(),
            Some(value) => {
                anyhow::bail!(
                    "expected Attributes to be of type 'Attributes' but it was of type '{:?}'",
                    value.ty()
                );
            }
        }
        .into_iter()
        .filter(|(name, _)| !name.starts_with(REF_POINTER_ATTRIBUTE_PREFIX))
        .collect();

        for (prop_name, prop_value) in prop_list.drain(..) {
            attributes.insert(
                format!("{REF_POINTER_ATTRIBUTE_PREFIX}{prop_name}"),
                prop_value,
            );
        }

        inst.properties
            .insert("Attributes".into(), attributes.into());
    }

    Ok(())
}

fn write_id_attributes(links: &RefLinks, dom: &mut WeakDom) -> anyhow::Result<()> {
    let mut used_ids: HashSet<_> = links.specified_ids.values().cloned().collect();
    used_ids.extend(
        dom.descendants()
            .filter_map(|inst| get_existing_id(inst).map(str::to_owned)),
    );

    for referent in &links.need_rewrite {
        let inst = match dom.get_by_ref_mut(*referent) {
            Some(inst) => inst,
            None => continue,
        };
        let id = if let Some(id) = links.specified_ids.get(referent) {
            id.clone()
        } else {
            let mut id = match inst.properties.get(&ustr("UniqueId")) {
                Some(Variant::UniqueId(id)) => id.to_string(),
                _ => UniqueId::now().unwrap().to_string(),
            };
            while !used_ids.insert(id.clone()) {
                id = UniqueId::now().unwrap().to_string();
            }

            id
        };

        let attributes = match inst.properties.get_mut(&ustr("Attributes")) {
            Some(Variant::Attributes(attrs)) => attrs,
            None => {
                inst.properties
                    .insert("Attributes".into(), Attributes::new().into());
                match inst.properties.get_mut(&ustr("Attributes")) {
                    Some(Variant::Attributes(attrs)) => attrs,
                    _ => unreachable!(),
                }
            }
            Some(value) => {
                anyhow::bail!(
                    "expected Attributes to be of type 'Attributes' but it was of type '{:?}'",
                    value.ty()
                );
            }
        };
        attributes.insert(REF_ID_ATTRIBUTE_NAME.into(), Variant::String(id));
    }
    Ok(())
}

fn get_existing_id(inst: &Instance) -> Option<&str> {
    if let Variant::Attributes(attrs) = inst.properties.get(&ustr("Attributes"))? {
        let id = attrs.get(REF_ID_ATTRIBUTE_NAME)?;
        match id {
            Variant::String(str) => Some(str),
            Variant::BinaryString(bstr) => std::str::from_utf8(bstr.as_ref()).ok(),
            _ => None,
        }
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    use crate::{
        snapshot::{InstanceMetadata, InstanceSnapshot},
        RojoRef,
    };

    fn old_tree_with_worker(id: &str) -> RojoTree {
        RojoTree::new(InstanceSnapshot::new().class_name("Folder").children(vec![
            InstanceSnapshot::new().name("Worker").class_name("Folder")
                .metadata(InstanceMetadata::new().specified_id(Some(RojoRef::new(id.to_owned())))),
        ]))
    }

    #[test]
    fn duplicate_new_names_do_not_inherit_specified_ids() {
        let old_tree = old_tree_with_worker("worker");
        let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
        let first = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Folder").with_name("Worker"),
        );
        let second = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Folder").with_name("Worker"),
        );
        for target in [first, second] {
            dom.insert(
                dom.root_ref(),
                InstanceBuilder::new("ObjectValue").with_property("Value", Variant::Ref(target)),
            );
        }

        let links = collect_referents(&dom, &old_tree);
        link_referents(links, &mut dom).unwrap();

        let first_id = get_existing_id(dom.get_by_ref(first).unwrap()).unwrap();
        let second_id = get_existing_id(dom.get_by_ref(second).unwrap()).unwrap();
        assert_ne!(first_id, "worker");
        assert_ne!(second_id, "worker");
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn generated_ids_do_not_reuse_reserved_specified_ids() {
        let unique_id = UniqueId::new(123, 456, 789);
        let reserved = unique_id.to_string();
        let old_tree = old_tree_with_worker(&reserved);
        let mut dom = WeakDom::new(InstanceBuilder::new("Folder"));
        let worker = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Folder").with_name("Worker"),
        );
        let mut attributes = Attributes::new();
        attributes.insert(REF_ID_ATTRIBUTE_NAME.into(), reserved.clone().into());
        let other = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Folder")
                .with_name("Other")
                .with_property("Attributes", attributes)
                .with_property("UniqueId", unique_id),
        );
        let mut pointers = Vec::new();
        for target in [worker, other] {
            pointers.push(dom.insert(
                dom.root_ref(),
                InstanceBuilder::new("ObjectValue").with_property("Value", Variant::Ref(target)),
            ));
        }

        let links = collect_referents(&dom, &old_tree);
        link_referents(links, &mut dom).unwrap();

        let worker_id = get_existing_id(dom.get_by_ref(worker).unwrap()).unwrap();
        let other_id = get_existing_id(dom.get_by_ref(other).unwrap()).unwrap();
        assert_eq!(worker_id, reserved);
        assert_ne!(other_id, reserved);
        for (pointer, expected) in pointers.into_iter().zip([worker_id, other_id]) {
            let Variant::Attributes(attributes) =
                &dom.get_by_ref(pointer).unwrap().properties[&ustr("Attributes")]
            else {
                panic!("reference linking should add attributes");
            };
            assert_eq!(
                attributes.get("Rojo_Target_Value"),
                Some(&Variant::String(expected.to_owned()))
            );
        }
    }
}
