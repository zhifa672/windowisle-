//! 从「岛仔 Daozai」改名为「窗屿 WindowIsle」后的一次性迁移。
//!
//! 改名换掉了几样「身份」：数据目录、凭据管理器里的服务名、开机自启项、
//! 单实例锁、托盘窗口类、内存清理服务。这里负责把旧版留下的数据接过来：
//!
//! - 数据目录 `%APPDATA%\Daozai` → `%APPDATA%\WindowIsle`：启动时整体搬过去
//!   （同盘重命名；失败时复制，复制不完整就撤销，保证旧数据原样留着）；
//! - 凭据：读取时新名下没有，就从旧名下取出、存到新名下、确认存好后删旧的；
//! - 开机自启、清理服务分别在 tray.rs、win-mem-cleaner 里处理。
//!
//! 旧版本没人用之后，可以整个删掉这个模块。

use std::path::Path;

/// 改名前的数据目录名（`%APPDATA%\Daozai`）。
pub const LEGACY_DATA_DIR: &str = "Daozai";

/// 把旧数据目录搬到新位置。返回 `None` 表示不需要迁移（没有旧数据，或已经迁移过）。
///
/// 新目录已经存在时不动任何东西：说明迁移过了，或者新版已经开始写数据，
/// 这时再合并会覆盖新数据。
pub fn migrate_data_dir(old: &Path, new: &Path) -> Option<Result<(), String>> {
    if new.exists() || !old.is_dir() {
        return None;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 同一个盘上重命名是原子的，最常见也最安全。
    if std::fs::rename(old, new).is_ok() {
        return Some(Ok(()));
    }
    // 重命名不了（比如旧版还开着、文件被占用）：复制一份，旧目录原样保留。
    match copy_dir(old, new) {
        Ok(()) => Some(Ok(())),
        Err(error) => {
            // 复制了一半：删掉不完整的新目录，下次启动重新迁移。
            let _ = std::fs::remove_dir_all(new);
            Some(Err(format!("无法迁移「岛仔」的数据（{}）：{error}", old.display())))
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// 启动时迁移应用数据目录。用 `WINDOWISLE_DATA_DIR` 指定了目录（开发、测试）时不迁移。
pub fn migrate_app_data() -> Option<Result<(), String>> {
    if std::env::var_os("WINDOWISLE_DATA_DIR").is_some() {
        return None;
    }
    let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA")?);
    migrate_data_dir(&appdata.join(LEGACY_DATA_DIR), &crate::model::config_dir())
}

/// 凭据迁移：`service` 名下没有 `user` 这一条时，从改名前的 `legacy` 名下取出，
/// 存到新名下；确认能读回同样的内容后才删旧的。失败静默，旧凭据保留，下次再试。
#[cfg(windows)]
pub fn adopt_credential(service: &str, legacy: &str, user: &str) {
    let Ok(current) = keyring::Entry::new(service, user) else { return };
    if current.get_password().is_ok() {
        return;
    }
    let Ok(old) = keyring::Entry::new(legacy, user) else { return };
    let Ok(secret) = old.get_password() else { return };
    if current.set_password(&secret).is_ok() && current.get_password().ok().as_deref() == Some(secret.as_str()) {
        let _ = old.delete_credential();
    }
}

/// 删除凭据时，改名前留下的同一条也一起删掉。
#[cfg(windows)]
pub fn delete_legacy_credential(legacy: &str, user: &str) {
    if let Ok(old) = keyring::Entry::new(legacy, user) {
        let _ = old.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("windowisle-legacy-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn data_dir_moves_once_with_nested_files() {
        let root = temp("move");
        let (old, new) = (root.join("Daozai"), root.join("WindowIsle"));
        std::fs::create_dir_all(old.join("clipboard")).unwrap();
        std::fs::write(old.join("settings.txt"), "pet=1\n").unwrap();
        std::fs::write(old.join("clipboard").join("a.bin"), [1u8, 2, 3]).unwrap();

        assert_eq!(migrate_data_dir(&old, &new), Some(Ok(())));
        assert_eq!(std::fs::read_to_string(new.join("settings.txt")).unwrap(), "pet=1\n");
        assert_eq!(std::fs::read(new.join("clipboard").join("a.bin")).unwrap(), [1, 2, 3]);
        // 再次启动：已经迁移过，什么也不做。
        assert_eq!(migrate_data_dir(&old, &new), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_new_dir_is_never_overwritten() {
        let root = temp("keep");
        let (old, new) = (root.join("Daozai"), root.join("WindowIsle"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("settings.txt"), "old").unwrap();
        std::fs::write(new.join("settings.txt"), "new").unwrap();
        assert_eq!(migrate_data_dir(&old, &new), None);
        assert_eq!(std::fs::read_to_string(new.join("settings.txt")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old.join("settings.txt")).unwrap(), "old", "旧数据原样保留");
        // 没有旧数据：也不需要迁移。
        assert_eq!(migrate_data_dir(&root.join("missing"), &root.join("other")), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
