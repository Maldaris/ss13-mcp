//! Object tree walker — collect all types that have visual representations.

use dreammaker::objtree::{ObjectTree, TypeRef};
use tracing::debug;

/// Type path prefixes we care about extracting
const EXTRACTABLE_PREFIXES: &[&str] = &[
    "/obj/",
    "/turf/",
    "/mob/",
];

/// Type path prefixes to skip (abstract bases, effects we don't need)
const SKIP_PREFIXES: &[&str] = &[
    "/obj/effect/abstract",
    "/obj/effect/landmark",
    "/obj/effect/spawner",
    "/obj/effect/mapping_helpers",
    "/obj/docking_port",
    "/mob/dead",
    "/mob/camera",
];

/// Collect all types that should have sprites extracted.
/// Optionally filter to only types matching a given prefix.
pub fn collect_extractable_types<'a>(
    objtree: &'a ObjectTree,
    filter: Option<&str>,
) -> Vec<TypeRef<'a>> {
    let mut types = Vec::new();

    walk_type(objtree.root(), &mut types, filter);

    debug!("Collected {} extractable types", types.len());
    types
}

fn walk_type<'a>(
    type_ref: TypeRef<'a>,
    out: &mut Vec<TypeRef<'a>>,
    filter: Option<&str>,
) {
    let path = &type_ref.get().path;

    // Check if this type matches our extraction criteria
    if !path.is_empty() {
        let dominated_by_extractable = EXTRACTABLE_PREFIXES.iter().any(|p| path.starts_with(p));
        let should_skip = SKIP_PREFIXES.iter().any(|p| path.starts_with(p));

        if dominated_by_extractable && !should_skip {
            if let Some(filter_prefix) = filter {
                if path.starts_with(filter_prefix) {
                    out.push(type_ref);
                }
            } else {
                out.push(type_ref);
            }
        }
    }

    // Recurse into children
    for child in type_ref.children() {
        walk_type(child, out, filter);
    }
}
