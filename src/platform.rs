use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;

mod process;

pub(crate) use process::{ProcessSnapshot, ProcessTree, foreground_process_snapshot};
pub use process::resolve_executable;

#[inline]
pub(crate) fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn spawn_named<T, F>(name: impl Into<String>, task: F) -> io::Result<JoinHandle<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    std::thread::Builder::new().name(name.into()).spawn(task)
}

pub(crate) fn reap_unit_thread(worker: JoinHandle<()>) {
    static REAPER: std::sync::OnceLock<std::sync::mpsc::SyncSender<JoinHandle<()>>> =
        std::sync::OnceLock::new();
    let sender = REAPER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<JoinHandle<()>>(32);
        let _ = spawn_named("ronsole-thread-reaper", move || {
            while let Ok(worker) = rx.recv() {
                let _ = worker.join();
            }
        });
        tx
    });
    if let Err(error) = sender.try_send(worker) {
        match error {
            std::sync::mpsc::TrySendError::Full(worker)
            | std::sync::mpsc::TrySendError::Disconnected(worker) => {
                if worker.is_finished() {
                    let _ = worker.join();
                } else {
                    drop(worker);
                }
            }
        }
    }
}

pub(crate) fn user_home_dir() -> Option<PathBuf> {
    user_home_dir_with(|name| std::env::var_os(name))
}

pub(crate) fn config_home_dir() -> Option<PathBuf> {
    config_home_dir_with(|name| std::env::var_os(name))
}

fn user_home_dir_with(mut env_value: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    env_value("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn config_home_dir_with(mut env_value: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    env_value("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            env_value("HOME")
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
                .map(|home| home.join(".config"))
        })
}


pub struct Clipboard {
    inner: arboard::Clipboard,
}

impl Clipboard {
    pub fn new() -> Result<Self, arboard::Error> {
        arboard::Clipboard::new().map(|inner| Self { inner })
    }

    pub fn set_text(&mut self, text: String) -> Result<(), arboard::Error> {
        self.inner.set_text(text)
    }

    pub fn get_text(&mut self) -> Result<String, arboard::Error> {
        self.inner.get_text()
    }

    pub fn get_file_list(&mut self) -> Result<Vec<PathBuf>, arboard::Error> {
        let mut paths = self.inner.get().file_list()?;
        normalize_linux_arboard_file_list(&mut paths);
        Ok(paths)
    }
}

pub(crate) fn normalize_linux_arboard_file_list(paths: &mut [PathBuf]) {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    for path in paths {
        if path.as_os_str().as_bytes().last() != Some(&b'\r') {
            continue;
        }
        let mut bytes = std::mem::take(path).into_os_string().into_vec();
        bytes.pop();
        *path = PathBuf::from(OsString::from_vec(bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_file_list_normalization_removes_only_transport_cr() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut paths = vec![
            PathBuf::from(OsString::from_vec(b"/tmp/a\r".to_vec())),
            PathBuf::from(OsString::from_vec(b"/tmp/b".to_vec())),
        ];
        normalize_linux_arboard_file_list(&mut paths);
        assert_eq!(paths[0].as_os_str().as_bytes(), b"/tmp/a");
        assert_eq!(paths[1].as_os_str().as_bytes(), b"/tmp/b");
    }

    #[test]
    fn user_home_directory_is_strictly_linux_home() {
        assert_eq!(
            user_home_dir_with(|name| (name == "HOME").then(|| OsString::from("/home/reyan"))),
            Some(PathBuf::from("/home/reyan"))
        );
        assert_eq!(
            user_home_dir_with(|name| (name == "USERPROFILE").then(|| OsString::from("C:/Users/x"))),
            None
        );
        assert_eq!(user_home_dir_with(|_| Some(OsString::new())), None);
    }

    #[test]
    fn config_home_prefers_xdg_and_falls_back_to_linux_home() {
        assert_eq!(
            config_home_dir_with(|name| match name {
                "XDG_CONFIG_HOME" => Some(OsString::from("/tmp/xdg")),
                "HOME" => Some(OsString::from("/home/reyan")),
                _ => None,
            }),
            Some(PathBuf::from("/tmp/xdg"))
        );
        assert_eq!(
            config_home_dir_with(|name| {
                (name == "HOME").then(|| OsString::from("/home/reyan"))
            }),
            Some(PathBuf::from("/home/reyan/.config"))
        );
        assert_eq!(
            config_home_dir_with(|name| {
                (name == "XDG_CONFIG_HOME").then(OsString::new)
            }),
            None
        );
    }

}
