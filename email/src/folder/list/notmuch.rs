use async_trait::async_trait;
use tracing::info;

use super::ListFolders;
use crate::{
    folder::{Folder, Folders},
    notmuch::{is_raw_notmuch_query, NotmuchContextSync},
    AnyResult,
};

pub struct ListNotmuchFolders {
    ctx: NotmuchContextSync,
}

impl ListNotmuchFolders {
    pub fn new(ctx: &NotmuchContextSync) -> Self {
        Self { ctx: ctx.clone() }
    }

    pub fn new_boxed(ctx: &NotmuchContextSync) -> Box<dyn ListFolders> {
        Box::new(Self::new(ctx))
    }

    pub fn some_new_boxed(ctx: &NotmuchContextSync) -> Option<Box<dyn ListFolders>> {
        Some(Self::new_boxed(ctx))
    }
}

#[async_trait]
impl ListFolders for ListNotmuchFolders {
    async fn list_folders(&self) -> AnyResult<Folders> {
        info!("listing notmuch folders via maildir");

        let ctx = self.ctx.lock().await;
        let mut folders = Folders::from_maildir_context(&ctx.mdir_ctx);

        // Append user-defined folder aliases whose value is a raw
        // notmuch query (e.g. `tag:unread`). These are virtual folders
        // that do not exist on disk but are usable wherever a folder
        // name is accepted.
        if let Some(aliases) = ctx.account_config.get_folder_aliases() {
            for (name, value) in aliases {
                if is_raw_notmuch_query(value)
                    && !folders.iter().any(|f| f.name.eq_ignore_ascii_case(name))
                {
                    folders.push(Folder {
                        kind: None,
                        name: name.clone(),
                        desc: value.clone(),
                    });
                }
            }
        }

        Ok(folders)
    }
}
