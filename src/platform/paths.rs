//! 用户持久化数据统一存放在 ~/.local/openuuyc；旧目录只用于兼容迁移。
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// 每个进程只初始化一次；跨进程迁移由文件锁串行化，避免同时移动旧数据。
fn initialized_app_data() -> &'static Result<PathBuf, String> {
    static ROOT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        let xdg = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        initialize(home.as_deref(), xdg.as_deref()).map_err(|error| format!("{error:#}"))
    })
}

pub(crate) fn app_data() -> Option<PathBuf> {
    initialized_app_data().as_ref().ok().cloned()
}

pub(crate) fn require_app_data() -> Result<PathBuf> {
    initialized_app_data()
        .as_ref()
        .cloned()
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

fn initialize(home: Option<&Path>, xdg: Option<&Path>) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let home = home.context("无法确定用户数据目录（需要绝对路径 HOME）")?;
    let root = home.join(".local/openuuyc");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .context("无法创建用户数据目录")?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".migration.lock"))?;
    lock.lock().context("无法锁定旧数据目录迁移")?;

    // 新目录优先；旧 XDG 目录优先于默认位置。不覆盖同名文件，不跟随符号链接。
    let mut sources = vec![home.join(".local/OpenUUYC")];
    if let Some(xdg) = xdg {
        sources.extend([xdg.join("openuuyc"), xdg.join("OpenUUYC")]);
    }
    sources.extend([
        home.join(".local/share/openuuyc"),
        home.join(".local/share/OpenUUYC"),
    ]);
    for source in sources {
        if source != root {
            migrate_directory(&source, &root)
                .with_context(|| format!("无法迁移旧数据目录 {}", source.display()))?;
        }
    }
    Ok(root)
}

fn migrate_directory(source: &Path, target: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || source.canonicalize()? == target.canonicalize()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        match std::fs::symlink_metadata(&destination) {
            Ok(existing) if kind.is_dir() && existing.is_dir() => {
                migrate_directory(&entry.path(), &destination)?;
            }
            Ok(_) => {} // 同名冲突保留旧文件，绝不覆盖新目录中的用户配置。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::rename(entry.path(), &destination) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
                        // 自定义 XDG 目录可能位于另一文件系统，复制成功后才移除源文件。
                        if kind.is_dir() {
                            std::fs::create_dir(&destination)?;
                            std::fs::set_permissions(
                                &destination,
                                entry.metadata()?.permissions(),
                            )?;
                            migrate_directory(&entry.path(), &destination)?;
                        } else if kind.is_file() {
                            let temporary =
                                target.join(format!(".migration-{}", uuid::Uuid::new_v4()));
                            let copied = std::fs::copy(entry.path(), &temporary)
                                .and_then(|_| std::fs::rename(&temporary, &destination));
                            if copied.is_err() {
                                let _ = std::fs::remove_file(&temporary);
                            }
                            copied?;
                            std::fs::remove_file(entry.path())?;
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    // 只删除已经迁空的目录；冲突文件和符号链接留在原处供用户核对。
    if std::fs::read_dir(source)?.next().is_none() {
        std::fs::remove_dir(source)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("openuuyc-paths-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn merges_legacy_names_and_preserves_conflicts() {
        let sandbox = Sandbox::new();
        let home = sandbox.0.join("home");
        let xdg = sandbox.0.join("xdg");
        for (directory, name, value) in [
            (home.join(".local/share/OpenUUYC/logs"), "old.log", "log"),
            (home.join(".local/share/openuuyc"), "session.lock", "lock"),
            (home.join(".local/OpenUUYC"), "media.json", "old"),
            (home.join(".local/openuuyc"), "media.json", "new"),
            (home.join(".local/openuuyc/logs"), "new.log", "new-log"),
            (xdg.join("OpenUUYC"), "notifications.json", "settings"),
        ] {
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(name), value).unwrap();
        }
        let root = initialize(Some(&home), Some(&xdg)).unwrap();
        assert_eq!(root, home.join(".local/openuuyc"));
        for (name, value) in [
            ("logs/old.log", "log"),
            ("logs/new.log", "new-log"),
            ("session.lock", "lock"),
            ("media.json", "new"),
            ("notifications.json", "settings"),
        ] {
            assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), value);
        }
        assert_eq!(
            std::fs::read_to_string(home.join(".local/OpenUUYC/media.json")).unwrap(),
            "old"
        );
        assert!(!home.join(".local/share/OpenUUYC").exists());
        assert!(!home.join(".local/share/openuuyc").exists());
        assert_eq!(initialize(Some(&home), Some(&xdg)).unwrap(), root);
    }

    #[test]
    fn does_not_follow_legacy_symlinks() {
        let sandbox = Sandbox::new();
        let home = sandbox.0.join("home");
        let external = sandbox.0.join("external");
        std::fs::create_dir_all(home.join(".local/share")).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("keep"), "original").unwrap();
        std::os::unix::fs::symlink(&external, home.join(".local/share/OpenUUYC")).unwrap();
        let root = initialize(Some(&home), None).unwrap();
        assert!(!root.join("keep").exists());
        assert!(external.join("keep").exists());
    }

    #[test]
    fn xdg_alias_of_new_root_is_not_migrated_into_itself() {
        let sandbox = Sandbox::new();
        let home = sandbox.0.join("home");
        let root = initialize(Some(&home), None).unwrap();
        std::fs::write(root.join("keep.json"), "keep").unwrap();
        let alias = sandbox.0.join("alias");
        std::os::unix::fs::symlink(home.join(".local"), &alias).unwrap();
        assert_eq!(initialize(Some(&home), Some(&alias)).unwrap(), root);
        assert_eq!(
            std::fs::read_to_string(root.join("keep.json")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn requires_home() {
        assert!(initialize(None, Some(Path::new("/tmp"))).is_err());
    }
}
