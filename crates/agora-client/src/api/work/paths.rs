//! Path canonicalisation for the work client: the signed form of a task's cwd and files, the cwd and `$HOME` they are
//! resolved against, and the absolute form of the paths a `work wait` names.

use crate::ClientError;
use agora_sched::claims;
use agora_sched::model::{AddArgs, WaitSpec};
use std::path::Path;

/// [`canonicalise_add`] for a task that names a command, a cwd or files; `base` is read only then.
pub(super) fn prepare_add(
    a: &mut AddArgs,
    base: impl FnOnce() -> Result<String, ClientError>,
    home: Option<&str>,
) -> Result<(), ClientError> {
    if a.command.is_some() || a.cwd.is_some() || !a.files.is_empty() {
        canonicalise_add(a, &base()?, home)?;
    }
    Ok(())
}

/// Canonical `cwd` (default `base`; relative values resolve against `base`) and canonical `files` (relative values
/// resolve against that cwd), written back into `a`.
pub(super) fn canonicalise_add(
    a: &mut AddArgs,
    base: &str,
    home: Option<&str>,
) -> Result<(), ClientError> {
    let raw = a.cwd.take().unwrap_or_else(|| base.to_string());
    let cwd = canonical_file(&raw, base, home)?;
    a.files = a
        .files
        .iter()
        .map(|file| canonical_file(file, &cwd, home))
        .collect::<Result<Vec<_>, _>>()?;
    a.cwd = Some(cwd);
    Ok(())
}

/// The signed form of one path: `~` expanded with `home`, a relative path joined to `base`, the longest existing
/// prefix resolved by `std::fs::canonicalize`, then normalised lexically by `claims::canonical_path`.
pub(super) fn canonical_file(
    path: &str,
    base: &str,
    home: Option<&str>,
) -> Result<String, ClientError> {
    if path.is_empty() || path.contains('\0') {
        return Err(ClientError::Usage(format!("{path:?} is not a file path")));
    }
    let expanded = expand_home(path, home);
    let absolute = if expanded.starts_with('/') {
        expanded
    } else {
        format!("{base}/{expanded}")
    };
    let resolved = resolve_links(&absolute);
    claims::canonical_path(&resolved, base, home).map_err(ClientError::Usage)
}

/// `path` with a leading `~` or `~/` replaced by `home`; any other path is returned unchanged.
fn expand_home(path: &str, home: Option<&str>) -> String {
    let (Some(home), Some(rest)) = (home, path.strip_prefix('~')) else {
        return path.to_string();
    };
    match rest.strip_prefix('/') {
        Some(tail) => format!("{home}/{tail}"),
        None if rest.is_empty() => home.to_string(),
        None => path.to_string(),
    }
}

/// `absolute` with its longest existing ancestor replaced by that ancestor's canonical path; the rest is appended.
fn resolve_links(absolute: &str) -> String {
    let path = Path::new(absolute);
    for ancestor in path.ancestors() {
        let Ok(real) = std::fs::canonicalize(ancestor) else {
            continue;
        };
        return match path.strip_prefix(ancestor) {
            Ok(rest) if !rest.as_os_str().is_empty() => {
                real.join(rest).to_string_lossy().into_owned()
            }
            _ => real.to_string_lossy().into_owned(),
        };
    }
    absolute.to_string()
}

pub(super) fn current_dir_string() -> Result<String, ClientError> {
    std::env::current_dir()?
        .into_os_string()
        .into_string()
        .map_err(|_| ClientError::Usage("the current directory is not valid UTF-8".into()))
}

/// `$HOME`, when it is set and valid UTF-8.
pub(super) fn home_dir() -> Option<String> {
    std::env::var_os("HOME").and_then(|home| home.into_string().ok())
}

/// `spec` with its file, gone and log paths made absolute against `base`, which is read only for a spec that names a
/// path. Every other condition is returned unchanged.
pub(super) fn absolute_spec(
    spec: WaitSpec,
    base: impl FnOnce() -> Result<String, ClientError>,
) -> Result<WaitSpec, ClientError> {
    match spec {
        WaitSpec::File { paths } => Ok(WaitSpec::File {
            paths: absolute_paths(&paths, &base()?)?,
        }),
        WaitSpec::Gone { paths } => Ok(WaitSpec::Gone {
            paths: absolute_paths(&paths, &base()?)?,
        }),
        WaitSpec::Log { path, pattern } => Ok(WaitSpec::Log {
            path: absolute_path(&path, &base()?)?,
            pattern,
        }),
        other => Ok(other),
    }
}

fn absolute_paths(paths: &[String], base: &str) -> Result<Vec<String>, ClientError> {
    paths.iter().map(|path| absolute_path(path, base)).collect()
}

/// `path` joined onto `base`; `Path::join` keeps an absolute path as it is. An empty or NUL path is refused, as in
/// [`canonical_file`]. Both inputs are UTF-8, so the joined path is too.
fn absolute_path(path: &str, base: &str) -> Result<String, ClientError> {
    if path.is_empty() || path.contains('\0') {
        return Err(ClientError::Usage(format!("{path:?} is not a file path")));
    }
    Ok(Path::new(base).join(path).to_string_lossy().into_owned())
}
