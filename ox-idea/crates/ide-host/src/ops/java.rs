//! Java launch support: classpath / JDK resolution and the java-debug server (jdtls).

use crate::Host;
use crate::rpc::Dispatcher;

use super::build::load;
use ide_build::compile;
use ide_core::{CancellationToken, IdeError, IdeResult, RequestId};
use ide_lsp::jdtls::{JavaBuildStatus, JavaLaunchInfo};
use ide_run::BuildPolicy;
use std::path::PathBuf;

/// Cancellable with `cancel_request(request)`.
/// Builds the workspace (when `build`), then resolves the runtime (or test)
/// classpath and the JDK of the module at `module` (absolute, or relative to the project root).
/// A module with Kotlin / Groovy / Scala sources is built by its build system, whose class directories come first.
pub async fn java_resolve_launch(
    host: &Host,
    window: &str,
    module: PathBuf,
    test: bool,
    policy: BuildPolicy,
    request: RequestId,
) -> IdeResult<JavaLaunchInfo> {
    host.require_trust(window)?;
    let cancel = host.begin_project_request(window, request);
    let result = resolve_launch(host, window, module, test, policy, &cancel).await;
    host.end_request(request);
    result
}

async fn resolve_launch(
    host: &Host,
    window: &str,
    module: PathBuf,
    test: bool,
    policy: BuildPolicy,
    cancel: &CancellationToken,
) -> IdeResult<JavaLaunchInfo> {
    let started = std::time::Instant::now();
    let project = host.project(window)?;
    let module = project.root.join(module);
    let settings = project.settings.effective();
    // A launch builds incrementally unless the configuration asks for Rebuild or NoBuild.
    let build = policy != BuildPolicy::NoBuild;
    let full = policy == BuildPolicy::Rebuild;
    let build_kind = if full { compile::BuildKind::Rebuild } else { compile::BuildKind::Build };
    let (model, external) = {
        let project = project.clone();
        let settings = settings.clone();
        let module = module.clone();
        tokio::task::spawn_blocking(move || {
            let model = load(&project.root, &project.build_cache_dir(), &settings);
            let external = build && compile::launch_has_non_java_sources(&project.root, &model, &module, test);
            (model, external)
        })
        .await
        .map_err(|e| IdeError::internal(e.to_string()))?
    };
    // Apply the jdtls bundle's staging redirect before the build: ECJ then writes
    // target/oxidea-ide/... and the launch publishes it into target/ (see publish_and_map).
    // Best effort: an unreachable server or an older bundle just means an unredirected launch.
    let _ = project
        .lsp
        .jdtls_command_cancellable(&project.root, "oxidea.java.build.redirect", Vec::new(), cancel)
        .await;
    let mut info = project.lsp.java_launch(&module, test, build, full, false, cancel).await?;
    let lsp_ms = started.elapsed().as_millis();
    let data_dir = host.local_data_dir.clone();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let built = if external {
            let built = compile::build_for_launch(&project.root, &model, &module, test, build_kind, &cancel)?;
            if let Some(failure) = built.failure {
                if policy != BuildPolicy::MakeAndContinueOnErrors {
                    return Err(IdeError::internal(failure));
                }
                info.build = Some(JavaBuildStatus::Failed);
                info.hints.push(failure);
            }
            built.class_dirs
        } else {
            Vec::new()
        };
        let mut class_paths: Vec<String> = built.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        for p in std::mem::take(&mut info.class_paths) {
            if !class_paths.iter().any(|c| same_entry(c, &p, cfg!(windows))) {
                class_paths.push(p);
            }
        }
        info.class_paths = class_paths;
        // Publish the bundle's staging build into the shared target/ tree and point the
        // launch at the published entries (see publish_and_map).
        let build_ok = matches!(info.build, None | Some(JavaBuildStatus::Succeeded));
        publish_and_map(&mut info, &built, build_ok);
        apply_library_classpath(&mut info, &project.root, &settings.project_structure.libraries, test);
        let compile_ms = started.elapsed().as_millis() - lsp_ms;
        // The module/project JDK, else the JDK jdtls compiles the module against, else one matching its Java version.
        let project_jdk = ide_sdk::apply::module_jdk(&project.root, &module, &settings);
        let from_jdtls = info.java_home.as_deref().map(ide_sdk::java::java_executable).filter(|p| p.is_file());
        let java = project_jdk.map(|j| ide_sdk::java::java_executable(&j.home)).or(from_jdtls).or_else(|| {
            // The build model's level includes what the module inherits from its parent POMs.
            let release = model
                .maven
                .as_ref()
                .and_then(|m| m.projects.iter().find(|p| project.root.join(&p.dir) == module))
                .and_then(|p| p.java_version)
                .or_else(|| ide_sdk::java::project_java_version(&module));
            let jdks = ide_sdk::java::installed_jdks();
            info.hints.extend(ide_sdk::java::missing_jdk_message(&jdks, release));
            ide_sdk::java::pick_jdk(&jdks, release).map(|j| ide_sdk::java::java_executable(&j.home))
        });
        info.java_exec = Some(java.map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "java".to_owned()));
        let exec = PathBuf::from(info.java_exec.as_deref().unwrap_or("java"));
        let shorten =
            ide_run::java_cmdline::shortening_for(&exec, &info.class_paths, &info.module_paths, cfg!(windows));
        info.shorten_command_line = Some(shorten.as_str().to_owned());
        if info.error_count > 0 {
            let own = ide_sdk::lombok::agent_in_args(settings.languages.java.jdtls_vm_args.split_whitespace());
            let managed = ide_tools::managed_bundle(&data_dir, "lombok");
            let repo = ide_sdk::apply::maven_local_repository(&project.root, &settings);
            let root = project.root.clone();
            // Detection may read the project's sources.
            let lombok = ide_sdk::lombok::lombok_agent(&root, own, managed, repo);
            info.hints.extend(lombok.problem());
        }
        if cancel.is_cancelled() {
            return Err(IdeError::cancelled());
        }
        tracing::info!(
            lsp_ms,
            compile_ms,
            jdk_ms = started.elapsed().as_millis() - lsp_ms - compile_ms,
            total_ms = started.elapsed().as_millis(),
            "Java launch resolved"
        );
        Ok(info)
    })
    .await
    .map_err(|e| IdeError::internal(e.to_string()))?
}

/// A classpath entry as a comparable key. Entries come from JDT LS and from our own
/// build model, which spell one directory differently on Windows (`\\?\` prefix,
/// `/` or `\\`, drive-letter case); compared as text, a module output stayed on the
/// classpath next to its launch snapshot and every resource was found twice.
fn entry_key(path: &str, windows: bool) -> String {
    let path = match path.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => path.strip_prefix(r"\\?\").unwrap_or(path).to_owned(),
    };
    let mut key = if windows { path.replace('\\', "/") } else { path };
    while key.len() > 1 && key.ends_with('/') {
        key.pop();
    }
    if windows { key.to_lowercase() } else { key }
}

fn apply_library_classpath(
    info: &mut JavaLaunchInfo,
    root: &std::path::Path,
    libraries: &[ide_core::settings::ProjectLibrary],
    test: bool,
) {
    use ide_core::settings::DependencyScope;
    let entries: Vec<_> = libraries
        .iter()
        .flat_map(|lib| {
            let allowed = match lib.scope {
                DependencyScope::Compile | DependencyScope::Runtime => true,
                DependencyScope::Test | DependencyScope::Provided => test,
            };
            lib.classes.iter().filter_map(move |entry| {
                ide_sdk::apply::setting_path(root, entry).map(|path| (path.to_string_lossy().into_owned(), allowed))
            })
        })
        .collect();
    // JDT may already have returned these archives on either classpath.
    for paths in [&mut info.class_paths, &mut info.module_paths] {
        paths.retain(|path| {
            let matching: Vec<_> = entries.iter().filter(|(entry, _)| same_entry(path, entry, cfg!(windows))).collect();
            matching.is_empty() || matching.iter().any(|(_, allowed)| *allowed)
        });
    }
    for (entry, allowed) in entries {
        if allowed
            && !info.class_paths.iter().chain(&info.module_paths).any(|path| same_entry(path, &entry, cfg!(windows)))
        {
            info.class_paths.push(entry);
        }
    }
}

fn same_entry(a: &str, b: &str, windows: bool) -> bool {
    a == b || entry_key(a, windows) == entry_key(b, windows)
}

/// Publishes the jdtls bundle's staging build into the shared `target/` tree and points the
/// launch at the published entries. The bundle redirects ECJ outputs and APT generated
/// sources into `.../target/oxidea-ide/...` so the delete-and-regenerate
/// annotation-processing cycle never touches the tree that terminal Maven builds write and
/// running JVMs read; a successful staging build is copied back over `target/<rest>` with
/// atomic per-file renames. Unredirected entries pass through unchanged, so a missing or
/// failed bundle degrades to the previous behavior.
fn publish_and_map(info: &mut JavaLaunchInfo, external_dirs: &[PathBuf], build_ok: bool) {
    for entry in info.class_paths.iter_mut().chain(info.module_paths.iter_mut()) {
        let Some(published) = published_entry(entry) else { continue };
        if build_ok {
            sync_staging(std::path::Path::new(entry), std::path::Path::new(&published), external_dirs);
        }
        *entry = published;
    }
    for paths in [&mut info.class_paths, &mut info.module_paths] {
        let mut unique: Vec<String> = Vec::with_capacity(paths.len());
        for path in paths.drain(..) {
            if !unique.iter().any(|u| same_entry(u, &path, cfg!(windows))) {
                unique.push(path);
            }
        }
        *paths = unique;
    }
}

/// The published form of a staging classpath entry: `.../target/oxidea-ide/<rest>` →
/// `.../target/<rest>`; `None` for entries outside a staging tree.
fn published_entry(entry: &str) -> Option<String> {
    let norm = entry.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').collect();
    let at = parts.iter().position(|s| *s == "oxidea-ide")?;
    if at == 0 || parts[at - 1] != "target" {
        return None;
    }
    let mut out = String::with_capacity(norm.len());
    for (i, part) in parts.iter().enumerate() {
        if i == at {
            continue;
        }
        if i > 0 {
            out.push('/');
        }
        out.push_str(part);
    }
    Some(out)
}

/// Copies the staging tree over the published tree: changed files go through a temp name +
/// rename (atomic per file, so a lazily loading JVM never reads a half-written class) and
/// vanished files are pruned — unless the published tree was also written by this launch's
/// external (non-Java) build, whose files staging does not own. Best effort: a locked file
/// keeps its previous complete version until the next launch.
fn sync_staging(staged: &std::path::Path, published: &std::path::Path, external_dirs: &[PathBuf]) {
    let prune = !external_dirs.iter().any(|dir| same_path(dir, published));
    if let Err(e) = sync_tree(staged, published, prune) {
        tracing::warn!("staging publish {} → {}: {e}", staged.display(), published.display());
    }
}

fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    same_entry(&a.to_string_lossy(), &b.to_string_lossy(), cfg!(windows))
}

fn sync_tree(from: &std::path::Path, to: &std::path::Path, prune: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            sync_tree(&source, &target, prune)?;
        } else if stale(&source, &target) {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let temp = to.join(format!(".oxidea-{}-{nonce}.tmp", std::process::id()));
            std::fs::copy(&source, &temp)?;
            if std::fs::rename(&temp, &target).is_err() {
                // The file may be locked by a running JVM; keep the previous complete version.
                let _ = std::fs::remove_file(&temp);
            }
        }
    }
    if prune {
        for entry in std::fs::read_dir(to)? {
            let entry = entry?;
            if !from.join(entry.file_name()).exists() {
                let target = entry.path();
                if target.is_dir() {
                    std::fs::remove_dir_all(&target)?;
                } else {
                    let _ = std::fs::remove_file(&target);
                }
            }
        }
    }
    Ok(())
}

/// Whether the published copy is missing or older than the staging original.
fn stale(source: &std::path::Path, target: &std::path::Path) -> bool {
    let (Ok(source_meta), Ok(target_meta)) = (std::fs::metadata(source), std::fs::metadata(target)) else {
        return true;
    };
    if source_meta.len() != target_meta.len() {
        return true;
    }
    match (source_meta.modified(), target_meta.modified()) {
        (Ok(source), Ok(target)) => source > target,
        _ => true,
    }
}

#[cfg(test)]
mod publish_tests {
    use super::*;

    #[test]
    fn maps_staging_entries_to_their_published_form() {
        assert_eq!(
            published_entry(r"F:\code\omsx\target\oxidea-ide\classes"),
            Some("F:/code/omsx/target/classes".to_owned())
        );
        assert_eq!(
            published_entry("/proj/target/oxidea-ide/test-classes"),
            Some("/proj/target/test-classes".to_owned())
        );
        assert_eq!(published_entry("F:/code/omsx/target/classes"), None);
        assert_eq!(published_entry("F:/code/omsx/target/oxidea/classes"), None);
        assert_eq!(published_entry("F:/code/oxidea-ide/classes"), None);
    }

    #[test]
    fn syncs_changed_files_and_prunes_vanished_ones() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        let published = dir.path().join("published");
        std::fs::create_dir_all(staged.join("ai")).unwrap();
        std::fs::write(staged.join("ai/A.class"), b"v1").unwrap();
        std::fs::write(staged.join("ai/B.class"), b"b").unwrap();
        sync_tree(&staged, &published, true).unwrap();
        assert_eq!(std::fs::read(published.join("ai/A.class")).unwrap(), b"v1");
        // A changed file is replaced, a vanished one is pruned.
        std::fs::write(staged.join("ai/A.class"), b"v2-longer").unwrap();
        std::fs::remove_file(staged.join("ai/B.class")).unwrap();
        sync_tree(&staged, &published, true).unwrap();
        assert_eq!(std::fs::read(published.join("ai/A.class")).unwrap(), b"v2-longer");
        assert!(!published.join("ai/B.class").exists());
        // Without prune, files staging does not own survive (external build output).
        std::fs::write(published.join("ai/Kotlin.class"), b"k").unwrap();
        sync_tree(&staged, &published, false).unwrap();
        assert!(published.join("ai/Kotlin.class").exists());
    }
}

/// Port of java-debug's DAP server inside the jdtls serving `module` (started on demand).
pub async fn java_debug_server(host: &Host, window: &str, module: PathBuf) -> IdeResult<u16> {
    host.require_trust(window)?;
    let project = host.project(window)?;
    let module = project.root.join(module);
    project.lsp.java_debug_server(&module).await
}

/// Incremental build of the jdtls workspace serving `module` (e.g. before HotSwap).
pub async fn java_build_workspace(
    host: &Host,
    window: &str,
    module: PathBuf,
    request: Option<RequestId>,
) -> IdeResult<ide_lsp::jdtls::JavaBuildStatus> {
    host.require_trust(window)?;
    let project = host.project(window)?;
    let module = project.root.join(module);
    // HotSwap may cancel the compile (the request id is allocated by the caller).
    let cancel = request.map_or_else(CancellationToken::new, |id| host.begin_request(id));
    let result = project.lsp.java_build(&module, false, &cancel).await;
    if let Some(id) = request {
        host.end_request(id);
    }
    result
}

/// Commands of this module the remote server answers.
pub(crate) fn register(d: &mut Dispatcher) {
    d.add("java_resolve_launch", |c| async move {
        java_resolve_launch(&c.host, &c.window, c.arg("module")?, c.arg("test")?, c.arg("policy")?, c.arg("request")?)
            .await
    });
    d.add("java_debug_server", |c| async move { java_debug_server(&c.host, &c.window, c.arg("module")?).await });
    d.add("java_build_workspace", |c| async move {
        java_build_workspace(&c.host, &c.window, c.arg("module")?, c.arg("request")?).await
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_scopes_filter_jdt_classpaths_and_module_paths() {
        use ide_core::settings::{DependencyScope, ProjectLibrary};
        let root = tempfile::tempdir().unwrap();
        let scopes =
            [DependencyScope::Compile, DependencyScope::Runtime, DependencyScope::Test, DependencyScope::Provided];
        let libraries: Vec<_> = scopes
            .iter()
            .enumerate()
            .map(|(i, scope)| ProjectLibrary {
                classes: vec![format!("lib/{i}.jar")],
                scope: *scope,
                ..Default::default()
            })
            .collect();
        let paths: Vec<_> =
            (0..4).map(|i| root.path().join(format!("lib/{i}.jar")).to_string_lossy().into_owned()).collect();
        for test in [false, true] {
            let mut info =
                JavaLaunchInfo { class_paths: paths.clone(), module_paths: paths.clone(), ..Default::default() };
            apply_library_classpath(&mut info, root.path(), &libraries, test);
            let expected = if test { &paths[..] } else { &paths[..2] };
            assert_eq!(info.class_paths, expected);
            assert_eq!(info.module_paths, expected);
        }
        let mut info = JavaLaunchInfo::default();
        apply_library_classpath(&mut info, root.path(), &libraries, false);
        assert_eq!(info.class_paths, &paths[..2]);
    }

    /// JDT LS and our build model spell one classpath entry differently on Windows
    /// (`\\?\` prefixes, `/` vs `\`, drive-letter case, trailing slashes); compared as
    /// text, a module output stayed on the classpath next to itself and every resource
    /// (MyBatis mapper XML) loaded twice.
    #[test]
    fn entry_keys_match_jdt_and_build_model_spellings() {
        assert!(same_entry(r"\\?\UNC\server\share\app\target\classes", r"\\server\share\app\target/classes", true));
        // Different directories stay different, and other platforms keep case and `\`.
        assert!(!same_entry(r"D:\a\target\classes", r"D:\a\target\test-classes", true));
        assert!(!same_entry("/work/App/target/classes", "/work/app/target/classes", false));
        assert!(!same_entry(r"/work/a\b", "/work/a/b", false));
        assert!(same_entry("/work/app/target/classes/", "/work/app/target/classes", false));
    }
}
