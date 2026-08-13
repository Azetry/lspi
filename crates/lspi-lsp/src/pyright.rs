use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

pub async fn resolve_pyright_command() -> Result<String> {
    if let Ok(value) = std::env::var("LSPI_PYRIGHT_COMMAND")
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    Ok("pyright-langserver".to_string())
}

pub async fn resolve_basedpyright_command() -> Result<String> {
    if let Ok(value) = std::env::var("LSPI_BASEDPYRIGHT_COMMAND")
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    Ok("basedpyright-langserver".to_string())
}

pub async fn preflight_pyright(command: &str) -> Result<()> {
    let paths = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    if resolve_command_in_paths(command, paths).is_some() {
        return Ok(());
    }

    Err(anyhow!(
        "pyright-langserver is not available on PATH. Install Pyright (e.g. `npm i -g pyright`) and ensure `pyright-langserver` is runnable, or set LSPI_PYRIGHT_COMMAND / LSPI_BASEDPYRIGHT_COMMAND."
    ))
}

fn resolve_command_in_paths(
    command: &str,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    let command_path = Path::new(command);
    if command_path.is_absolute() || command_path.components().count() > 1 {
        return executable_candidates(command_path).find(|candidate| is_executable(candidate));
    }

    for directory in paths {
        if let Some(candidate) = executable_candidates(&directory.join(command))
            .find(|candidate| is_executable(candidate))
        {
            return Some(candidate);
        }
    }
    None
}

fn executable_candidates(path: &Path) -> impl Iterator<Item = PathBuf> {
    #[cfg(not(windows))]
    let candidates = vec![path.to_path_buf()];
    #[cfg(windows)]
    let candidates = {
        let mut candidates = vec![path.to_path_buf()];
        if path.extension().is_none() {
            let extensions = std::env::var_os("PATHEXT")
                .and_then(|value| value.into_string().ok())
                .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
            candidates.extend(
                extensions
                    .split(';')
                    .filter(|extension| !extension.trim().is_empty())
                    .map(|extension| {
                        let extension = extension.trim().trim_start_matches('.');
                        path.with_extension(extension)
                    }),
            );
        }
        candidates
    };
    candidates.into_iter()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    #[test]
    fn resolves_language_server_without_running_help_or_version() {
        let directory = tempdir().unwrap();
        let executable = directory.path().join("pyright-langserver");
        std::fs::write(&executable, "language server fixture").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            super::resolve_command_in_paths("pyright-langserver", [directory.path().to_path_buf()]),
            Some(executable)
        );
        assert_eq!(
            super::resolve_command_in_paths("missing-langserver", [PathBuf::from("/missing")]),
            None
        );
    }
}
