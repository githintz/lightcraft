//! The behaviour every [`Provider`] must have, as one check each adapter's tests run against a
//! fresh, empty account (an in-memory fake, a temporary folder, a local test server, or, by hand,
//! a live test account).

use crate::{CloudError, ContentId, ObjectName, Provider, fetch_blob, store_blob};

/// Runs every check on `p`, which must start empty. Returns the first broken expectation.
/// Leaves the account empty again when it passes.
pub fn check_provider(p: &mut dyn Provider) -> Result<(), String> {
    let label = p_label(p);
    let fail = |what: &str, got: &dyn std::fmt::Debug| Err(format!("{label}: {what} (got {got:?})"));
    let listed = p.list().map_err(|e| format!("list on an empty account: {e}"))?;
    if !listed.is_empty() {
        return fail("the account must start empty", &listed.len());
    }

    // round trip of a content-addressed blob, checked against its id
    let data: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let (id, remote) = store_blob(p, &data).map_err(|e| format!("put: {e}"))?;
    let back = fetch_blob(p, &remote, &id, data.len() as u64).map_err(|e| format!("get: {e}"))?;
    if back != data {
        return fail("get returned other bytes", &back.len());
    }

    // storing the same bytes again replaces the object, it doesn't add one
    store_blob(p, &data).map_err(|e| format!("put again: {e}"))?;
    let listed = p.list().map_err(|e| format!("list: {e}"))?;
    let blob_name = ObjectName::blob(&id);
    match listed.as_slice() {
        [one] if one.name == blob_name && one.size == data.len() as u64 && one.remote == remote => {}
        other => return fail("list must hold exactly the one blob with its size and ref", &other),
    }

    // an empty object and a non-blob name
    let manifest = ObjectName::parse("manifest/empty").map_err(|e| e.to_string())?;
    let empty_ref = p.put(&manifest, &[]).map_err(|e| format!("put empty: {e}"))?;
    match p.get(&empty_ref, 0) {
        Ok(v) if v.is_empty() => {}
        other => return fail("an empty object reads back empty", &other),
    }

    // the size limit is enforced
    match p.get(&remote, data.len() as u64 - 1) {
        Err(CloudError::TooLarge { .. }) => {}
        other => return fail("get over max_len must be TooLarge", &other.map(|v| v.len())),
    }

    // delete, then the object is gone; deleting again is fine
    for r in [&remote, &empty_ref] {
        p.delete(r).map_err(|e| format!("delete: {e}"))?;
        p.delete(r).map_err(|e| format!("delete of a deleted object: {e}"))?;
        match p.get(r, u64::MAX) {
            Err(CloudError::NotFound) => {}
            other => return fail("get after delete must be NotFound", &other.map(|v| v.len())),
        }
    }
    let listed = p.list().map_err(|e| format!("list after delete: {e}"))?;
    if !listed.is_empty() {
        return fail("the account must be empty after deleting everything", &listed);
    }

    // a different id than the content is reported as damage
    let (_, other_ref) = store_blob(p, b"other").map_err(|e| format!("put: {e}"))?;
    match fetch_blob(p, &other_ref, &ContentId::of(b"expected"), 100) {
        Err(CloudError::Corrupt(_)) => {}
        other => return fail("content that doesn't match its id must be Corrupt", &other),
    }
    p.delete(&other_ref).map_err(|e| format!("delete: {e}"))?;
    Ok(())
}

fn p_label(p: &dyn Provider) -> String {
    let info = p.info();
    format!("{} ({})", info.label, info.provider)
}
