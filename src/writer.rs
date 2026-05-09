use std::io::Write;
use std::path::Path;
use crate::entity::{Entity, Reference};
use crate::rank::RankManifest;

pub fn write_entities_jsonl(entities: &[Entity], output: &mut dyn Write, pretty: bool) -> std::io::Result<()> {
    for entity in entities {
        if pretty {
            serde_json::to_writer_pretty(&mut *output, entity)
        } else {
            serde_json::to_writer(&mut *output, entity)
        }.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        writeln!(output)?;
    }
    Ok(())
}

pub fn write_refs_jsonl(refs: &[Reference], output: &mut dyn Write, pretty: bool) -> std::io::Result<()> {
    for reference in refs {
        if pretty {
            serde_json::to_writer_pretty(&mut *output, reference)
        } else {
            serde_json::to_writer(&mut *output, reference)
        }.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        writeln!(output)?;
    }
    Ok(())
}

pub fn write_to_files(
    entities: &[Entity],
    refs: &[Reference],
    root: &std::path::Path,
    pretty: bool,
) -> std::io::Result<()> {
    let dir = root.join(".sigil");
    std::fs::create_dir_all(&dir)?;

    let file = std::fs::File::create(dir.join("entities.jsonl"))?;
    let mut w = std::io::BufWriter::new(file);
    write_entities_jsonl(entities, &mut w, pretty)?;
    w.flush()?;

    if !refs.is_empty() {
        let file = std::fs::File::create(dir.join("refs.jsonl"))?;
        let mut w = std::io::BufWriter::new(file);
        write_refs_jsonl(refs, &mut w, pretty)?;
        w.flush()?;
    }

    Ok(())
}

/// Write the file-level PageRank manifest to `.sigil/rank.json`.
/// Called by `sigil index` when rank is enabled (the default).
pub fn write_rank_json(manifest: &RankManifest, root: &Path, pretty: bool) -> std::io::Result<()> {
    let dir = root.join(".sigil");
    std::fs::create_dir_all(&dir)?;
    let content = if pretty {
        serde_json::to_string_pretty(manifest)
    } else {
        serde_json::to_string(manifest)
    }
    .map_err(std::io::Error::other)?;
    std::fs::write(dir.join("rank.json"), content)
}

/// Remove a stale `.sigil/rank.json` when the user runs `sigil index --no-rank`.
/// Missing file is not an error.
pub fn remove_rank_json(root: &Path) -> std::io::Result<()> {
    let path = root.join(".sigil").join("rank.json");
    if !path.exists() {
        return Ok(());
    }
    std::fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{Entity, Reference};

    fn sample_entity() -> Entity {
        Entity {
            file: "src/main.py".to_string(),
            name: "foo".to_string(),
            kind: "function".to_string(),
            line_start: 1,
            line_end: 3,
            parent: None,
            qualified_name: None,
            sig: Some("def foo(x: int):".to_string()),
            meta: None,
            body_hash: Some("abcdef1234567890".to_string()),
            sig_hash: Some("1234567890abcdef".to_string()),
            struct_hash: "fedcba0987654321".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
        }
    }

    #[test]
    fn writes_one_entity_per_line() {
        let entities = vec![sample_entity()];
        let mut buf = Vec::new();
        write_entities_jsonl(&entities, &mut buf, false).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(output.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["name"], "foo");
    }

    #[test]
    fn writes_refs_per_line() {
        let refs = vec![Reference {
            file: "src/main.py".to_string(),
            caller: Some("main".to_string()),
            name: "Config".to_string(),
            ref_kind: "instantiation".to_string(),
            line: 50,
        }];
        let mut buf = Vec::new();
        write_refs_jsonl(&refs, &mut buf, false).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(output.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["kind"], "instantiation");
        assert!(parsed.get("ref_kind").is_none(), "0.4.0 serializes as `kind`");
    }

    #[test]
    fn null_fields_serialized() {
        let mut e = sample_entity();
        e.parent = None;
        e.meta = None;
        let mut buf = Vec::new();
        write_entities_jsonl(&[e], &mut buf, false).unwrap();
        let output = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed["parent"].is_null());
        assert!(parsed["meta"].is_null());
    }
}
