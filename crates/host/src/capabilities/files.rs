//! Built-in filesystem capabilities:
//! `files/read`, `files/read-many`, `files/write`, `files/edit`, `files/replace`,
//! `files/list`, `files/grep`, `files/find`, `files/find-many`.

use crate::capability::{
    ArgumentMode, CallContext, Capability, CapabilityCatalog, CapabilityError, CapabilityMeta,
};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

const MAX_WHOLE_READ_BYTES: u64 = 256 * 1024;
const MAX_CHUNK_READ_BYTES: usize = 128 * 1024;
const DEFAULT_CHUNK_LINES: usize = 400;
const MAX_CHUNK_LINES: usize = 2_000;
const MAX_BATCH_ITEMS: usize = 16;
const MAX_BATCH_RESULT_BYTES: usize = 768 * 1024;
const MAX_SEARCH_RESULTS: usize = 10_000;
const DEFAULT_FIND_LIMIT: usize = 1_000;
const DEFAULT_BATCH_FIND_LIMIT: usize = 100;
const MAX_BATCH_FIND_LIMIT: usize = 1_000;
const DEFAULT_LIST_LIMIT: usize = 500;
const MAX_LIST_LIMIT: usize = 5_000;
const MAX_GREP_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn resolve_path(cwd: &Path, supplied: &str) -> Result<PathBuf, CapabilityError> {
    if supplied.trim().is_empty() || supplied.contains('\0') {
        return Err(CapabilityError::invalid_args(
            "files",
            "Path must be a non-empty string without NUL bytes.",
        ));
    }
    if supplied.starts_with('@') {
        return Err(CapabilityError::invalid_args(
            "files",
            "Leading '@' path markers are unavailable.",
        ));
    }
    let p = Path::new(supplied);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(cwd.join(p))
    }
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn to_posix(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn is_hidden_rel(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(s) => s.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, CapabilityError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let g = Glob::new(pat).map_err(|e| {
            CapabilityError::invalid_args("files", format!("Invalid glob '{pat}': {e}"))
        })?;
        builder.add(g);
        if !pat.starts_with("**/") {
            if let Ok(g2) = Glob::new(&format!("**/{pat}")) {
                builder.add(g2);
            }
        }
    }
    builder.build().map_err(|e| {
        CapabilityError::invalid_args("files", format!("Failed building globset: {e}"))
    })
}

fn read_single_file(
    ctx: &CallContext,
    supplied_path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<Value, CapabilityError> {
    let target = resolve_path(&ctx.cwd, supplied_path)?;
    let meta = fs::metadata(&target)
        .map_err(|e| CapabilityError::exec("FileReadError", format!("{supplied_path}: {e}")))?;
    if !meta.is_file() {
        return Err(CapabilityError::exec(
            "FileReadError",
            format!("Only regular files can be read: {supplied_path}"),
        ));
    }
    let canonical = canonical_or_lexical(&target);
    let chunked = offset.is_some() || limit.is_some();

    if !chunked {
        if meta.len() > MAX_WHOLE_READ_BYTES {
            return Err(CapabilityError::exec(
                "FileTooLargeError",
                format!(
                    "File is too large for a whole read ({} bytes, max {MAX_WHOLE_READ_BYTES}); use offset and limit.",
                    meta.len()
                ),
            ));
        }
        let raw = fs::read(&target)
            .map_err(|e| CapabilityError::exec("FileReadError", format!("{supplied_path}: {e}")))?;
        if raw.contains(&0) {
            return Err(CapabilityError::exec(
                "FileReadError",
                "File appears to be binary (contains NUL bytes); Cant file APIs handle UTF-8 text only.",
            ));
        }
        let text = String::from_utf8(raw.clone())
            .map_err(|_| CapabilityError::exec("FileReadError", "File is not valid UTF-8 text."))?;
        let digest = sha256_hex(&raw);
        let total_lines = text.split('\n').count();
        ctx.file_coverage
            .record_complete(canonical, digest.clone(), total_lines);
        return Ok(json!({
            "text": text,
            "complete": true,
            "coverageComplete": true,
            "startLine": 1,
            "endLine": total_lines,
            "totalLines": total_lines,
            "totalBytes": raw.len(),
            "sha256": digest,
        }));
    }

    let start_line = offset.unwrap_or(1);
    let req_limit = limit.unwrap_or(DEFAULT_CHUNK_LINES);
    if start_line < 1 {
        return Err(CapabilityError::invalid_args(
            "files.read",
            "offset must be >= 1",
        ));
    }
    if !(1..=MAX_CHUNK_LINES).contains(&req_limit) {
        return Err(CapabilityError::invalid_args(
            "files.read",
            format!("limit must be between 1 and {MAX_CHUNK_LINES}"),
        ));
    }
    let raw = fs::read(&target)
        .map_err(|e| CapabilityError::exec("FileReadError", format!("{supplied_path}: {e}")))?;
    if raw.contains(&0) {
        return Err(CapabilityError::exec(
            "FileReadError",
            "File appears to be binary (contains NUL bytes).",
        ));
    }
    let text = String::from_utf8(raw.clone())
        .map_err(|_| CapabilityError::exec("FileReadError", "File is not valid UTF-8 text."))?;
    let digest = sha256_hex(&raw);
    let lines: Vec<&str> = text.split('\n').collect();
    let total_lines = lines.len();
    if start_line > total_lines {
        return Err(CapabilityError::exec(
            "FileReadError",
            format!("Offset {start_line} is beyond end of file ({total_lines} lines total)."),
        ));
    }

    let mut collected = Vec::new();
    let mut window_bytes = 0usize;
    for line in lines.iter().skip(start_line - 1).take(req_limit) {
        let extra = line.len() + usize::from(!collected.is_empty());
        if window_bytes + extra > MAX_CHUNK_READ_BYTES {
            break;
        }
        window_bytes += extra;
        collected.push(*line);
    }
    if collected.is_empty() {
        return Err(CapabilityError::exec(
            "FileTooLargeError",
            format!("Line {start_line} exceeds the {MAX_CHUNK_READ_BYTES}-byte chunk limit."),
        ));
    }
    let end_line = start_line + collected.len() - 1;
    let complete = start_line == 1 && end_line == total_lines;
    let coverage_complete = ctx.file_coverage.record_chunk(
        canonical,
        digest.clone(),
        total_lines,
        start_line,
        end_line,
    );

    let mut obj = json!({
        "text": collected.join("\n"),
        "complete": complete,
        "coverageComplete": coverage_complete,
        "startLine": start_line,
        "endLine": end_line,
        "totalLines": total_lines,
        "totalBytes": raw.len(),
        "sha256": digest,
    });
    if end_line < total_lines {
        obj.as_object_mut()
            .unwrap()
            .insert("nextOffset".into(), json!(end_line + 1));
    }
    Ok(obj)
}

fn atomic_replace(target: &Path, content: &str, expected_sha: &str) -> Result<(), CapabilityError> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp_path = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4().simple()
    ));

    let write_res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        #[cfg(unix)]
        if let Ok(meta) = fs::metadata(target) {
            let _ = fs::set_permissions(&tmp_path, meta.permissions());
        }
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(CapabilityError::exec("FileWriteError", e.to_string()));
    }

    let current = fs::read(target).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        CapabilityError::exec("FileWriteError", e.to_string())
    })?;
    if !sha256_hex(&current).eq_ignore_ascii_case(expected_sha) {
        let _ = fs::remove_file(&tmp_path);
        return Err(CapabilityError::exec(
            "FileConflictError",
            "File changed while mutation was being prepared; read it again before retrying.",
        ));
    }
    if let Err(e) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(CapabilityError::exec("FileWriteError", e.to_string()));
    }
    Ok(())
}

// --- Capabilities ---

struct FilesRead(CapabilityMeta);
#[async_trait]
impl Capability for FilesRead {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
            CapabilityError::invalid_args("files.read", "Missing required 'path'.")
        })?;
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let ctx = ctx.clone();
        let path = path.to_string();
        tokio::task::spawn_blocking(move || read_single_file(&ctx, &path, offset, limit))
            .await
            .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

struct FilesReadMany(CapabilityMeta);
#[async_trait]
impl Capability for FilesReadMany {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let paths: Vec<String> = args
            .get("paths")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CapabilityError::invalid_args("files.readMany", "Missing 'paths' array.")
            })?
            .iter()
            .map(|v| {
                v.as_str().map(ToString::to_string).ok_or_else(|| {
                    CapabilityError::invalid_args("files.readMany", "Each path must be a string.")
                })
            })
            .collect::<Result<_, _>>()?;
        if paths.is_empty() || paths.len() > MAX_BATCH_ITEMS {
            return Err(CapabilityError::invalid_args(
                "files.readMany",
                format!("paths must contain 1..={MAX_BATCH_ITEMS} entries."),
            ));
        }
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || {
            let mut results = Vec::with_capacity(paths.len());
            for path in paths {
                let mut entry = read_single_file(&ctx, &path, None, None)?;
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("path".into(), Value::String(path));
                results.push(entry);
            }
            let out = Value::Array(results);
            let encoded_len = serde_json::to_vec(&out).map(|b| b.len()).unwrap_or(0);
            if encoded_len > MAX_BATCH_RESULT_BYTES {
                return Err(CapabilityError::exec(
                    "FileTooLargeError",
                    format!("Batch read exceeds {MAX_BATCH_RESULT_BYTES}-byte aggregate limit."),
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

struct FilesWrite(CapabilityMeta);
#[async_trait]
impl Capability for FilesWrite {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("files.write", "Missing 'path'."))?
            .to_string();
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("files.write", "Missing 'content'."))?
            .to_string();
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || {
            let target = resolve_path(&ctx.cwd, &path)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| CapabilityError::exec("FileWriteError", e.to_string()))?;
            }
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        CapabilityError::exec(
                            "FileExistsError",
                            format!("File already exists: {path}"),
                        )
                    } else {
                        CapabilityError::exec("FileWriteError", e.to_string())
                    }
                })?;
            f.write_all(content.as_bytes())
                .and_then(|_| f.sync_all())
                .map_err(|e| CapabilityError::exec("FileWriteError", e.to_string()))?;
            let digest = sha256_hex(content.as_bytes());
            ctx.file_coverage.record_complete(
                canonical_or_lexical(&target),
                digest.clone(),
                content.split('\n').count(),
            );
            Ok(json!({
                "path": path,
                "created": true,
                "bytes": content.len(),
                "sha256": digest,
            }))
        })
        .await
        .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

struct FilesReplace(CapabilityMeta);
#[async_trait]
impl Capability for FilesReplace {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("files.replace", "Missing 'path'."))?
            .to_string();
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("files.replace", "Missing 'content'."))?
            .to_string();
        let expected_sha = args
            .get("expectedSha256")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                CapabilityError::invalid_args("files.replace", "Missing 'expectedSha256'.")
            })?
            .to_string();
        if expected_sha.len() != 64 || !expected_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CapabilityError::invalid_args(
                "files.replace",
                "expectedSha256 must be a 64-character hex SHA-256 digest.",
            ));
        }
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || {
            let target = resolve_path(&ctx.cwd, &path)?;
            let canonical = canonical_or_lexical(&target);
            if !ctx.file_coverage.has_complete(&canonical, &expected_sha) {
                return Err(CapabilityError::exec(
                    "FileCoverageError",
                    "File must be read completely at its current SHA-256 before it can be replaced.",
                ));
            }
            atomic_replace(&target, &content, &expected_sha)?;
            let digest = sha256_hex(content.as_bytes());
            ctx.file_coverage
                .record_complete(canonical, digest.clone(), content.split('\n').count());
            Ok(json!({
                "path": path,
                "replaced": true,
                "bytes": content.len(),
                "sha256": digest,
            }))
        })
        .await
        .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

struct FilesEdit(CapabilityMeta);
#[async_trait]
impl Capability for FilesEdit {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("files.edit", "Missing 'path'."))?
            .to_string();
        let expected_sha = args
            .get("expectedSha256")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let edits_val = args
            .get("edits")
            .and_then(Value::as_array)
            .ok_or_else(|| CapabilityError::invalid_args("files.edit", "Missing 'edits' array."))?;
        if edits_val.is_empty() {
            return Err(CapabilityError::invalid_args(
                "files.edit",
                "edits must contain at least one replacement.",
            ));
        }
        let mut edits = Vec::with_capacity(edits_val.len());
        for (i, item) in edits_val.iter().enumerate() {
            let old_text = item
                .get("oldText")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CapabilityError::invalid_args(
                        "files.edit",
                        format!("edits[{i}].oldText must be a non-empty string."),
                    )
                })?
                .to_string();
            let new_text = item
                .get("newText")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CapabilityError::invalid_args(
                        "files.edit",
                        format!("edits[{i}].newText must be a string."),
                    )
                })?
                .to_string();
            edits.push((i, old_text, new_text));
        }

        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || {
            let target = resolve_path(&ctx.cwd, &path)?;
            let before = fs::read(&target)
                .map_err(|e| CapabilityError::exec("FileReadError", format!("{path}: {e}")))?;
            let before_sha = sha256_hex(&before);
            if let Some(ref exp) = expected_sha {
                if !before_sha.eq_ignore_ascii_case(exp) {
                    return Err(CapabilityError::exec(
                        "FileConflictError",
                        "File changed since expectedSha256; read it again before editing.",
                    ));
                }
            }
            let text = String::from_utf8(before)
                .map_err(|_| CapabilityError::exec("FileReadError", "File is not valid UTF-8."))?;

            let mut spans = Vec::with_capacity(edits.len());
            for (i, old_text, new_text) in edits {
                let start = text.find(&old_text).ok_or_else(|| {
                    CapabilityError::exec(
                        "FileEditError",
                        format!("edits[{i}].oldText was not found exactly in {path}"),
                    )
                })?;
                if text[start + 1..].contains(&old_text) {
                    return Err(CapabilityError::exec(
                        "FileEditError",
                        format!("edits[{i}].oldText is not unique in {path}"),
                    ));
                }
                spans.push((i, start, start + old_text.len(), new_text));
            }
            spans.sort_by_key(|s| s.1);
            for w in spans.windows(2) {
                if w[1].1 < w[0].2 {
                    return Err(CapabilityError::exec(
                        "FileEditError",
                        format!(
                            "edits[{}].oldText overlaps another exact replacement",
                            w[1].0
                        ),
                    ));
                }
            }

            let replacements = spans.len();
            let mut content = text;
            for (_, start, end, new_text) in spans.into_iter().rev() {
                content.replace_range(start..end, &new_text);
            }
            let digest = sha256_hex(content.as_bytes());
            let canonical = canonical_or_lexical(&target);
            let was_complete = ctx.file_coverage.has_complete(&canonical, &before_sha);
            if digest != before_sha {
                atomic_replace(&target, &content, &before_sha)?;
            }
            if was_complete {
                ctx.file_coverage.record_complete(
                    canonical,
                    digest.clone(),
                    content.split('\n').count(),
                );
            }
            Ok(json!({
                "path": path,
                "edited": true,
                "replacements": replacements,
                "bytes": content.len(),
                "sha256": digest,
            }))
        })
        .await
        .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

struct FilesList(CapabilityMeta);
#[async_trait]
impl Capability for FilesList {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let rel_path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_string();
        let depth = args
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 10) as usize;
        let kind = args
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("all")
            .to_string();
        let include_hidden = args
            .get("includeHidden")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let full_path = args.get("pathFormat").and_then(Value::as_str) == Some("full");
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_LIST_LIMIT as u64)
            .clamp(1, MAX_LIST_LIMIT as u64) as usize;
        let respect_gitignore = args
            .get("respectGitignore")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let excludes: Vec<String> = args
            .get("exclude")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || {
            let root = resolve_path(&ctx.cwd, &rel_path)?;
            let exclude_set = build_globset(&excludes)?;
            let mut builder = WalkBuilder::new(&root);
            builder
                .max_depth(Some(depth))
                .hidden(!include_hidden)
                .git_ignore(respect_gitignore)
                .git_global(respect_gitignore)
                .git_exclude(respect_gitignore)
                .sort_by_file_path(|a, b| a.cmp(b));

            let mut entries = Vec::new();
            let mut limit_reached = false;
            for result in builder.build() {
                let Ok(entry) = result else { continue };
                if entry.depth() == 0 {
                    continue;
                }
                let abs = entry.path();
                let rel_to_root = abs.strip_prefix(&root).unwrap_or(abs);
                if rel_to_root
                    .components()
                    .any(|c| matches!(c, Component::Normal(s) if s == ".git"))
                {
                    continue;
                }
                if !include_hidden && is_hidden_rel(rel_to_root) {
                    continue;
                }
                if exclude_set.is_match(rel_to_root) {
                    continue;
                }
                let ft = entry.file_type();
                let entry_type = match ft {
                    Some(t) if t.is_dir() => "directory",
                    Some(t) if t.is_file() => "file",
                    Some(t) if t.is_symlink() => "symlink",
                    _ => "other",
                };
                if kind == "files" && entry_type != "file" {
                    continue;
                }
                if kind == "directories" && entry_type != "directory" {
                    continue;
                }
                if entries.len() >= limit {
                    limit_reached = true;
                    break;
                }
                let size = if entry_type == "file" {
                    entry.metadata().ok().map(|m| m.len())
                } else {
                    None
                };
                let display_path = if full_path {
                    to_posix(abs)
                } else {
                    to_posix(abs.strip_prefix(&ctx.cwd).unwrap_or(abs))
                };
                let name = abs
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                entries.push(json!({
                    "path": display_path,
                    "name": name,
                    "type": entry_type,
                    "size": size,
                }));
            }
            Ok(json!({ "entries": entries, "limitReached": limit_reached }))
        })
        .await
        .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

struct FindOptions<'a> {
    limit: usize,
    max_depth: Option<usize>,
    excludes: &'a [String],
    include_hidden: bool,
    full_path: bool,
}

fn find_paths_sync(
    cwd: &Path,
    pattern: &str,
    rel_dir: &str,
    FindOptions {
        limit,
        max_depth,
        excludes,
        include_hidden,
        full_path,
    }: FindOptions<'_>,
) -> Result<(Vec<String>, bool), CapabilityError> {
    let root = resolve_path(cwd, rel_dir)?;
    let matcher = build_globset(&[pattern.to_string()])?;
    let exclude_set = build_globset(excludes)?;
    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(!include_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .sort_by_file_path(|a, b| a.cmp(b));
    if let Some(d) = max_depth {
        builder.max_depth(Some(d));
    }
    let mut paths = Vec::new();
    let mut limit_reached = false;
    for res in builder.build() {
        let Ok(entry) = res else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let abs = entry.path();
        let rel = abs.strip_prefix(&root).unwrap_or(abs);
        if rel
            .components()
            .any(|c| matches!(c, Component::Normal(s) if s == ".git"))
        {
            continue;
        }
        if !include_hidden && is_hidden_rel(rel) {
            continue;
        }
        if exclude_set.is_match(rel) {
            continue;
        }
        let rel_posix = to_posix(rel);
        if !matcher.is_match(&rel_posix) {
            continue;
        }
        if paths.len() >= limit {
            limit_reached = true;
            break;
        }
        paths.push(if full_path { to_posix(abs) } else { rel_posix });
    }
    Ok((paths, limit_reached))
}

struct FilesFind(CapabilityMeta);
#[async_trait]
impl Capability for FilesFind {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("files.find", "Missing 'pattern'."))?
            .to_string();
        let rel_dir = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_string();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_FIND_LIMIT as u64)
            .clamp(1, MAX_SEARCH_RESULTS as u64) as usize;
        let max_depth = args
            .get("maxDepth")
            .and_then(Value::as_u64)
            .map(|d| d as usize);
        let include_hidden = args
            .get("includeHidden")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let full_path = args.get("pathFormat").and_then(Value::as_str) == Some("full");
        let excludes: Vec<String> = args
            .get("exclude")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let cwd = ctx.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let (paths, limit_reached) = find_paths_sync(
                &cwd,
                &pattern,
                &rel_dir,
                FindOptions {
                    limit,
                    max_depth,
                    excludes: &excludes,
                    include_hidden,
                    full_path,
                },
            )?;
            Ok(json!({ "paths": paths, "limitReached": limit_reached }))
        })
        .await
        .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

struct FilesFindMany(CapabilityMeta);
#[async_trait]
impl Capability for FilesFindMany {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let patterns: Vec<String> = args
            .get("patterns")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CapabilityError::invalid_args("files.findMany", "Missing 'patterns' array.")
            })?
            .iter()
            .map(|v| {
                v.as_str().map(ToString::to_string).ok_or_else(|| {
                    CapabilityError::invalid_args(
                        "files.findMany",
                        "Each pattern must be a string.",
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        if patterns.is_empty() || patterns.len() > MAX_BATCH_ITEMS {
            return Err(CapabilityError::invalid_args(
                "files.findMany",
                format!("patterns must contain 1..={MAX_BATCH_ITEMS} entries."),
            ));
        }
        let rel_dir = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_string();
        let limit_per = args
            .get("limitPerPattern")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_BATCH_FIND_LIMIT as u64)
            .clamp(1, MAX_BATCH_FIND_LIMIT as u64) as usize;
        let max_depth = args
            .get("maxDepth")
            .and_then(Value::as_u64)
            .map(|d| d as usize);
        let include_hidden = args
            .get("includeHidden")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let full_path = args.get("pathFormat").and_then(Value::as_str) == Some("full");
        let excludes: Vec<String> = args
            .get("exclude")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let cwd = ctx.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let mut groups = Vec::with_capacity(patterns.len());
            for pattern in patterns {
                let (paths, limit_reached) = find_paths_sync(
                    &cwd,
                    &pattern,
                    &rel_dir,
                    FindOptions { limit: limit_per, max_depth, excludes: &excludes, include_hidden, full_path },
                )?;
                if limit_reached {
                    return Err(CapabilityError::exec(
                        "FindLimitError",
                        format!("Find pattern '{pattern}' exceeded its {limit_per}-path limit; refine pattern or path."),
                    ));
                }
                groups.push(json!({ "pattern": pattern, "paths": paths }));
            }
            let out = Value::Array(groups);
            if serde_json::to_vec(&out).map(|b| b.len()).unwrap_or(0) > MAX_BATCH_RESULT_BYTES {
                return Err(CapabilityError::exec(
                    "FindLimitError",
                    format!("Batch find exceeds {MAX_BATCH_RESULT_BYTES}-byte aggregate limit."),
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

struct FilesGrep(CapabilityMeta);
#[async_trait]
impl Capability for FilesGrep {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }
    async fn call(&self, ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("files.grep", "Missing 'pattern'."))?
            .to_string();
        let rel_path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_string();
        let glob = args
            .get("glob")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let ignore_case = args
            .get("ignoreCase")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let literal = args
            .get("literal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(200)
            .clamp(1, MAX_SEARCH_RESULTS as u64) as usize;

        let regex_pat = if literal {
            regex::escape(&pattern)
        } else {
            pattern
        };
        let re = RegexBuilder::new(&regex_pat)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| {
                CapabilityError::invalid_args("files.grep", format!("Invalid regex: {e}"))
            })?;

        let cwd = ctx.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let root = resolve_path(&cwd, &rel_path)?;
            let glob_matcher = match glob {
                Some(ref g) => Some(build_globset(std::slice::from_ref(g))?),
                None => None,
            };

            let mut matches = Vec::new();
            let mut limit_reached = false;

            let mut search_file = |file_path: &Path, display_path: String| -> bool {
                let Ok(meta) = fs::metadata(file_path) else {
                    return false;
                };
                if !meta.is_file() || meta.len() > MAX_GREP_FILE_BYTES {
                    return false;
                }
                let Ok(bytes) = fs::read(file_path) else {
                    return false;
                };
                if bytes.contains(&0) {
                    return false;
                }
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    return false;
                };
                for (idx, line) in text.split('\n').enumerate() {
                    if let Some(m) = re.find(line) {
                        if matches.len() >= limit {
                            return true;
                        }
                        let col = line[..m.start()].chars().count() + 1;
                        let truncated_line = if line.len() > 2000 {
                            let mut end = 2000;
                            while end > 0 && !line.is_char_boundary(end) {
                                end -= 1;
                            }
                            &line[..end]
                        } else {
                            line
                        };
                        matches.push(json!({
                            "path": display_path,
                            "line": idx + 1,
                            "column": col,
                            "text": truncated_line,
                        }));
                    }
                }
                false
            };

            if root.is_file() {
                let disp = to_posix(root.strip_prefix(&cwd).unwrap_or(&root));
                limit_reached = search_file(&root, disp);
            } else {
                let mut builder = WalkBuilder::new(&root);
                builder
                    .hidden(false)
                    .git_ignore(true)
                    .git_global(true)
                    .git_exclude(true)
                    .sort_by_file_path(|a, b| a.cmp(b));
                for res in builder.build() {
                    let Ok(entry) = res else { continue };
                    if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                        continue;
                    }
                    let abs = entry.path();
                    let rel_root = abs.strip_prefix(&root).unwrap_or(abs);
                    if rel_root
                        .components()
                        .any(|c| matches!(c, Component::Normal(s) if s == ".git"))
                    {
                        continue;
                    }
                    let rel_posix = to_posix(rel_root);
                    if let Some(ref gm) = glob_matcher {
                        if !gm.is_match(&rel_posix) {
                            continue;
                        }
                    }
                    let disp = to_posix(abs.strip_prefix(&cwd).unwrap_or(abs));
                    if search_file(abs, disp) {
                        limit_reached = true;
                        break;
                    }
                }
            }

            Ok(json!({ "matches": matches, "limitReached": limit_reached }))
        })
        .await
        .map_err(|e| CapabilityError::exec("TaskError", e.to_string()))?
    }
}

fn steel_pos(fields: &[&str]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("steel".into(), json!({ "positional": fields }));
    m
}

pub fn register(catalog: &CapabilityCatalog) {
    catalog.register(Arc::new(FilesRead(CapabilityMeta {
        tool_name: "read".into(),
        api_path: vec!["files".into(), "read".into()],
        description: "Read a UTF-8 text file as structured exact content with sha256. Without offset/limit the whole file must fit 256 KiB; offset or limit requests a bounded line chunk.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer", "minimum": 1 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 2000 }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["path"]),
    })));

    catalog.register(Arc::new(FilesReadMany(CapabilityMeta {
        tool_name: "__cant_files_read_many".into(),
        api_path: vec!["files".into(), "readMany".into()],
        description: "Read up to 16 complete UTF-8 text files in caller-provided order (256 KiB per file, 768 KiB aggregate).".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["paths"],
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 16 }
            }
        }),
        result_schema: json!({ "type": "array" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["paths"]),
    })));

    catalog.register(Arc::new(FilesGrep(CapabilityMeta {
        tool_name: "grep".into(),
        api_path: vec!["files".into(), "grep".into()],
        description: "Search file contents for a regex or literal pattern, respecting .gitignore. Returns structured matches {path, line, column, text}.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["pattern"],
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "glob": { "type": "string" },
                "ignoreCase": { "type": "boolean" },
                "literal": { "type": "boolean" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 10000 }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["pattern"]),
    })));

    catalog.register(Arc::new(FilesFind(CapabilityMeta {
        tool_name: "find".into(),
        api_path: vec!["files".into(), "find".into()],
        description: "Find files matching a glob pattern, respecting .gitignore.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["pattern"],
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 10000 },
                "maxDepth": { "type": "integer", "minimum": 1 },
                "exclude": { "type": "array", "items": { "type": "string" } },
                "includeHidden": { "type": "boolean" },
                "pathFormat": { "type": "string", "enum": ["relative", "full"] }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["pattern"]),
    })));

    catalog.register(Arc::new(FilesFindMany(CapabilityMeta {
        tool_name: "__cant_files_find_many".into(),
        api_path: vec!["files".into(), "findMany".into()],
        description: "Evaluate up to 16 glob patterns against one directory and return paths grouped by pattern.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["patterns"],
            "properties": {
                "patterns": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 16 },
                "path": { "type": "string" },
                "limitPerPattern": { "type": "integer", "minimum": 1, "maximum": 1000 },
                "maxDepth": { "type": "integer", "minimum": 1 },
                "exclude": { "type": "array", "items": { "type": "string" } },
                "includeHidden": { "type": "boolean" },
                "pathFormat": { "type": "string", "enum": ["relative", "full"] }
            }
        }),
        result_schema: json!({ "type": "array" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["patterns"]),
    })));

    catalog.register(Arc::new(FilesList(CapabilityMeta {
        tool_name: "__cant_files_list".into(),
        api_path: vec!["files".into(), "list".into()],
        description: "List directory entries with structured metadata, bounded recursion, and .gitignore filtering.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": { "type": "string" },
                "depth": { "type": "integer", "minimum": 1, "maximum": 10 },
                "kind": { "type": "string", "enum": ["all", "files", "directories"] },
                "exclude": { "type": "array", "items": { "type": "string" } },
                "includeHidden": { "type": "boolean" },
                "pathFormat": { "type": "string", "enum": ["relative", "full"] },
                "limit": { "type": "integer", "minimum": 1, "maximum": 5000 },
                "respectGitignore": { "type": "boolean" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["path"]),
    })));

    catalog.register(Arc::new(FilesWrite(CapabilityMeta {
        tool_name: "write".into(),
        api_path: vec!["files".into(), "write".into()],
        description: "Create a new UTF-8 text file. Fails if the file already exists; use files/edit or files/replace for existing files.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "content"],
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["path", "content"]),
    })));

    catalog.register(Arc::new(FilesEdit(CapabilityMeta {
        tool_name: "edit".into(),
        api_path: vec!["files".into(), "edit".into()],
        description: "Edit a text file by replacing unique, non-overlapping exact oldText regions (use replace-text in Steel).".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "edits"],
            "properties": {
                "path": { "type": "string" },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["oldText", "newText"],
                        "properties": {
                            "oldText": { "type": "string" },
                            "newText": { "type": "string" }
                        }
                    }
                },
                "expectedSha256": { "type": "string" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["path", "edits"]),
    })));

    catalog.register(Arc::new(FilesReplace(CapabilityMeta {
        tool_name: "__cant_files_replace".into(),
        api_path: vec!["files".into(), "replace".into()],
        description: "Atomically replace an existing file after complete read coverage and exact expectedSha256 match.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "content", "expectedSha256"],
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" },
                "expectedSha256": { "type": "string" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["path", "content"]),
    })));
}
