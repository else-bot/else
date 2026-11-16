//! Keeps each saved server's rail icon in step with the icon the server
//! keeps for every desktop. The rail draws the local cache; a check
//! adopts the server's choice into it, and a choice made here goes to the
//! server.

use super::*;
use crate::server_icon::{self, ServerIcon, Sync};
use std::time::Instant;

pub(super) struct IconWrite {
    pub icon: ServerIcon,
    /// Offered to a server nobody gave an icon yet; a failure waits for
    /// the next check instead of being shown.
    pub carried: bool,
}

impl Shell {
    /// Follow the icon the server reports once no write or download for
    /// this server could still change it.
    pub(super) fn adopt_server_icon(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(observed) = self.servers.entries.get(&id) else {
            return;
        };
        if !self.icon_writes.settled(id, observed.asked) || self.icon_fetches.contains(&id) {
            return;
        }
        let Some(server) = &observed.server else {
            return;
        };
        let cached = self.rail.icons.get(id);
        match Sync::of(cached, &server.icon) {
            Sync::Keep => {}
            Sync::Adopt(icon) => {
                if let Err(err) = self.cache_server_icon(id, icon) {
                    self.connection_error = Some(err);
                }
            }
            Sync::Fetch(digest) => self.fetch_server_picture(id, digest, cx),
            Sync::Carry => {
                let icon = cached.clone();
                self.share_server_icon(
                    id,
                    IconWrite {
                        icon,
                        carried: true,
                    },
                    cx,
                );
            }
        }
    }

    /// Draw `icon` for the server on this desktop.
    pub(super) fn cache_server_icon(&mut self, id: Uuid, icon: ServerIcon) -> Result<(), String> {
        let store = self.launch.store.as_ref().map_err(Clone::clone)?;
        self.rail.icons.set(store, id, icon)
    }

    fn server_client(&self, id: Uuid) -> Option<(Profile, Result<Arc<Client>, String>)> {
        let profile = self.registry.find(id)?.profile.clone();
        let client = Client::new(profile.origin.clone(), &profile.token, self.runtime.clone());
        Some((profile, client.map(Arc::new)))
    }

    /// Save the icon on the server, which every desktop picks up on its
    /// next check. Writes to one server go one at a time, and only the
    /// latest waiting icon is sent next.
    pub(super) fn share_server_icon(&mut self, id: Uuid, write: IconWrite, cx: &mut Context<Self>) {
        let Some((_, client)) = self.server_client(id) else {
            return;
        };
        let client = match client {
            Ok(client) => client,
            Err(err) => {
                if !write.carried {
                    self.connection_error = Some(err);
                }
                return;
            }
        };
        let Some(write) = self.icon_writes.begin(id, write) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.set_server_icon(write.icon, write.carried).await;
            let _ = this.update(cx, |shell, cx| {
                let next = shell.icon_writes.finish(id, result.is_ok());
                match result {
                    Ok(()) => shell.servers.recheck(id),
                    // A server from before shared icons keeps the icon on this desktop only.
                    Err(ClientError::Api { status: 404, .. }) => {}
                    Err(err) if !write.carried => {
                        shell.connection_error =
                            Some(format!("Could not save the icon on the server. {err}"));
                    }
                    Err(_) => {}
                }
                if let Some(next) = next {
                    shell.share_server_icon(id, next, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Download the server's picture and draw it here, unless the icon
    /// changed meanwhile. A failure waits for the next check.
    fn fetch_server_picture(&mut self, id: Uuid, digest: String, cx: &mut Context<Self>) {
        let Some((profile, Ok(client))) = self.server_client(id) else {
            return;
        };
        self.icon_fetches.insert(id);
        let asked = Instant::now();
        cx.spawn(async move |this, cx| {
            let result = client.server_picture(digest).await;
            let _ = this.update(cx, |shell, cx| {
                shell.icon_fetches.remove(&id);
                let current = shell
                    .registry
                    .find(id)
                    .is_some_and(|saved| saved.profile == profile);
                let Ok(Some(png)) = result else {
                    return;
                };
                if !current || !shell.icon_writes.settled(id, Some(asked)) {
                    return;
                }
                if let Some(image) = server_icon::picture(png)
                    && let Err(err) = shell.cache_server_icon(id, ServerIcon::Picture(image))
                {
                    shell.connection_error = Some(err);
                }
                cx.notify();
            });
        })
        .detach();
    }
}
