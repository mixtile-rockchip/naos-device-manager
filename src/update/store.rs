use super::{Error, Result, Task, valid_id, valid_state};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub(super) const MAX_TASKS: usize = 32;
pub(super) const HISTORY: &str = "tasks.json";
pub(super) const PENDING: &str = "hawkbit-pending.json";
const MAX_JOURNAL: u64 = 1024 * 1024;

pub(super) struct Store {
    path: PathBuf,
    directory: File,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    format: u32,
    tasks: Option<Vec<Task>>,
}

impl Store {
    pub(super) fn open(dir: &Path) -> Result<Self> {
        if !dir.is_absolute()
            || !dir.components().any(|c| matches!(c, Component::Normal(_)))
            || dir.components().any(|c| matches!(c, Component::ParentDir))
        {
            return Err(Error::Invalid(
                "state directory must be absolute and non-root".into(),
            ));
        }
        let path: PathBuf = dir.components().collect();
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .map_err(Error::persistence)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(Error::persistence)?;
        directory
            .set_permissions(Permissions::from_mode(0o700))
            .map_err(Error::persistence)?;
        Ok(Self { path, directory })
    }

    pub(super) fn read(&self, name: &str, max: usize) -> Result<Option<Vec<Task>>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.path.join(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::persistence(error)),
        };
        let metadata = file.metadata().map_err(Error::persistence)?;
        if !metadata.is_file()
            || metadata.permissions().mode() & 0o777 != 0o600
            || metadata.len() > MAX_JOURNAL
        {
            return Err(Error::Persistence(
                "update journal must be bounded regular mode 0600".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_JOURNAL + 1)
            .read_to_end(&mut bytes)
            .map_err(Error::persistence)?;
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(Error::Persistence("update journal exceeds bound".into()));
        }
        let journal: Journal = serde_json::from_slice(&bytes).map_err(Error::persistence)?;
        let tasks = journal.tasks.unwrap_or_default();
        if journal.format != 1 || tasks.len() > max {
            return Err(Error::Persistence("invalid update journal format".into()));
        }
        let mut ids = HashSet::new();
        let mut requests = HashSet::new();
        for task in &tasks {
            if !valid_id(&task.id)
                || !ids.insert(&task.id)
                || !valid_state(&task.state)
                || !matches!(task.source.as_str(), "local" | "hawkbit")
                || task.created_at.timestamp() <= -62_135_596_800
                || task.updated_at.timestamp() <= -62_135_596_800
            {
                return Err(Error::Persistence("invalid update task record".into()));
            }
            if !task.request_id.is_empty() && !requests.insert(&task.request_id) {
                return Err(Error::Persistence(
                    "duplicate request_id in update journal".into(),
                ));
            }
        }
        Ok(Some(tasks))
    }

    pub(super) fn save(&self, name: &str, tasks: &[Task], max: usize) -> Result<()> {
        if tasks.len() > max {
            return Err(Error::Persistence(
                "update journal exceeds task bound".into(),
            ));
        }
        #[derive(Serialize)]
        struct WriteJournal<'a> {
            format: u32,
            tasks: &'a [Task],
        }
        let mut bytes = serde_json::to_vec_pretty(&WriteJournal { format: 1, tasks })
            .map_err(Error::persistence)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(Error::Persistence("update journal exceeds bound".into()));
        }
        let mut file = tempfile::Builder::new()
            .prefix(".tasks-")
            .tempfile_in(&self.path)
            .map_err(Error::persistence)?;
        file.as_file()
            .set_permissions(Permissions::from_mode(0o600))
            .map_err(Error::persistence)?;
        file.write_all(&bytes).map_err(Error::persistence)?;
        file.as_file().sync_all().map_err(Error::persistence)?;
        file.persist(self.path.join(name))
            .map_err(Error::persistence)?;
        self.sync_dir()
    }

    pub(super) fn clear_pending(&self) -> Result<()> {
        match fs::remove_file(self.path.join(PENDING)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::persistence(error)),
        }
        self.sync_dir()
    }

    fn sync_dir(&self) -> Result<()> {
        self.directory.sync_all().map_err(Error::persistence)
    }
}
