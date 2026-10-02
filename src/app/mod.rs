pub mod cache;

use self::cache::UsersCache;
use crate::{
    config::Config,
    db::{self, delete_user_logs, writer::FlushBuffer},
    error::Error,
    kick::api::{is_valid_slug, normalize_slug, KickApi, MAX_LOOKUP_BATCH},
    Result,
};
use anyhow::Context;
use dashmap::DashSet;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct App {
    pub kick: Arc<KickApi>,
    pub users: UsersCache,
    pub optout_codes: Arc<DashSet<String>>,
    pub db: Arc<clickhouse::Client>,
    pub config: Arc<Config>,
    pub flush_buffer: FlushBuffer,
}

/// The result of looking up the channels which are going to be joined
pub struct ResolvedChannels {
    /// User id -> channel slug
    pub channels: HashMap<String, String>,
    /// How many requests to Kick failed. The channels they were about are missing from
    /// `channels` and should be tried again later.
    pub failed_lookups: usize,
}

/// Splits a list of ids and names. Kick user ids are numbers, anything else is a channel slug.
fn split_ids_and_names(entries: Vec<String>) -> (Vec<String>, Vec<String>) {
    entries
        .into_iter()
        .map(|entry| entry.trim().to_owned())
        .filter(|entry| !entry.is_empty())
        .partition(|entry| entry.parse::<u64>().is_ok())
}

impl App {
    /// Resolves a list of channels where each entry is either a Kick user id or a channel slug,
    /// which is how channels are written in the config and sent to the admin API.
    /// Returns a map of user id to channel slug.
    pub async fn get_channels_by_entries(
        &self,
        entries: Vec<String>,
        ignore_cache: bool,
    ) -> Result<HashMap<String, String>> {
        let (users, _) = self.resolve_entries(entries, ignore_cache, false).await?;

        Ok(users)
    }

    /// Like `get_channels_by_entries`, but meant for the background task which joins the
    /// configured channels: when a request to Kick fails, the channels which were not affected
    /// are still returned and the number of failures is reported.
    /// With hundreds of thousands of channels some of the many requests are going to fail.
    pub async fn get_channels_for_joining(
        &self,
        entries: Vec<String>,
        ignore_cache: bool,
    ) -> Result<ResolvedChannels> {
        let (channels, failed_lookups) = self.resolve_entries(entries, ignore_cache, true).await?;

        Ok(ResolvedChannels {
            channels,
            failed_lookups,
        })
    }

    async fn resolve_entries(
        &self,
        entries: Vec<String>,
        ignore_cache: bool,
        lenient: bool,
    ) -> Result<(HashMap<String, String>, usize)> {
        let (ids, names) = split_ids_and_names(entries);
        let (mut users, mut failed_lookups) = self
            .resolve_users(ids, names.clone(), ignore_cache, lenient)
            .await?;

        // Slugs have `-` where usernames have `_`, so names with an underscore which Kick does
        // not know are looked up with a hyphen instead
        let hyphenated: Vec<String> = {
            let found: HashSet<&str> = users.values().map(String::as_str).collect();
            names
                .iter()
                .map(|name| normalize_slug(name))
                .filter(|name| name.contains('_') && !found.contains(name.as_str()))
                .map(|name| name.replace('_', "-"))
                .collect()
        };
        if !hyphenated.is_empty() {
            let (more_users, more_failed_lookups) = self
                .resolve_users(vec![], hyphenated, ignore_cache, lenient)
                .await?;
            users.extend(more_users);
            failed_lookups += more_failed_lookups;
        }

        Ok((users, failed_lookups))
    }

    /// Resolves user ids and logins (channel slugs). Returns a map of user id to login.
    ///
    /// Kick is asked first. Users it does not know (for example deleted accounts) are looked up
    /// in the logged messages, so the logs stay usable for them.
    pub async fn get_users(
        &self,
        ids: Vec<String>,
        names: Vec<String>,
        ignore_cache: bool,
    ) -> Result<HashMap<String, String>> {
        let (users, _) = self.resolve_users(ids, names, ignore_cache, false).await?;

        Ok(users)
    }

    /// Returns the users and the number of failed requests, which is always 0 unless `lenient`
    async fn resolve_users(
        &self,
        ids: Vec<String>,
        names: Vec<String>,
        ignore_cache: bool,
        lenient: bool,
    ) -> Result<(HashMap<String, String>, usize)> {
        let mut users = HashMap::new();
        let mut ids_to_request = Vec::new();
        let mut names_to_request = Vec::new();

        // Slugs are lowercase
        let names: Vec<String> = names.iter().map(|name| normalize_slug(name)).collect();

        if ignore_cache {
            ids_to_request.clone_from(&ids);
            names_to_request.clone_from(&names);
        } else {
            for id in ids {
                match self.users.get_login(&id) {
                    Some(Some(login)) => {
                        users.insert(id, login);
                    }
                    Some(None) => (),
                    None => ids_to_request.push(id),
                }
            }

            for name in names {
                match self.users.get_id(&name) {
                    Some(Some(id)) => {
                        users.insert(id, name);
                    }
                    Some(None) => (),
                    None => names_to_request.push(name),
                }
            }
        }

        let mut new_channels = Vec::with_capacity(ids_to_request.len() + names_to_request.len());
        // Lookups which failed, nothing is known about them
        let mut failed_ids: HashSet<String> = HashSet::new();
        let mut failed_names: HashSet<String> = HashSet::new();

        // There are no chunks if the list is empty, so there is no empty request made
        for chunk in ids_to_request.chunks(MAX_LOOKUP_BATCH) {
            debug!("Requesting user info for {} ids", chunk.len());
            match self.kick.channels_by_user_ids(chunk).await {
                Ok(channels) => new_channels.extend(channels),
                Err(err) if lenient => {
                    warn!("Could not look up {} channels by id: {err}", chunk.len());
                    failed_ids.extend(chunk.iter().cloned());
                }
                Err(err) => return Err(err.into()),
            }
        }

        let valid_names: Vec<String> = names_to_request
            .iter()
            .filter(|name| is_valid_slug(name))
            .cloned()
            .collect();
        for chunk in valid_names.chunks(MAX_LOOKUP_BATCH) {
            debug!("Requesting user info for {} names", chunk.len());
            match self.kick.channels_by_slugs(chunk).await {
                Ok(channels) => new_channels.extend(channels),
                Err(err) if lenient => {
                    warn!("Could not look up {} channels by name: {err}", chunk.len());
                    failed_names.extend(chunk.iter().cloned());
                }
                Err(err) => return Err(err.into()),
            }
        }

        for channel in new_channels {
            self.users
                .insert(channel.user_id.clone(), channel.slug.clone());

            users.insert(channel.user_id, channel.slug);
        }

        // Users which Kick did not return, try to find them in the logs. Not when joining
        // channels: an account which does not exist anymore is not joined, like in the original.
        let missing_ids: Vec<String> = ids_to_request
            .iter()
            .filter(|id| !users.contains_key(id.as_str()) && !failed_ids.contains(id.as_str()))
            .cloned()
            .collect();
        if !lenient && !missing_ids.is_empty() {
            match db::get_user_logins(&self.db, &missing_ids).await {
                Ok(logins) => {
                    for (id, login) in logins {
                        // The login may belong to somebody else by now, so it is not
                        // remembered as the id of that login
                        self.users.remember_login(id.clone(), login.clone());
                        users.insert(id, login);
                    }
                }
                Err(err) => warn!("Could not look up user logins in the database: {err:?}"),
            }
        }

        // Users which could not be found at all
        for id in ids_to_request {
            if !users.contains_key(id.as_str()) && !failed_ids.contains(id.as_str()) {
                self.users.insert_optional(Some(id), None);
            }
        }
        {
            let found: HashSet<&str> = users.values().map(String::as_str).collect();
            for name in names_to_request {
                if !found.contains(name.as_str()) && !failed_names.contains(name.as_str()) {
                    self.users.insert_optional(None, Some(name));
                }
            }
        }

        Ok((users, failed_ids.len() + failed_names.len()))
    }

    pub async fn get_user_id_by_name(&self, name: &str) -> Result<String> {
        // Slugs have `-` where usernames have `_` ("Some_User" is kick.com/some-user),
        // so both spellings are tried
        let primary = normalize_slug(name);
        let hyphenated = primary.replace('_', "-");
        let mut candidates = vec![primary];
        if candidates[0] != hyphenated {
            candidates.push(hyphenated);
        }

        // The names which were not already known to be missing
        let mut looked_up = Vec::new();
        let mut api_error = None;

        for candidate in &candidates {
            match self.users.get_id(candidate) {
                Some(Some(id)) => return Ok(id),
                Some(None) => continue,
                None => (),
            }
            looked_up.push(candidate);

            if !is_valid_slug(candidate) {
                self.users.insert_optional(None, Some(candidate.clone()));
                continue;
            }

            match self
                .kick
                .channels_by_slugs(std::slice::from_ref(candidate))
                .await
            {
                Ok(channels) => {
                    match channels
                        .into_iter()
                        .find(|channel| channel.slug.eq_ignore_ascii_case(candidate))
                    {
                        Some(channel) => {
                            self.users
                                .insert(channel.user_id.clone(), channel.slug);
                            return Ok(channel.user_id);
                        }
                        None => self.users.insert_optional(None, Some(candidate.clone())),
                    }
                }
                Err(err) => {
                    warn!("Could not look up channel {candidate}: {err}");
                    api_error = Some(err);
                }
            }
        }

        // Kick does not know the name (or cannot be reached), but it may have been logged.
        // Names which were already known to be missing are not looked up again, the query
        // is not cheap.
        for candidate in looked_up {
            match db::get_user_id_by_login(&self.db, candidate).await {
                Ok(Some(id)) if !id.is_empty() => {
                    // Only remember the answer if Kick could be asked, otherwise a name which
                    // does not exist (anymore) would be given to an id for hours
                    if api_error.is_none() {
                        self.users.remember_id(candidate.clone(), id.clone());
                    }
                    return Ok(id);
                }
                Ok(_) => (),
                Err(err) => warn!("Could not look up {candidate} in the database: {err:?}"),
            }
        }

        match api_error {
            Some(err) => Err(err.into()),
            None => Err(Error::NotFound),
        }
    }

    pub async fn optout_user(&self, user_id: &str) -> anyhow::Result<()> {
        delete_user_logs(&self.db, user_id)
            .await
            .context("Could not delete logs")?;

        self.config.opt_out.insert(user_id.to_owned(), true);
        self.config.save()?;
        info!("User {user_id} opted out");

        Ok(())
    }

    pub fn check_opted_out(&self, channel_id: &str, user_id: Option<&str>) -> Result<()> {
        if self.config.opt_out.contains_key(channel_id) {
            return Err(Error::ChannelOptedOut);
        }

        if let Some(user_id) = user_id {
            if self.config.opt_out.contains_key(user_id) {
                return Err(Error::UserOptedOut);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::split_ids_and_names;
    use pretty_assertions::assert_eq;

    #[test]
    fn splits_ids_from_names() {
        let entries = vec![
            "676".to_owned(),
            "xqc".to_owned(),
            " 7183419 ".to_owned(),
            "".to_owned(),
            "some-user".to_owned(),
            "2fast".to_owned(),
            // all digits, but not a possible id
            "123456789012345678901234567890".to_owned(),
        ];

        let (ids, names) = split_ids_and_names(entries);

        assert_eq!(ids, vec!["676", "7183419"]);
        assert_eq!(
            names,
            vec!["xqc", "some-user", "2fast", "123456789012345678901234567890"]
        );
    }
}
