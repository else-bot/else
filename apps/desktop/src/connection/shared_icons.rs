//! Keeps each saved server's rail icon in step with the icon the server
//! keeps for every desktop. The rail draws the local cache; a check
//! adopts the server's choice into it, and a choice made here goes to the
//! server.

use super::*;
use crate::server_icon::{self, ServerIcon, Shared, Stored, Sync};
use std::time::Instant;

pub(super) struct IconWrite {
    icon: ServerIcon,
    /// Offered to a server nobody gave an icon yet; a failure waits for
    /// the next check instead of being shown.
    carried: bool,
    /// The connection the icon was chosen for; a write still waiting when
    /// its address or token changes is dropped.
    profile: Profile,
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
            Sync::Carry => self.share_server_icon(id, cached.clone(), true, cx),
        }
    }

    /// Draw `icon` for the server on this desktop.
    pub(super) fn cache_server_icon(&mut self, id: Uuid, icon: ServerIcon) -> Result<(), String> {
        let store = self.launch.store.as_ref().map_err(Clone::clone)?;
        self.rail.icons.set(store, id, icon)
    }

    fn current_profile(&self, id: Uuid, profile: &Profile) -> bool {
        self.registry
            .find(id)
            .is_some_and(|saved| saved.profile == *profile)
    }

    /// Save the icon on the server, which every desktop picks up on its
    /// next check.
    pub(super) fn share_server_icon(
        &mut self,
        id: Uuid,
        icon: ServerIcon,
        carried: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.registry.find(id) else {
            return;
        };
        let profile = saved.profile.clone();
        self.send_icon_write(
            id,
            IconWrite {
                icon,
                carried,
                profile,
            },
            cx,
        );
    }

    /// Writes to one server go one at a time, and only the latest waiting
    /// icon is sent next.
    fn send_icon_write(&mut self, id: Uuid, write: IconWrite, cx: &mut Context<Self>) {
        let Some(write) = self.icon_writes.begin(id, write) else {
            return;
        };
        let client = Client::new(
            write.profile.origin.clone(),
            &write.profile.token,
            self.runtime.clone(),
        );
        let client = match client {
            Ok(client) if self.current_profile(id, &write.profile) => Arc::new(client),
            outdated_or_failed => {
                if let Err(err) = outdated_or_failed
                    && !write.carried
                {
                    self.connection_error = Some(err);
                }
                if let Some(next) = self.icon_writes.finish(id, false) {
                    self.send_icon_write(id, next, cx);
                }
                return;
            }
        };
        let carried = write.carried;
        cx.spawn(async move |this, cx| {
            let result = client.set_server_icon(write.icon, carried).await;
            let _ = this.update(cx, |shell, cx| {
                let next = shell.icon_writes.finish(id, result.is_ok());
                match result {
                    Ok(()) => shell.servers.recheck(id),
                    // A server from before shared icons keeps the icon on this desktop only.
                    Err(ClientError::Api { status: 404, .. }) => {}
                    Err(err) if !carried => {
                        shell.connection_error =
                            Some(format!("Could not save the icon on the server. {err}"));
                    }
                    Err(_) => {}
                }
                if let Some(next) = next {
                    shell.send_icon_write(id, next, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Download the server's picture and draw it here, unless the icon
    /// changed meanwhile. A failure waits for the next check.
    fn fetch_server_picture(&mut self, id: Uuid, digest: String, cx: &mut Context<Self>) {
        let Some(saved) = self.registry.find(id) else {
            return;
        };
        let profile = saved.profile.clone();
        let Ok(client) = Client::new(profile.origin.clone(), &profile.token, self.runtime.clone())
        else {
            return;
        };
        let client = Arc::new(client);
        self.icon_fetches.insert(id);
        let asked = Instant::now();
        cx.spawn(async move |this, cx| {
            let result = client.server_picture(digest.clone()).await;
            let _ = this.update(cx, |shell, cx| {
                shell.icon_fetches.remove(&id);
                let observed = shell
                    .servers
                    .entries
                    .get(&id)
                    .and_then(|observed| observed.server.as_ref());
                let latest = matches!(
                    observed.map(|server| &server.icon),
                    Some(Shared::Set(Stored::Picture { digest: Some(seen) })) if *seen == digest
                );
                if !latest {
                    // A check while downloading saw another icon.
                    shell.adopt_server_icon(id, cx);
                    return;
                }
                let fresh = shell.current_profile(id, &profile)
                    && shell.icon_writes.settled(id, Some(asked));
                if let (true, Ok(Some(png))) = (fresh, result)
                    && let Some(image) = server_icon::picture(png)
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
